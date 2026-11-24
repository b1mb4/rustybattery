use crate::apps::AppEnergyTracker;
use crate::autopilot::{thaw_persisted_frozen_apps, SmartAutopilot};
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
use crate::optimize::thaw_owned_user_cgroups;
use crate::statusbar::{write_cached_status, NotificationMonitor};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

pub fn install_signal_handlers() {
    let handler = handle_shutdown_signal as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

/// Unbiased rolling-window power estimator (`F-4`) that avoids upward bias on coarse
/// (e.g. 10 mWh step) battery fuel gauges by computing `(e_oldest - e_newest) * 3600 / dt`
/// over a ~60–90 s window and applying the blend uniformly across ticks.
#[derive(Debug, Clone, Default)]
pub struct RollingEnergyEstimator {
    window: VecDeque<(i64, f64)>,
    min_step_wh: Option<f64>,
    smoothed_w: Option<f64>,
}

impl RollingEnergyEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.min_step_wh = None;
        self.smoothed_w = None;
    }

    pub fn observe(&mut self, ts: i64, energy_now_wh: f64, power_now_w: f64, discharging: bool) -> f64 {
        if !discharging {
            self.window.clear();
            self.smoothed_w = None;
            return power_now_w;
        }

        if let Some(&(_, prev_wh)) = self.window.back() {
            if energy_now_wh > prev_wh + 0.05 {
                // Battery was charged
                self.window.clear();
            } else {
                let step = (prev_wh - energy_now_wh).abs();
                if step > 1e-5 {
                    self.min_step_wh = Some(match self.min_step_wh {
                        Some(cur) => cur.min(step),
                        None => step,
                    });
                }
            }
        }

        self.window.push_back((ts, energy_now_wh));
        while self.window.len() > 2 {
            if let Some(&(oldest_ts, _)) = self.window.front() {
                if ts - oldest_ts > 90 {
                    self.window.pop_front();
                } else {
                    break;
                }
            }
        }

        let mut target_w = power_now_w;
        if let (Some(&(t_old, e_old)), Some(&(t_new, e_new))) =
            (self.window.front(), self.window.back())
        {
            let dt = t_new - t_old;
            let de = (e_old - e_new).max(0.0);
            let gauge_step = self.min_step_wh.unwrap_or(0.01);
            if dt >= 60 && de >= 3.0 * gauge_step {
                let p_window = (de * 3600.0) / (dt as f64);
                if p_window > 0.3 && p_window < 120.0 {
                    target_w = if power_now_w > 0.1 {
                        0.5 * power_now_w + 0.5 * p_window
                    } else {
                        p_window
                    };
                }
            }
        }

        let out = match self.smoothed_w {
            Some(prev) => 0.7 * target_w + 0.3 * prev,
            None => target_w,
        };
        self.smoothed_w = Some(out);
        out
    }
}

pub fn systemd_unit_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".config")
        })
        .join("systemd")
        .join("user");
    let _ = fs::create_dir_all(&config_dir);
    config_dir.join("rustybattery.service")
}

pub fn is_daemon_active(db: &Database) -> bool {
    let now = chrono::Utc::now().timestamp();
    let interval = db
        .get_meta("daemon_interval_secs")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(5);
    let max_age = (interval * 3 + 20).max(35);
    if crate::statusbar::read_cached_status(max_age).is_some() {
        return true;
    }
    if let Some(hb_str) = db.get_meta("daemon_heartbeat_ts") {
        if let Ok(hb) = hb_str.parse::<i64>() {
            if (now - hb).abs() <= max_age {
                return true;
            }
        }
    }
    false
}

pub fn is_service_file_installed() -> bool {
    systemd_unit_path().exists()
}

fn interruptible_sleep(total: Duration) {
    let step = Duration::from_millis(250);
    let mut elapsed = Duration::ZERO;
    while elapsed < total && !STOP_REQUESTED.load(Ordering::SeqCst) {
        let rem = total.saturating_sub(elapsed).min(step);
        thread::sleep(rem);
        elapsed += rem;
    }
}

/// Runs the continuous background collector daemon.
pub fn run_daemon(interval_secs: u64) -> Result<()> {
    install_signal_handlers();
    let db_path = default_db_path();
    let db = Database::open(&db_path)?;
    let _ = thaw_persisted_frozen_apps(&db);
    let _ = thaw_owned_user_cgroups();

    let effective_interval = interval_secs.clamp(2, 60);
    let _ = db.set_meta("daemon_interval_secs", &effective_interval.to_string());

    let high_drain_threshold_w = if let Ok(initial_bat) = read_battery_snapshot() {
        let _ = db.ensure_initial_seed(&initial_bat);
        let _ = db.record_battery_snapshot(&initial_bat);
        (initial_bat.energy_full_wh * 0.36).clamp(18.0, 35.0)
    } else {
        22.0
    };

    spawn_logind_sleep_monitor(db_path.clone());

    let mut tracker = AppEnergyTracker::with_interval(effective_interval);
    let mut notif_monitor = NotificationMonitor::new(high_drain_threshold_w);
    let mut autopilot = SmartAutopilot::new();
    let mut energy_estimator = RollingEnergyEstimator::new();
    let sleep_dur = Duration::from_secs(effective_interval);
    let mut prev_snap = read_battery_snapshot().ok();
    let mut last_hb_write_ts: i64 = 0;

    println!(
        "rustybattery daemon started (sampling every {}s -> {})",
        sleep_dur.as_secs(),
        db_path.display()
    );

    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        interruptible_sleep(sleep_dur);
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            break;
        }

        let now = chrono::Utc::now().timestamp();
        if now - last_hb_write_ts >= 15
            && db.set_meta("daemon_heartbeat_ts", &now.to_string()).is_ok()
        {
            last_hb_write_ts = now;
        }
        let _ = db.prune_if_due(now);

        if let Ok(snap) = read_battery_snapshot() {
            if let Some(prev) = &prev_snap {
                let slept = snap.suspended_total_secs - prev.suspended_total_secs;
                if snap.boot_id == prev.boot_id && slept >= 30.0 {
                    let _ = db.record_sleep_session(&crate::db::SleepSession {
                        start_ts: prev.timestamp,
                        end_ts: snap.timestamp,
                        start_pct: prev.capacity_pct,
                        end_pct: snap.capacity_pct,
                        start_wh: prev.energy_now_wh,
                        end_wh: snap.energy_now_wh,
                        on_battery: prev.status.is_discharging() || snap.status.is_discharging(),
                    });
                }
            }
            let power_for_tick = energy_estimator.observe(
                snap.timestamp,
                snap.energy_now_wh,
                snap.power_w,
                snap.status.is_discharging(),
            );

            let _ = db.record_battery_snapshot(&snap);
            let (deltas, _) = tracker.sample_delta(
                power_for_tick,
                snap.energy_full_wh,
                snap.status.is_discharging(),
            );
            let _ = db.record_app_deltas(&deltas);
            let _ = write_cached_status(&snap, &deltas);
            notif_monitor.evaluate(&snap, &deltas);
            notif_monitor.evaluate_budgets(&db, &snap);
            autopilot.evaluate(&db, &snap, &deltas);
            prev_snap = Some(snap);
        }
    }

    autopilot.thaw_all(&db);
    let _ = thaw_owned_user_cgroups();
    Ok(())
}

/// Listens to `systemd-logind` `PrepareForSleep(bool)` via `gdbus monitor` so pre-sleep actions
/// (blocking Bluetooth, enforcing `power-saver`, dimming keyboard) run before the kernel suspends.
fn spawn_logind_sleep_monitor(db_path: PathBuf) {
    thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            let Ok(mut child) = Command::new("gdbus")
                .args([
                    "monitor",
                    "--system",
                    "--dest",
                    "org.freedesktop.login1",
                    "--object-path",
                    "/org/freedesktop/login1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };

            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return;
            };
            let mut pre_sleep_snap: Option<crate::battery::BatterySnapshot> = None;

            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if STOP_REQUESTED.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    break;
                }
                if line.contains("PrepareForSleep (true,)") || line.contains("PrepareForSleep (true)") {
                    if let (Ok(snap), Ok(db)) = (read_battery_snapshot(), Database::open(&db_path)) {
                        let on_bat = !snap.ac_online && snap.status.is_discharging();
                        crate::autopilot::handle_sleep_transition(&db, true, on_bat);
                        let _ = db.record_battery_snapshot(&snap);
                        pre_sleep_snap = Some(snap);
                    }
                } else if line.contains("PrepareForSleep (false,)")
                    || line.contains("PrepareForSleep (false)")
                {
                    if let (Ok(snap), Ok(db)) = (read_battery_snapshot(), Database::open(&db_path)) {
                        let on_bat = !snap.ac_online && snap.status.is_discharging();
                        crate::autopilot::handle_sleep_transition(&db, false, on_bat);
                        let _ = db.record_battery_snapshot(&snap);
                        if let Some(start) = pre_sleep_snap.take() {
                            if snap.timestamp - start.timestamp >= 30 {
                                let _ = db.record_sleep_session(&crate::db::SleepSession {
                                    start_ts: start.timestamp,
                                    end_ts: snap.timestamp,
                                    start_pct: start.capacity_pct,
                                    end_pct: snap.capacity_pct,
                                    start_wh: start.energy_now_wh,
                                    end_wh: snap.energy_now_wh,
                                    on_battery: start.status.is_discharging() || on_bat,
                                });
                            }
                        }
                    }
                }
            }
            let _ = child.wait();
            interruptible_sleep(Duration::from_secs(5));
        }
    });
}

pub fn install_systemd_service(interval_secs: u64) -> Result<String> {
    let current_exe = env::current_exe().context("Failed to locate current rustybattery binary")?;

    let local_bin_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin");
    let _ = fs::create_dir_all(&local_bin_dir);
    let target_bin = local_bin_dir.join("rustybattery");
    let battery_alias_bin = local_bin_dir.join("battery");

    let exe_to_use = if current_exe != target_bin {
        let _ = fs::remove_file(&target_bin);
        if fs::copy(&current_exe, &target_bin).is_ok() {
            let _ = fs::remove_file(&battery_alias_bin);
            let _ = std::os::unix::fs::symlink(&target_bin, battery_alias_bin);
            target_bin
        } else {
            current_exe
        }
    } else {
        let _ = fs::remove_file(&battery_alias_bin);
        let _ = std::os::unix::fs::symlink(target_bin, battery_alias_bin);
        current_exe
    };

    let unit_path = systemd_unit_path();
    let unit_content = format!(
        "[Unit]
Description=RustyBattery - Phone-Style Laptop Battery & Per-App Energy Tracker
Documentation=https://github.com/b1mb4/rustybattery
After=default.target

[Service]
Type=simple
ExecStart={exe} daemon --interval {interval}
ExecStopPost={exe} resume --all
Restart=always
RestartSec=5
TimeoutStopSec=15
Nice=10
NoNewPrivileges=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryMax=200M

[Install]
WantedBy=default.target
",
        exe = exe_to_use.display(),
        interval = interval_secs.clamp(2, 60)
    );

    fs::write(&unit_path, unit_content)
        .with_context(|| format!("Failed to write {}", unit_path.display()))?;

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", "rustybattery.service"])
        .output();

    match (reload, enable) {
        (Ok(r), Ok(e)) if r.status.success() && e.status.success() => Ok(format!(
            "Installed & started systemd user service: {}\nBinary installed at: {}",
            unit_path.display(),
            exe_to_use.display()
        )),
        _ => Ok(format!(
            "Wrote service file to: {}\nBinary installed at: {}\nTo enable & start in your terminal, run:\n  systemctl --user daemon-reload && systemctl --user enable --now rustybattery.service",
            unit_path.display(),
            exe_to_use.display()
        )),
    }
}

pub fn uninstall_systemd_service() -> Result<String> {
    let _ = thaw_owned_user_cgroups();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "rustybattery.service"])
        .output();

    let unit_path = systemd_unit_path();
    if unit_path.exists() {
        fs::remove_file(&unit_path)?;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    Ok(format!(
        "Stopped and removed systemd user service ({})",
        unit_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_energy_estimator_is_unbiased_on_10mwh_quantized_gauge() {
        // Probe F-4: 3.0 W true power with a 10 mWh (0.010 Wh) quantized fuel gauge sampled every 5s
        let mut est = RollingEnergyEstimator::new();
        let true_power_w = 3.0;
        let mut sum_w = 0.0;
        let ticks = 120;
        for i in 0..ticks {
            let ts = i * 5;
            let exact_wh = 50.0 - (true_power_w * (ts as f64) / 3600.0);
            // Quantize to 10 mWh (0.01 Wh) steps
            let quantized_wh = (exact_wh * 100.0).floor() / 100.0;
            let w = est.observe(ts, quantized_wh, true_power_w, true);
            sum_w += w;
        }
        let mean_w = sum_w / (ticks as f64);
        assert!(
            (mean_w - true_power_w).abs() <= true_power_w * 0.03,
            "mean_w={mean_w} deviated by >3% from {true_power_w}"
        );
    }
}
