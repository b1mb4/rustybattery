use crate::apps::AppDeltaUsage;
use crate::battery::{BatterySnapshot, BatteryStatus};
use crate::db::Database;
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, TargetKind,
};
use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

const META_AUTOPILOT_ENABLED: &str = "autopilot_enabled";
const META_LAST_AUTOPILOT_ACTION: &str = "autopilot_last_action";
const META_AUTOPILOT_LOG: &str = "autopilot_action_log";
const META_BT_BLOCKED_BY_SLEEP: &str = "autopilot_bt_blocked_for_sleep";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub auto_switch_power_profile: bool,
    pub auto_freeze_enabled: bool,
    pub idle_freeze_mins: u64,
    pub idle_power_threshold_w: f64,
    pub low_battery_pct: f64,
    pub critical_battery_pct: f64,
    pub dim_kbd_on_low_battery: bool,
    pub s2idle_sleep_saver: bool,
    pub whitelist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ConfigFileWrapper {
    #[serde(default)]
    smart_saver: AutopilotConfig,
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
            s2idle_sleep_saver: true,
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

static CONFIG_CACHE: Mutex<Option<(Option<SystemTime>, AutopilotConfig)>> = Mutex::new(None);

pub fn parse_config_str(content: &str) -> AutopilotConfig {
    let mut cfg = if let Ok(wrapped) = toml::from_str::<ConfigFileWrapper>(content) {
        if content.contains("[smart_saver]") {
            wrapped.smart_saver
        } else {
            toml::from_str::<AutopilotConfig>(content).unwrap_or_default()
        }
    } else {
        toml::from_str::<AutopilotConfig>(content).unwrap_or_default()
    };
    cfg.idle_freeze_mins = cfg.idle_freeze_mins.clamp(1, 120);
    cfg.idle_power_threshold_w = cfg.idle_power_threshold_w.clamp(0.2, 10.0);
    cfg.low_battery_pct = cfg.low_battery_pct.clamp(5.0, 80.0);
    cfg.critical_battery_pct = cfg
        .critical_battery_pct
        .clamp(3.0, (cfg.low_battery_pct - 2.0).max(3.0));
    cfg
}

pub fn format_config_str(cfg: &AutopilotConfig) -> String {
    let wrapper = ConfigFileWrapper {
        smart_saver: cfg.clone(),
    };
    let body = toml::to_string_pretty(&wrapper).unwrap_or_default();
    format!(
        "# RustyBattery Smart Battery Saver Configuration\n\
         # Edit here or press Shift+A in the `battery` TUI.\n\n\
         {body}"
    )
}

pub fn load_autopilot_config() -> AutopilotConfig {
    let path = config_file_path();
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Ok(guard) = CONFIG_CACHE.lock() {
        if let Some((cached_mtime, cached_cfg)) = &*guard {
            if *cached_mtime == mtime && mtime.is_some() {
                return cached_cfg.clone();
            }
        }
    }

    let cfg = match fs::read_to_string(&path) {
        Ok(content) => parse_config_str(&content),
        Err(_) => {
            let def = AutopilotConfig::default();
            let _ = save_autopilot_config(&def);
            return def;
        }
    };

    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, cfg.clone()));
    }
    cfg
}

pub fn save_autopilot_config(cfg: &AutopilotConfig) -> Result<()> {
    let path = config_file_path();
    let content = format_config_str(cfg);
    fs::write(&path, content)
        .with_context(|| format!("Failed to write config at {}", path.display()))?;
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, cfg.clone()));
    }
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
    last_lid_closed: Option<bool>,
    bt_blocked_for_sleep: bool,
    unfocused_since: HashMap<String, Instant>,
    auto_frozen_apps: HashSet<String>,
    low_bat_triggered: bool,
    critical_bat_triggered: bool,
}

impl SmartAutopilot {
    pub fn new() -> Self {
        Self {
            last_ac_online: None,
            last_lid_closed: None,
            bt_blocked_for_sleep: false,
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

        // ------------------------------------------------------------------
        // RULE 4: s2idle Lid-Close / Pre-Sleep Saver (Bluetooth + Power Profile)
        // ------------------------------------------------------------------
        let lid_closed = is_lid_closed();
        if cfg.s2idle_sleep_saver {
            if lid_closed && self.last_lid_closed != Some(true) {
                let _ = set_system_power_profile(true);
                let _ = dim_keyboard_backlight();
                let mut bt_note = "";
                if on_battery && is_bluetooth_unblocked() && set_bluetooth_blocked(true) {
                    self.bt_blocked_for_sleep = true;
                    bt_note = " & soft-blocked Bluetooth";
                }
                record_autopilot_action(
                    db,
                    &format!("🌙 Lid closed (s2idle) → enforced power-saver{bt_note}"),
                );
            } else if !lid_closed && self.last_lid_closed == Some(true) && self.bt_blocked_for_sleep {
                let _ = set_bluetooth_blocked(false);
                self.bt_blocked_for_sleep = false;
                record_autopilot_action(db, "☀️ Lid opened → restored Bluetooth radio");
            }
        }
        self.last_lid_closed = Some(lid_closed);
    }
}

pub fn is_lid_closed() -> bool {
    let Ok(entries) = fs::read_dir("/proc/acpi/button/lid") else {
        return false;
    };
    for entry in entries.flatten() {
        let state_path = entry.path().join("state");
        if let Ok(s) = fs::read_to_string(&state_path) {
            if s.to_lowercase().contains("closed") {
                return true;
            }
        }
    }
    false
}

pub fn is_bluetooth_unblocked() -> bool {
    if let Ok(out) = Command::new("rfkill").args(["list", "bluetooth"]).output() {
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if text.contains("soft blocked: no") {
            return true;
        }
    }
    false
}

pub fn set_bluetooth_blocked(block: bool) -> bool {
    let action = if block { "block" } else { "unblock" };
    Command::new("rfkill")
        .args([action, "bluetooth"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Invoked immediately by the logind `PrepareForSleep` D-Bus listener when entering or waking from sleep.
pub fn handle_sleep_transition(db: &Database, entering_sleep: bool, on_battery: bool) {
    let cfg = load_autopilot_config();
    if !cfg.enabled || !cfg.s2idle_sleep_saver {
        return;
    }
    if entering_sleep {
        let _ = set_system_power_profile(true);
        let _ = dim_keyboard_backlight();
        let mut bt_note = "";
        if on_battery && is_bluetooth_unblocked() && set_bluetooth_blocked(true) {
            let _ = db.set_meta(META_BT_BLOCKED_BY_SLEEP, "1");
            bt_note = " & soft-blocked Bluetooth";
        }
        record_autopilot_action(
            db,
            &format!("🌙 Entering s2idle sleep → enforced power-saver{bt_note}"),
        );
    } else if db.get_meta(META_BT_BLOCKED_BY_SLEEP).as_deref() == Some("1") {
        let _ = set_bluetooth_blocked(false);
        let _ = db.delete_meta(META_BT_BLOCKED_BY_SLEEP);
        record_autopilot_action(db, "☀️ Woke from sleep → restored Bluetooth radio");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_preserves_special_app_names() {
        let mut cfg = AutopilotConfig::default();
        cfg.idle_freeze_mins = 15;
        cfg.idle_power_threshold_w = 1.25;
        cfg.s2idle_sleep_saver = false;
        cfg.whitelist = vec![
            "Spotify".to_string(),
            "App, With Comma & #Hash".to_string(),
        ];

        let serialized = format_config_str(&cfg);
        let parsed = parse_config_str(&serialized);
        assert_eq!(parsed, cfg);
        assert!(is_app_whitelisted(&parsed, "spotify"));
        assert!(is_app_whitelisted(&parsed, "App, With Comma & #Hash"));
        assert!(!is_app_whitelisted(&parsed, "Waterfox"));
    }
}
