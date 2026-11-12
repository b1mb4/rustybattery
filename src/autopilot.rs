use crate::apps::AppDeltaUsage;
use crate::battery::{BatterySnapshot, BatteryStatus};
use crate::db::Database;
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, TargetKind,
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::Command;
use std::time::Instant;

const META_AUTOPILOT_ENABLED: &str = "autopilot_enabled";
const META_LAST_AUTOPILOT_ACTION: &str = "autopilot_last_action";
const IDLE_UNFOCUSED_FREEZE_SECS: u64 = 600; // 10 minutes unfocused

pub fn is_autopilot_enabled(db: &Database) -> bool {
    match db.get_meta(META_AUTOPILOT_ENABLED).as_deref() {
        Some("0") | Some("false") | Some("off") => false,
        _ => true, // Enabled by default
    }
}

pub fn set_autopilot_enabled(db: &Database, enabled: bool) -> Result<String> {
    db.set_meta(META_AUTOPILOT_ENABLED, if enabled { "1" } else { "0" })?;
    let msg = if enabled {
        "Smart Battery Saver Autopilot ENABLED (AC/Battery profile switch, 10m unfocused app freeze, low-battery rules)".to_string()
    } else {
        "Smart Battery Saver Autopilot DISABLED".to_string()
    };
    let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &msg);
    Ok(msg)
}

pub fn get_last_autopilot_action(db: &Database) -> String {
    db.get_meta(META_LAST_AUTOPILOT_ACTION).unwrap_or_else(|| {
        "Watching AC state, window focus & background drain".to_string()
    })
}

/// Stateful daemon engine for Automatic "Smart Battery Saver" Rules
pub struct SmartAutopilot {
    last_ac_online: Option<bool>,
    unfocused_since: HashMap<String, Instant>,
    auto_frozen_apps: HashSet<String>,
    low_bat_20_triggered: bool,
    low_bat_10_triggered: bool,
}

impl SmartAutopilot {
    pub fn new() -> Self {
        Self {
            last_ac_online: None,
            unfocused_since: HashMap::new(),
            auto_frozen_apps: HashSet::new(),
            low_bat_20_triggered: false,
            low_bat_10_triggered: false,
        }
    }

    /// Evaluates all Smart Battery Saver rules on each daemon tick.
    pub fn evaluate(
        &mut self,
        db: &Database,
        snap: &BatterySnapshot,
        deltas: &[AppDeltaUsage],
    ) {
        if !is_autopilot_enabled(db) {
            // If disabled while apps are auto-frozen, unfreeze them cleanly
            if !self.auto_frozen_apps.is_empty() {
                let apps: Vec<String> = self.auto_frozen_apps.drain().collect();
                for app in apps {
                    let _ = set_app_frozen(&app, false);
                }
            }
            return;
        }

        let now = Instant::now();
        let on_battery = !snap.ac_online && snap.status == BatteryStatus::Discharging;

        // ------------------------------------------------------------------
        // RULE 1: AC <-> Battery Power Profile & Unfreeze on AC Plug-in
        // ------------------------------------------------------------------
        if let Some(prev_ac) = self.last_ac_online {
            if prev_ac && !snap.ac_online {
                // Transitioned from AC -> Battery
                let profile_note = set_system_power_profile(true);
                let action = format!("Unplugged AC → switched to {}", profile_note);
                let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
            } else if !prev_ac && snap.ac_online {
                // Transitioned from Battery -> AC
                let profile_note = set_system_power_profile(false);
                let mut unfrozen_count = 0usize;
                let apps: Vec<String> = self.auto_frozen_apps.drain().collect();
                for app in apps {
                    if set_app_frozen(&app, false).is_ok() {
                        unfrozen_count += 1;
                    }
                }
                let action = if unfrozen_count > 0 {
                    format!(
                        "Plugged into AC → {} & resumed {} frozen app(s)",
                        profile_note, unfrozen_count
                    )
                } else {
                    format!("Plugged into AC → restored {}", profile_note)
                };
                let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
            }
        } else if on_battery {
            // Initial daemon startup on battery
            let _ = set_system_power_profile(true);
        }
        self.last_ac_online = Some(snap.ac_online);

        // Reset low-battery triggers when charging above thresholds
        if !on_battery || snap.capacity_pct > 22.0 {
            self.low_bat_20_triggered = false;
        }
        if !on_battery || snap.capacity_pct > 12.0 {
            self.low_bat_10_triggered = false;
        }

        // ------------------------------------------------------------------
        // RULE 2: Auto-Resume Focused Apps & Auto-Freeze Idle Heavy Background Apps
        // ------------------------------------------------------------------
        for d in deltas {
            // Never auto-freeze desktop shells, audio servers, or terminal emulators hosting interactive sessions
            if !is_eligible_for_autofreeze(&d.app_name, &d.category) {
                continue;
            }

            if d.is_focused {
                self.unfocused_since.remove(&d.app_name);
                // If this app was auto-frozen (or frozen in general) and user focuses its window, resume immediately!
                if self.auto_frozen_apps.remove(&d.app_name) || d.is_frozen {
                    if set_app_frozen(&d.app_name, false).is_ok() {
                        let action = format!("▶ Auto-resumed {} on window focus", d.app_name);
                        let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
                    }
                }
            } else if on_battery && !d.is_frozen {
                let entry = self.unfocused_since.entry(d.app_name.clone()).or_insert(now);
                let unfocused_dur = now.duration_since(*entry).as_secs();

                // If unfocused for >= 10 minutes while still drawing >= 0.85W or >= 3.5% CPU on battery
                if unfocused_dur >= IDLE_UNFOCUSED_FREEZE_SECS
                    && (d.live_power_w >= 0.85 || d.live_cpu_pct >= 3.5)
                {
                    if set_app_frozen(&d.app_name, true).is_ok() {
                        self.auto_frozen_apps.insert(d.app_name.clone());
                        self.unfocused_since.remove(&d.app_name);
                        let action = format!(
                            "❄️ Auto-froze unfocused {} (idle {}m, saving {:.1}W)",
                            d.app_name,
                            unfocused_dur / 60,
                            d.live_power_w
                        );
                        let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
                        send_autopilot_notification(
                            "❄️ Smart Battery Saver: App Frozen",
                            &format!(
                                "{} was unfocused for {}m while drawing {:.1}W. Frozen to save battery (focus window to resume).",
                                d.app_name,
                                unfocused_dur / 60,
                                d.live_power_w
                            ),
                        );
                    }
                }
            }
        }

        // ------------------------------------------------------------------
        // RULE 3: Low-Battery Threshold Triggers (20% and 10%)
        // ------------------------------------------------------------------
        if on_battery && snap.capacity_pct <= 20.0 && !self.low_bat_20_triggered {
            self.low_bat_20_triggered = true;
            let _ = set_system_power_profile(true);
            let kbd_dimmed = dim_keyboard_backlight();
            let action = if kbd_dimmed {
                format!(
                    "🔋 Low Battery ({:.0}%) → Enforced power-saver & dimmed keyboard backlight",
                    snap.capacity_pct
                )
            } else {
                format!(
                    "🔋 Low Battery ({:.0}%) → Enforced power-saver profile",
                    snap.capacity_pct
                )
            };
            let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
        }

        if on_battery && snap.capacity_pct <= 10.0 && !self.low_bat_10_triggered {
            self.low_bat_10_triggered = true;
            // Auto-stop safe user background services (indexers, store updaters)
            let mut targets = scan_optimization_targets(snap.power_w);
            for t in &mut targets {
                t.selected = t.kind == TargetKind::UserUnit;
            }
            let res = execute_optimizations(&targets);
            if !res.actions_taken.is_empty() {
                let action = format!(
                    "🚨 Critical Battery ({:.0}%) → Auto-stopped {} background service(s) (~{:.1}W saved)",
                    snap.capacity_pct,
                    res.actions_taken.len(),
                    res.estimated_watts_saved
                );
                let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, &action);
            }
        }
    }
}

fn is_eligible_for_autofreeze(app_name: &str, category: &str) -> bool {
    let lower = app_name.to_lowercase();
    if lower.contains("gnome-shell")
        || lower.contains("wayland")
        || lower.contains("xorg")
        || lower.contains("pipewire")
        || lower.contains("wireplumber")
        || lower.contains("pulseaudio")
        || lower.contains("rustybattery")
        || lower.contains("systemd")
        || lower.contains("ptyxis")
        || lower.contains("terminal")
        || lower.contains("alacritty")
        || lower.contains("kitty")
        || lower.contains("wezterm")
    {
        return false;
    }

    matches!(
        category,
        "Browser" | "Productivity" | "Social & Chat" | "Audio & Media" | "App"
    )
}

fn set_system_power_profile(powersave: bool) -> String {
    let target_ppd = if powersave { "power-saver" } else { "balanced" };
    if let Ok(status) = Command::new("powerprofilesctl")
        .args(["set", target_ppd])
        .status()
    {
        if status.success() {
            return format!("powerprofilesctl={}", target_ppd);
        }
    }

    let target_tuned = if powersave { "powersave" } else { "balanced" };
    if let Ok(status) = Command::new("tuned-adm")
        .args(["profile", target_tuned])
        .status()
    {
        if status.success() {
            return format!("tuned={}", target_tuned);
        }
    }

    if powersave {
        "powersave mode".to_string()
    } else {
        "balanced mode".to_string()
    }
}

fn dim_keyboard_backlight() -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/leds") else {
        return false;
    };
    let mut dimmed = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("kbd_backlight") {
            let b_path = entry.path().join("brightness");
            if fs::write(&b_path, "0").is_ok() {
                dimmed = true;
            }
        }
    }
    dimmed
}

fn send_autopilot_notification(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args([
            "-a",
            "RustyBattery Smart Saver",
            "-u",
            "normal",
            "-i",
            "battery-good-symbolic",
            summary,
            body,
        ])
        .status();
}
