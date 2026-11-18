use crate::apps::AppEnergyTracker;
use crate::autopilot::SmartAutopilot;
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
use crate::statusbar::NotificationMonitor;
use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

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
    if let Some(hb_str) = db.get_meta("daemon_heartbeat_ts") {
        if let Ok(hb) = hb_str.parse::<i64>() {
            if (now - hb).abs() <= 20 {
                return true;
            }
        }
    }
    false
}

pub fn is_service_file_installed() -> bool {
    systemd_unit_path().exists()
}

/// Runs the continuous background collector daemon.
pub fn run_daemon(interval_secs: u64) -> Result<()> {
    let db_path = default_db_path();
    let db = Database::open(&db_path)?;

    if let Ok(initial_bat) = read_battery_snapshot() {
        let _ = db.ensure_initial_seed(&initial_bat);
        let _ = db.record_battery_snapshot(&initial_bat);
    }

    spawn_logind_sleep_monitor(db_path.clone());

    let mut tracker = AppEnergyTracker::new();
    let mut notif_monitor = NotificationMonitor::new(22.0);
    let mut autopilot = SmartAutopilot::new();
    let sleep_dur = Duration::from_secs(interval_secs.max(1));
    let mut prev_snap = read_battery_snapshot().ok();

    println!(
        "rustybattery daemon started (sampling every {}s -> {})",
        sleep_dur.as_secs(),
        db_path.display()
    );

    loop {
        thread::sleep(sleep_dur);

        let now = chrono::Utc::now().timestamp();
        let _ = db.set_meta("daemon_heartbeat_ts", &now.to_string());
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
            let _ = db.record_battery_snapshot(&snap);
            let (deltas, _) = tracker.sample_delta(
                snap.power_w,
                snap.energy_full_wh,
                snap.status.is_discharging(),
            );
            let _ = db.record_app_deltas(&deltas);
            notif_monitor.evaluate(&snap, &deltas);
            autopilot.evaluate(&db, &snap, &deltas);
            prev_snap = Some(snap);
        }
    }
}

/// Listens to `systemd-logind` `PrepareForSleep(bool)` via `gdbus monitor` so pre-sleep actions
/// (blocking Bluetooth, enforcing `power-saver`, dimming keyboard) run before the kernel suspends.
fn spawn_logind_sleep_monitor(db_path: PathBuf) {
    thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

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
            return;
        };
        let Ok(db) = Database::open(&db_path) else {
            return;
        };
        let mut pre_sleep_snap: Option< crate::battery::BatterySnapshot> = None;

        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("PrepareForSleep (true,)") || line.contains("PrepareForSleep (true)") {
                if let Ok(snap) = read_battery_snapshot() {
                    let on_bat = !snap.ac_online && snap.status.is_discharging();
                    crate::autopilot::handle_sleep_transition(&db, true, on_bat);
                    let _ = db.record_battery_snapshot(&snap);
                    pre_sleep_snap = Some(snap);
                }
            } else if line.contains("PrepareForSleep (false,)")
                || line.contains("PrepareForSleep (false)")
            {
                if let Ok(snap) = read_battery_snapshot() {
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
    });
}

pub fn install_systemd_service(interval_secs: u64) -> Result<String> {
    let current_exe = env::current_exe().context("Failed to locate current rustybattery binary")?;

    // Copy binary to ~/.local/bin/rustybattery so it stays in a standard user PATH location
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
            let _ = std::os::unix::fs::symlink(&target_bin, &battery_alias_bin);
            target_bin
        } else {
            current_exe
        }
    } else {
        let _ = fs::remove_file(&battery_alias_bin);
        let _ = std::os::unix::fs::symlink(&target_bin, &battery_alias_bin);
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
ExecStart={} daemon --interval {}
Restart=always
RestartSec=5
Nice=10

[Install]
WantedBy=default.target
",
        exe_to_use.display(),
        interval_secs.max(2)
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
