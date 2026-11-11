use crate::apps::{AppDeltaUsage, AppEnergyTracker};
use crate::battery::{read_battery_snapshot, BatterySnapshot, BatteryStatus};
use crate::db::{default_db_path, AppSummaryRow, Database};
use anyhow::Result;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub fn print_statusbar_config(bar_type: &str) {
    match bar_type.to_lowercase().as_str() {
        "polybar" => {
            println!(
                r#"# Add to ~/.config/polybar/config.ini:
[module/rustybattery]
type = custom/script
exec = battery statusbar --format polybar
interval = 5
click-left = ghostty -e battery &
click-middle = battery optimize &"#
            );
        }
        "tmux" => {
            println!(
                r##"# Add to ~/.tmux.conf:
set -g status-interval 5
set -g status-right "#(battery statusbar --format tmux) | %H:%M""##
            );
        }
        _ => {
            println!(
                r#"// Add to ~/.config/waybar/config.jsonc:
"custom/rustybattery": {{
    "exec": "battery statusbar --format waybar",
    "return-type": "json",
    "interval": 5,
    "format": "{{}}",
    "on-click": "xdg-terminal-exec battery || ghostty -e battery || kitty -e battery || gnome-terminal -- battery",
    "on-click-middle": "battery optimize",
    "tooltip": true
}}"#
            );
        }
    }
}

pub fn run_statusbar(format: &str, top_n: usize, config_snippet: Option<&str>) -> Result<()> {
    if let Some(cfg) = config_snippet {
        print_statusbar_config(cfg);
        return Ok(());
    }

    let db = Database::open(&default_db_path())?;
    let bat = read_battery_snapshot()?;
    let _ = db.ensure_initial_seed(&bat);

    let mut tracker = AppEnergyTracker::new();
    thread::sleep(Duration::from_millis(200));
    let (live_deltas, _) =
        tracker.sample_delta(bat.power_w, bat.energy_full_wh, bat.status.is_discharging());

    let since_ts = db.get_last_full_charge_ts();
    let mut apps = db.get_app_breakdown(since_ts, &live_deltas).unwrap_or_default();
    apps.sort_by(|a, b| {
        b.battery_pct
            .partial_cmp(&a.battery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let icon = match bat.status {
        BatteryStatus::Charging => "⚡",
        BatteryStatus::Full => "🔌",
        BatteryStatus::NotCharging => "🔌",
        BatteryStatus::Discharging => {
            if bat.capacity_pct <= 15.0 {
                "🪫"
            } else {
                "🔋"
            }
        }
        BatteryStatus::Unknown => "🔋",
    };

    let time_rem = match bat.time_remaining_mins {
        Some(m) => format!("~{}h {:02}m", m / 60, m % 60),
        None => "calculating".to_string(),
    };

    match format.to_lowercase().as_str() {
        "polybar" => {
            let top_app = apps
                .first()
                .map(|a| format!(" • {}", a.app_name))
                .unwrap_or_default();
            println!(
                "{} {:.0}% ({:.1}W{})",
                icon, bat.capacity_pct, bat.power_w, top_app
            );
        }
        "tmux" | "plain" => {
            println!(
                "{} {:.0}% {:.1}W ({})",
                icon, bat.capacity_pct, bat.power_w, time_rem
            );
        }
        _ => {
            // Waybar JSON format
            let class = match bat.status {
                BatteryStatus::Charging => "charging",
                BatteryStatus::Full | BatteryStatus::NotCharging => "full",
                BatteryStatus::Discharging => {
                    if bat.capacity_pct <= 15.0 {
                        "critical"
                    } else if bat.capacity_pct <= 30.0 {
                        "warning"
                    } else if bat.power_w >= 22.0 {
                        "high-drain"
                    } else {
                        "discharging"
                    }
                }
                BatteryStatus::Unknown => "unknown",
            };

            let mut tooltip_lines = vec![
                format!(
                    "🦀 RustyBattery — {:.1}% ({})",
                    bat.capacity_pct,
                    bat.status.as_str()
                ),
                format!(
                    "⚡ Power: {:.2} W  •  ⏳ {}",
                    bat.power_w, time_rem
                ),
                format!(
                    "🔋 Energy: {:.1} / {:.1} Wh  (Health: {:.1}%)",
                    bat.energy_now_wh, bat.energy_full_wh, bat.health_pct
                ),
                String::new(),
                "🔥 Top Battery Consumers:".to_string(),
            ];

            for (i, app) in apps.iter().take(top_n.max(1)).enumerate() {
                tooltip_lines.push(format!(
                    " {}. {} — {:.1}% ({:.2} Wh · {:.1} W live)",
                    i + 1,
                    app.app_name,
                    app.battery_pct,
                    app.energy_wh,
                    app.live_power_w
                ));
            }

            tooltip_lines.push(String::new());
            tooltip_lines.push("💡 Left-click: Open RustyBattery TUI".to_string());
            tooltip_lines.push("⚡ Middle-click: Run Battery Optimizer".to_string());

            let payload = serde_json::json!({
                "text": format!("{} {:.0}% ({:.1}W)", icon, bat.capacity_pct, bat.power_w),
                "alt": class,
                "tooltip": tooltip_lines.join("\n"),
                "class": class,
                "percentage": bat.capacity_int
            });
            println!("{}", serde_json::to_string(&payload)?);
        }
    }

    Ok(())
}

/// Sends a native desktop notification via `notify-send` (with `gdbus` fallback).
pub fn send_desktop_notification(
    summary: &str,
    body: &str,
    urgency: &str,
    icon: &str,
) -> Result<()> {
    let status = Command::new("notify-send")
        .args([
            "-a",
            "RustyBattery",
            "-u",
            urgency,
            "-i",
            icon,
            summary,
            body,
        ])
        .status();

    if let Ok(st) = status {
        if st.success() {
            return Ok(());
        }
    }

    // Fallback to gdbus org.freedesktop.Notifications
    let _ = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "RustyBattery",
            "0",
            icon,
            summary,
            body,
            "[]",
            "{}",
            "5000",
        ])
        .output();

    Ok(())
}

/// Sends an immediate status / alert notification summarizing battery health and top consumer.
pub fn send_status_notification(bat: &BatterySnapshot, top_apps: &[AppSummaryRow]) -> Result<()> {
    let time_rem = match bat.time_remaining_mins {
        Some(m) => format!("~{}h {:02}m left", m / 60, m % 60),
        None => bat.status.as_str().to_string(),
    };

    let summary = format!(
        "🦀 Battery {:.0}% ({:.1} W) — {}",
        bat.capacity_pct, bat.power_w, time_rem
    );

    let mut lines = Vec::new();
    if !top_apps.is_empty() {
        lines.push("Top draining apps:".to_string());
        for (i, a) in top_apps.iter().take(3).enumerate() {
            lines.push(format!(
                "{}. {} ({:.1}% · {:.1}W live)",
                i + 1,
                a.app_name,
                a.battery_pct,
                a.live_power_w
            ));
        }
    }
    lines.push("Run `battery optimize` to stop unused hogs.".to_string());

    let urgency = if bat.status.is_discharging() && bat.capacity_pct <= 15.0 {
        "critical"
    } else {
        "normal"
    };

    send_desktop_notification(&summary, &lines.join("\n"), urgency, "battery-caution")
}

/// Stateful notification monitor used by `rustybattery daemon` to alert the user on
/// sustained high power drain or low battery thresholds without spamming.
pub struct NotificationMonitor {
    high_drain_watts: f64,
    consecutive_high_drain_ticks: u32,
    last_high_drain_alert: Option<Instant>,
    alerted_20_pct: bool,
    alerted_10_pct: bool,
}

impl NotificationMonitor {
    pub fn new(high_drain_watts: f64) -> Self {
        Self {
            high_drain_watts,
            consecutive_high_drain_ticks: 0,
            last_high_drain_alert: None,
            alerted_20_pct: false,
            alerted_10_pct: false,
        }
    }

    pub fn evaluate(&mut self, bat: &BatterySnapshot, deltas: &[AppDeltaUsage]) {
        if !bat.status.is_discharging() {
            self.consecutive_high_drain_ticks = 0;
            if bat.capacity_pct > 25.0 {
                self.alerted_20_pct = false;
                self.alerted_10_pct = false;
            }
            return;
        }

        let top_live = deltas.first();
        let top_desc = top_live
            .map(|a| format!("Top consumer: {} ({:.1} W)", a.app_name, a.live_power_w))
            .unwrap_or_else(|| "Run `battery optimize` to reduce power draw.".to_string());

        // 1. Check Low Battery Thresholds (10% and 20%)
        if bat.capacity_pct <= 10.0 && !self.alerted_10_pct {
            self.alerted_10_pct = true;
            self.alerted_20_pct = true;
            let _ = send_desktop_notification(
                &format!("🪫 Critical Battery: {:.0}% ({:.1} W)", bat.capacity_pct, bat.power_w),
                &format!("{top_desc}\nRun `battery optimize` now to extend battery life."),
                "critical",
                "battery-empty",
            );
            return;
        } else if bat.capacity_pct <= 20.0 && !self.alerted_20_pct {
            self.alerted_20_pct = true;
            let _ = send_desktop_notification(
                &format!("⚠️ Low Battery: {:.0}% ({:.1} W)", bat.capacity_pct, bat.power_w),
                &format!("{top_desc}\nTip: Press 'o' in `battery` or run `battery optimize`."),
                "normal",
                "battery-low",
            );
            return;
        }

        // 2. Check Sustained High Power Drain (3 consecutive ticks >= high_drain_watts)
        if bat.power_w >= self.high_drain_watts {
            self.consecutive_high_drain_ticks += 1;
            let cooldown_ok = self
                .last_high_drain_alert
                .map(|t| t.elapsed() >= Duration::from_secs(900))
                .unwrap_or(true);

            if self.consecutive_high_drain_ticks >= 3 && cooldown_ok {
                self.last_high_drain_alert = Some(Instant::now());
                self.consecutive_high_drain_ticks = 0;
                let _ = send_desktop_notification(
                    &format!("🔥 High Battery Drain Detected ({:.1} W)", bat.power_w),
                    &format!("{top_desc}\nRun `battery optimize` to stop background hogs."),
                    "normal",
                    "battery-caution",
                );
            }
        } else {
            self.consecutive_high_drain_ticks = 0;
        }
    }
}
