use crate::apps::AppDeltaUsage;
use crate::battery::{BatterySnapshot, BatteryStatus};
use crate::db::Database;
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, TargetKind,
};
use anyhow::{Context, Result};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const META_AUTOPILOT_ENABLED: &str = "autopilot_enabled";
const META_LAST_AUTOPILOT_ACTION: &str = "autopilot_last_action";
const META_AUTOPILOT_LOG: &str = "autopilot_action_log";

#[derive(Debug, Clone)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub auto_switch_power_profile: bool,
    pub auto_freeze_enabled: bool,
    pub idle_freeze_mins: u64,
    pub idle_power_threshold_w: f64,
    pub low_battery_pct: f64,
    pub critical_battery_pct: f64,
    pub dim_kbd_on_low_battery: bool,
    pub whitelist: Vec<String>,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_switch_power_profile: true,
            auto_freeze_enabled: true,
            idle_freeze_mins: 10,
            idle_power_threshold_w: 0.85,
            low_battery_pct: 20.0,
            critical_battery_pct: 10.0,
            dim_kbd_on_low_battery: true,
            whitelist: Vec::new(),
        }
    }
}

pub fn config_file_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".config")
        })
        .join("rustybattery");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.toml")
}

pub fn load_autopilot_config() -> AutopilotConfig {
    let path = config_file_path();
    let Ok(content) = fs::read_to_string(&path) else {
        let cfg = AutopilotConfig::default();
        let _ = save_autopilot_config(&cfg);
        return cfg;
    };

    let mut cfg = AutopilotConfig::default();
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        match key {
            "enabled" => cfg.enabled = val == "true" || val == "1",
            "auto_switch_power_profile" => {
                cfg.auto_switch_power_profile = val == "true" || val == "1"
            }
            "auto_freeze_enabled" => cfg.auto_freeze_enabled = val == "true" || val == "1",
            "idle_freeze_mins" => {
                if let Ok(n) = val.parse::<u64>() {
                    cfg.idle_freeze_mins = n.clamp(1, 120);
                }
            }
            "idle_power_threshold_w" => {
                if let Ok(w) = val.parse::<f64>() {
                    cfg.idle_power_threshold_w = w.clamp(0.2, 10.0);
                }
            }
            "low_battery_pct" => {
                if let Ok(p) = val.parse::<f64>() {
                    cfg.low_battery_pct = p.clamp(5.0, 50.0);
                }
            }
            "critical_battery_pct" => {
                if let Ok(p) = val.parse::<f64>() {
                    cfg.critical_battery_pct = p.clamp(3.0, 30.0);
                }
            }
            "dim_kbd_on_low_battery" => {
                cfg.dim_kbd_on_low_battery = val == "true" || val == "1"
            }
            "whitelist" => {
                let trimmed = val.trim_start_matches('[').trim_end_matches(']');
                cfg.whitelist = trimmed
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    cfg
}

pub fn save_autopilot_config(cfg: &AutopilotConfig) -> Result<()> {
    let path = config_file_path();
    let wl_items: Vec<String> = cfg
        .whitelist
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect();
    let content = format!(
        "# RustyBattery Smart Battery Saver Configuration\n\
         # Edit here or press Shift+A in the `battery` TUI.\n\n\
         [smart_saver]\n\
         enabled = {}\n\
         auto_switch_power_profile = {}\n\
         auto_freeze_enabled = {}\n\
         idle_freeze_mins = {}\n\
         idle_power_threshold_w = {:.2}\n\
         low_battery_pct = {:.0}\n\
         critical_battery_pct = {:.0}\n\
         dim_kbd_on_low_battery = {}\n\
         whitelist = [{}]\n",
        cfg.enabled,
        cfg.auto_switch_power_profile,
        cfg.auto_freeze_enabled,
        cfg.idle_freeze_mins,
        cfg.idle_power_threshold_w,
        cfg.low_battery_pct,
        cfg.critical_battery_pct,
        cfg.dim_kbd_on_low_battery,
        wl_items.join(", ")
    );
    fs::write(&path, content)
        .with_context(|| format!("Failed to write config at {}", path.display()))?;
    Ok(())
}

pub fn is_app_whitelisted(cfg: &AutopilotConfig, app_name: &str) -> bool {
    let lower = app_name.to_lowercase();
    cfg.whitelist
        .iter()
        .any(|w| lower == w.to_lowercase() || lower.contains(&w.to_lowercase()))
}

pub fn toggle_app_whitelist(app_name: &str) -> Result<bool> {
    let mut cfg = load_autopilot_config();
    let lower = app_name.to_lowercase();
    let added = if let Some(pos) = cfg
        .whitelist
        .iter()
        .position(|w| w.to_lowercase() == lower)
    {
        cfg.whitelist.remove(pos);
        false
    } else {
        cfg.whitelist.push(app_name.to_string());
        // If app was frozen, ensure it is resumed immediately upon whitelisting
        let _ = set_app_frozen(app_name, false);
        true
    };
    save_autopilot_config(&cfg)?;
    Ok(added)
}

pub fn is_autopilot_enabled(db: &Database) -> bool {
    let cfg = load_autopilot_config();
    if !cfg.enabled {
        return false;
    }
    match db.get_meta(META_AUTOPILOT_ENABLED).as_deref() {
        Some("0") | Some("false") | Some("off") => false,
        _ => true,
    }
}

pub fn set_autopilot_enabled(db: &Database, enabled: bool) -> Result<String> {
    let mut cfg = load_autopilot_config();
    cfg.enabled = enabled;
    let _ = save_autopilot_config(&cfg);
    db.set_meta(META_AUTOPILOT_ENABLED, if enabled { "1" } else { "0" })?;
    let msg = if enabled {
        format!(
            "Smart Battery Saver ENABLED (freeze unfocused >={}m, low-bat <={:.0}%)",
            cfg.idle_freeze_mins, cfg.low_battery_pct
        )
    } else {
        "Smart Battery Saver DISABLED".to_string()
    };
    record_autopilot_action(db, &msg);
    Ok(msg)
}

pub fn get_last_autopilot_action(db: &Database) -> String {
    db.get_meta(META_LAST_AUTOPILOT_ACTION).unwrap_or_else(|| {
        "Watching AC state, window focus & background drain".to_string()
    })
}

pub fn get_autopilot_log(db: &Database) -> Vec<String> {
    db.get_meta(META_AUTOPILOT_LOG)
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn record_autopilot_action(db: &Database, action: &str) {
    let _ = db.set_meta(META_LAST_AUTOPILOT_ACTION, action);
    let ts = Local::now().format("%H:%M:%S");
    let entry = format!("[{}] {}", ts, action);
    let mut lines = get_autopilot_log(db);
    lines.insert(0, entry);
    lines.truncate(12);
    let _ = db.set_meta(META_AUTOPILOT_LOG, &lines.join("\n"));
}

/// Stateful daemon engine for Automatic "Smart Battery Saver" Rules
pub struct SmartAutopilot {
    last_ac_online: Option<bool>,
    unfocused_since: HashMap<String, Instant>,
    auto_frozen_apps: HashSet<String>,
    low_bat_triggered: bool,
    critical_bat_triggered: bool,
}

impl SmartAutopilot {
    pub fn new() -> Self {
        Self {
            last_ac_online: None,
            unfocused_since: HashMap::new(),
            auto_frozen_apps: HashSet::new(),
            low_bat_triggered: false,
            critical_bat_triggered: false,
        }
    }

    /// Evaluates all Smart Battery Saver rules on each daemon tick using `config.toml`.
    pub fn evaluate(
        &mut self,
        db: &Database,
        snap: &BatterySnapshot,
        deltas: &[AppDeltaUsage],
    ) {
        let cfg = load_autopilot_config();
        if !cfg.enabled || !is_autopilot_enabled(db) {
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
        let idle_freeze_secs = cfg.idle_freeze_mins.saturating_mul(60);

        // ------------------------------------------------------------------
        // RULE 1: AC <-> Battery Power Profile & Unfreeze on AC Plug-in
        // ------------------------------------------------------------------
        if let Some(prev_ac) = self.last_ac_online {
            if prev_ac && !snap.ac_online && cfg.auto_switch_power_profile {
                let profile_note = set_system_power_profile(true);
                record_autopilot_action(db, &format!("Unplugged AC → switched to {}", profile_note));
            } else if !prev_ac && snap.ac_online {
                let profile_note = if cfg.auto_switch_power_profile {
                    set_system_power_profile(false)
                } else {
                    "AC mode".to_string()
                };
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
                record_autopilot_action(db, &action);
            }
        } else if on_battery && cfg.auto_switch_power_profile {
            let _ = set_system_power_profile(true);
        }
        self.last_ac_online = Some(snap.ac_online);

        // Reset low-battery triggers when charging above thresholds
        if !on_battery || snap.capacity_pct > cfg.low_battery_pct + 2.0 {
            self.low_bat_triggered = false;
        }
        if !on_battery || snap.capacity_pct > cfg.critical_battery_pct + 2.0 {
            self.critical_bat_triggered = false;
        }

        // ------------------------------------------------------------------
        // RULE 2: Auto-Resume Focused/Whitelisted Apps & Auto-Freeze Idle Heavy Apps
        // ------------------------------------------------------------------
        for d in deltas {
            if is_app_whitelisted(&cfg, &d.app_name) {
                self.unfocused_since.remove(&d.app_name);
                if self.auto_frozen_apps.remove(&d.app_name) {
                    let _ = set_app_frozen(&d.app_name, false);
                }
                continue;
            }

            if !is_eligible_for_autofreeze(&d.app_name, &d.category) {
                continue;
            }

            if d.is_focused {
                self.unfocused_since.remove(&d.app_name);
                if self.auto_frozen_apps.remove(&d.app_name) || d.is_frozen {
                    if set_app_frozen(&d.app_name, false).is_ok() {
                        record_autopilot_action(
                            db,
                            &format!("▶ Auto-resumed {} on window focus", d.app_name),
                        );
                    }
                }
            } else if cfg.auto_freeze_enabled && on_battery && !d.is_frozen {
                let entry = self.unfocused_since.entry(d.app_name.clone()).or_insert(now);
                let unfocused_dur = now.duration_since(*entry).as_secs();

                if unfocused_dur >= idle_freeze_secs
                    && (d.live_power_w >= cfg.idle_power_threshold_w || d.live_cpu_pct >= 3.5)
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
                        record_autopilot_action(db, &action);
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
        // RULE 3: Configurable Low-Battery & Critical-Battery Threshold Triggers
        // ------------------------------------------------------------------
        if on_battery && snap.capacity_pct <= cfg.low_battery_pct && !self.low_bat_triggered {
            self.low_bat_triggered = true;
            if cfg.auto_switch_power_profile {
                let _ = set_system_power_profile(true);
            }
            let kbd_dimmed = if cfg.dim_kbd_on_low_battery {
                dim_keyboard_backlight()
            } else {
                false
            };
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
            record_autopilot_action(db, &action);
        }

        if on_battery
            && snap.capacity_pct <= cfg.critical_battery_pct
            && !self.critical_bat_triggered
        {
            self.critical_bat_triggered = true;
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
                record_autopilot_action(db, &action);
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
