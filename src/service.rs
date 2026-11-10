use crate::apps::AppEnergyTracker;
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
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

    let mut tracker = AppEnergyTracker::new();
    let sleep_dur = Duration::from_secs(interval_secs.max(1));

    println!(
        "rustybattery daemon started (sampling every {}s -> {})",
        sleep_dur.as_secs(),
        db_path.display()
    );

    loop {
        thread::sleep(sleep_dur);

        let now = chrono::Utc::now().timestamp();
        let _ = db.set_meta("daemon_heartbeat_ts", &now.to_string());

        if let Ok(snap) = read_battery_snapshot() {
            let _ = db.record_battery_snapshot(&snap);
            let (deltas, _) = tracker.sample_delta(
                snap.power_w,
                snap.energy_full_wh,
                snap.status.is_discharging(),
            );
            let _ = db.record_app_deltas(&deltas);
        }
    }
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

    let exe_to_use = if current_exe != target_bin {
        if fs::copy(&current_exe, &target_bin).is_ok() {
            target_bin
        } else {
            current_exe
        }
    } else {
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
