use crate::apps::{detect_focused_apps, AppDeltaUsage};
use crate::battery::{BatterySnapshot, BatteryStatus};
use crate::db::Database;
use crate::guards::check_freeze_guards;
use crate::optimize::{
    execute_optimizations, find_app_cgroup_dirs, scan_optimization_targets, set_app_frozen,
    TargetKind,
};
use crate::power::{
    dim_and_save_kbd_backlight, restore_saved_kbd_backlight, restore_saved_power_profile,
    save_and_switch_powersave, should_skip_bluetooth_block,
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

const META_LAST_AUTOPILOT_ACTION: &str = "autopilot_last_action";
const META_AUTOPILOT_LOG: &str = "autopilot_action_log";
const META_BT_BLOCKED_BY_SLEEP: &str = "autopilot_bt_blocked_for_sleep";
const META_FROZEN_APPS: &str = "autopilot_frozen_apps";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub auto_switch_power_profile: bool,
    /// Opt-in auto-freeze for idle background apps (defaults to `false` for safety).
    pub auto_freeze_enabled: bool,
    pub idle_freeze_mins: u64,
    pub idle_power_threshold_w: f64,
    pub low_battery_pct: f64,
    pub critical_battery_pct: f64,
    pub dim_kbd_on_low_battery: bool,
    pub s2idle_sleep_saver: bool,
    pub whitelist: Vec<String>,
    /// Optional explicit allowlist of app names eligible for auto-freeze.
    pub freeze_allowlist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OptionalWrapper {
    smart_saver: Option<AutopilotConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigFileWrapper {
    smart_saver: AutopilotConfig,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_switch_power_profile: true,
            auto_freeze_enabled: false,
            idle_freeze_mins: 10,
            idle_power_threshold_w: 0.85,
            low_battery_pct: 20.0,
            critical_battery_pct: 10.0,
            dim_kbd_on_low_battery: true,
            s2idle_sleep_saver: true,
            whitelist: Vec::new(),
            freeze_allowlist: Vec::new(),
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

fn sanitize_config(mut cfg: AutopilotConfig) -> AutopilotConfig {
    cfg.idle_freeze_mins = cfg.idle_freeze_mins.clamp(1, 120);
    cfg.idle_power_threshold_w = cfg.idle_power_threshold_w.clamp(0.2, 10.0);
    cfg.low_battery_pct = cfg.low_battery_pct.clamp(5.0, 80.0);
    cfg.critical_battery_pct = cfg
        .critical_battery_pct
        .clamp(3.0, (cfg.low_battery_pct - 2.0).max(3.0));
    cfg.whitelist.retain(|s| !s.trim().is_empty());
    cfg.freeze_allowlist.retain(|s| !s.trim().is_empty());
    cfg
}

/// Strictly parses `content` as TOML and returns `Err` on syntax/type errors instead of
/// silently resetting to defaults.
pub fn try_parse_config_str(content: &str) -> Result<AutopilotConfig> {
    let opt: OptionalWrapper =
        toml::from_str(content).context("Invalid TOML syntax in config.toml")?;
    let cfg = if let Some(inner) = opt.smart_saver {
        inner
    } else {
        toml::from_str::<AutopilotConfig>(content).context("Invalid Smart Saver config fields")?
    };
    Ok(sanitize_config(cfg))
}

pub fn parse_config_str(content: &str) -> AutopilotConfig {
    try_parse_config_str(content).unwrap_or_default()
}

pub fn format_config_str(cfg: &AutopilotConfig) -> String {
    let wrapper = ConfigFileWrapper {
        smart_saver: sanitize_config(cfg.clone()),
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
        Ok(content) => match try_parse_config_str(&content) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!("rustybattery: config.toml parse error ({e}); keeping safe config");
                if let Ok(guard) = CONFIG_CACHE.lock() {
                    if let Some((_, cached_cfg)) = &*guard {
                        return cached_cfg.clone();
                    }
                }
                return AutopilotConfig::default();
            }
        },
        Err(_) => AutopilotConfig::default(),
    };

    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, cfg.clone()));
    }
    cfg
}

pub fn save_autopilot_config(cfg: &AutopilotConfig) -> Result<()> {
    let path = config_file_path();
    let clean = sanitize_config(cfg.clone());
    let content = format_config_str(&clean);
    fs::write(&path, content)
        .with_context(|| format!("Failed to write config at {}", path.display()))?;
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, clean));
    }
    Ok(())
}

fn matches_app_pattern(pattern: &str, app_name: &str) -> bool {
    let pat = pattern.trim().to_lowercase();
    let app = app_name.trim().to_lowercase();
    if pat.is_empty() || app.is_empty() {
        return false;
    }
    if let Some(inner) = pat.strip_prefix('*').and_then(|s| s.strip_suffix('*')) {
        return !inner.is_empty() && app.contains(inner);
    }
    if let Some(prefix) = pat.strip_suffix('*') {
        return !prefix.is_empty() && app.starts_with(prefix);
    }
    if let Some(suffix) = pat.strip_prefix('*') {
        return !suffix.is_empty() && app.ends_with(suffix);
    }
    app == pat
}

/// Checks if `app_name` matches the whitelist (case-insensitive exact match, or `*` glob;
/// empty strings never match).
pub fn is_app_whitelisted(cfg: &AutopilotConfig, app_name: &str) -> bool {
    cfg.whitelist
        .iter()
        .any(|w| matches_app_pattern(w, app_name))
}

pub fn toggle_app_whitelist(app_name: &str) -> Result<bool> {
    let mut cfg = load_autopilot_config();
    let trimmed = app_name.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let added = if let Some(pos) = cfg
        .whitelist
        .iter()
        .position(|w| w.trim().eq_ignore_ascii_case(trimmed))
    {
        cfg.whitelist.remove(pos);
        false
    } else {
        cfg.whitelist.push(trimmed.to_string());
        let _ = set_app_frozen(trimmed, false);
        true
    };
    save_autopilot_config(&cfg)?;
    Ok(added)
}

pub fn is_autopilot_enabled(_db: &Database) -> bool {
    load_autopilot_config().enabled
}

pub fn set_autopilot_enabled(db: &Database, enabled: bool) -> Result<String> {
    let mut cfg = load_autopilot_config();
    cfg.enabled = enabled;
    save_autopilot_config(&cfg)?;
    if !enabled {
        let _ = thaw_persisted_frozen_apps(db);
    }
    let msg = if enabled {
        format!(
            "Smart Battery Saver ENABLED (low-bat <={:.0}%, auto-freeze={})",
            cfg.low_battery_pct,
            if cfg.auto_freeze_enabled { "ON" } else { "OFF" }
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
    let entry = format!("[{ts}] {action}");
    let mut lines = get_autopilot_log(db);
    lines.insert(0, entry);
    lines.truncate(12);
    let _ = db.set_meta(META_AUTOPILOT_LOG, &lines.join("\n"));
}

pub fn persist_frozen_apps(db: &Database, apps: &HashSet<String>) {
    if apps.is_empty() {
        let _ = db.delete_meta(META_FROZEN_APPS);
    } else if let Ok(json) = serde_json::to_string(apps) {
        let _ = db.set_meta(META_FROZEN_APPS, &json);
    }
}

/// Thaws any apps recorded in `meta` `"autopilot_frozen_apps"` from a previous daemon/TUI session.
pub fn thaw_persisted_frozen_apps(db: &Database) -> usize {
    let Some(raw) = db.get_meta(META_FROZEN_APPS) else {
        return 0;
    };
    let _ = db.delete_meta(META_FROZEN_APPS);
    let apps: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
    let mut count = 0usize;
    for app in apps {
        if set_app_frozen(&app, false).is_ok() {
            count += 1;
        }
    }
    count
}

/// Stateful daemon engine for Automatic "Smart Battery Saver" Rules
pub struct SmartAutopilot {
    last_ac_online: Option<bool>,
    last_lid_closed: Option<bool>,
    last_eval_instant: Option<Instant>,
    bt_blocked_for_sleep: bool,
    pub unfocused_since: HashMap<String, Instant>,
    pub auto_frozen_apps: HashSet<String>,
    low_bat_triggered: bool,
    critical_bat_triggered: bool,
}

impl Default for SmartAutopilot {
    fn default() -> Self {
        Self::new()
    }
}

impl SmartAutopilot {
    pub fn new() -> Self {
        Self {
            last_ac_online: None,
            last_lid_closed: None,
            last_eval_instant: None,
            bt_blocked_for_sleep: false,
            unfocused_since: HashMap::new(),
            auto_frozen_apps: HashSet::new(),
            low_bat_triggered: false,
            critical_bat_triggered: false,
        }
    }

    pub fn thaw_all(&mut self, db: &Database) -> usize {
        let mut count = 0usize;
        let apps: Vec<String> = self.auto_frozen_apps.drain().collect();
        for app in apps {
            if set_app_frozen(&app, false).is_ok() {
                count += 1;
            }
        }
        persist_frozen_apps(db, &self.auto_frozen_apps);
        count + thaw_persisted_frozen_apps(db)
    }

    /// Evaluates all Smart Battery Saver rules on each daemon tick using `config.toml`.
    pub fn evaluate(
        &mut self,
        db: &Database,
        snap: &BatterySnapshot,
        deltas: &[AppDeltaUsage],
    ) {
        let cfg = load_autopilot_config();
        if !cfg.enabled {
            if !self.auto_frozen_apps.is_empty() {
                self.thaw_all(db);
            }
            return;
        }

        let now = Instant::now();
        if let Some(prev_eval) = self.last_eval_instant {
            if now.duration_since(prev_eval).as_secs() > 45 {
                // Gap > 45s (e.g. resume from suspend or daemon stall): clear stale unfocused timers
                self.unfocused_since.clear();
            }
        }
        self.last_eval_instant = Some(now);

        let on_battery = !snap.ac_online && snap.status == BatteryStatus::Discharging;
        let idle_freeze_secs = cfg.idle_freeze_mins.saturating_mul(60);

        // ------------------------------------------------------------------
        // RULE 1: AC <-> Battery Power Profile & Unfreeze on AC Plug-in
        // ------------------------------------------------------------------
        if let Some(prev_ac) = self.last_ac_online {
            if prev_ac && !snap.ac_online && cfg.auto_switch_power_profile {
                if let Ok(note) = save_and_switch_powersave(db) {
                    record_autopilot_action(db, &format!("Unplugged AC → switched to {note}"));
                }
            } else if !prev_ac && snap.ac_online {
                let profile_note = if cfg.auto_switch_power_profile {
                    restore_saved_power_profile(db).unwrap_or_else(|| "AC mode".to_string())
                } else {
                    "AC mode".to_string()
                };
                let _ = restore_saved_kbd_backlight(db);
                let unfrozen_count = self.thaw_all(db);
                let action = if unfrozen_count > 0 {
                    format!(
                        "Plugged into AC → {profile_note} & resumed {unfrozen_count} frozen app(s)"
                    )
                } else {
                    format!("Plugged into AC → restored {profile_note}")
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
        // Fail closed when window focus is unknown (`Focus::Unknown` and no delta has `is_focused`).
        let any_delta_focused = deltas.iter().any(|d| d.is_focused);
        let focus_unknown = !any_delta_focused && detect_focused_apps().is_unknown();

        if focus_unknown {
            self.unfocused_since.clear();
            if !self.auto_frozen_apps.is_empty() {
                self.thaw_all(db);
            }
        } else {
            for d in deltas {
                if is_app_whitelisted(&cfg, &d.app_name) {
                    self.unfocused_since.remove(&d.app_name);
                    if self.auto_frozen_apps.remove(&d.app_name) {
                        let _ = set_app_frozen(&d.app_name, false);
                        persist_frozen_apps(db, &self.auto_frozen_apps);
                    }
                    continue;
                }

                if !is_eligible_for_autofreeze(&cfg, &d.app_name, &d.category) {
                    continue;
                }

                if d.is_focused {
                    self.unfocused_since.remove(&d.app_name);
                    if self.auto_frozen_apps.remove(&d.app_name) || d.is_frozen {
                        if set_app_frozen(&d.app_name, false).is_ok() {
                            persist_frozen_apps(db, &self.auto_frozen_apps);
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
                        let dirs = find_app_cgroup_dirs(&d.app_name);
                        if check_freeze_guards(&d.app_name, &dirs).is_some() {
                            continue;
                        }
                        if set_app_frozen(&d.app_name, true).is_ok() {
                            self.auto_frozen_apps.insert(d.app_name.clone());
                            persist_frozen_apps(db, &self.auto_frozen_apps);
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
            // P0-5: Evict unfocused timers for apps no longer present in `deltas`
            self.unfocused_since
                .retain(|name, _| deltas.iter().any(|d| &d.app_name == name));
        }

        // ------------------------------------------------------------------
        // RULE 3: Configurable Low-Battery & Critical-Battery Threshold Triggers
        // ------------------------------------------------------------------
        if on_battery && snap.capacity_pct <= cfg.low_battery_pct && !self.low_bat_triggered {
            self.low_bat_triggered = true;
            let prof_switched = if cfg.auto_switch_power_profile {
                save_and_switch_powersave(db).is_ok()
            } else {
                false
            };
            let kbd_dimmed = if cfg.dim_kbd_on_low_battery {
                dim_and_save_kbd_backlight(db)
            } else {
                false
            };
            if prof_switched || kbd_dimmed {
                let action = format!(
                    "🔋 Low Battery ({:.0}%) → power-saver={} kbd-dimmed={}",
                    snap.capacity_pct, prof_switched, kbd_dimmed
                );
                record_autopilot_action(db, &action);
            }
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
        // RULE 4: s2idle Lid-Close / Pre-Sleep Saver (Battery only, restores on open)
        // ------------------------------------------------------------------
        let lid_closed = is_lid_closed();
        if cfg.s2idle_sleep_saver {
            if lid_closed && self.last_lid_closed != Some(true) && on_battery {
                let _ = save_and_switch_powersave(db);
                let _ = dim_and_save_kbd_backlight(db);
                let mut bt_note = "";
                if !should_skip_bluetooth_block()
                    && is_bluetooth_unblocked()
                    && set_bluetooth_blocked(true)
                {
                    self.bt_blocked_for_sleep = true;
                    let _ = db.set_meta(META_BT_BLOCKED_BY_SLEEP, "1");
                    bt_note = " & soft-blocked Bluetooth";
                }
                record_autopilot_action(
                    db,
                    &format!("🌙 Lid closed on battery (s2idle) → power-saver{bt_note}"),
                );
            } else if !lid_closed && self.last_lid_closed == Some(true) {
                if on_battery {
                    let _ = restore_saved_kbd_backlight(db);
                } else {
                    let _ = restore_saved_power_profile(db);
                    let _ = restore_saved_kbd_backlight(db);
                }
                if self.bt_blocked_for_sleep
                    || db.get_meta(META_BT_BLOCKED_BY_SLEEP).as_deref() == Some("1")
                {
                    let _ = set_bluetooth_blocked(false);
                    self.bt_blocked_for_sleep = false;
                    let _ = db.delete_meta(META_BT_BLOCKED_BY_SLEEP);
                    record_autopilot_action(db, "☀️ Lid opened → restored Bluetooth radio");
                }
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

pub fn is_eligible_for_autofreeze(cfg: &AutopilotConfig, app_name: &str, category: &str) -> bool {
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
        || lower.contains("ghostty")
        || lower.contains("konsole")
        || lower.contains("foot")
    {
        return false;
    }

    if !cfg.freeze_allowlist.is_empty() {
        return cfg
            .freeze_allowlist
            .iter()
            .any(|pat| matches_app_pattern(pat, app_name));
    }

    matches!(category, "Browser" | "Productivity" | "App")
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
        if !on_battery {
            return;
        }
        let _ = save_and_switch_powersave(db);
        let _ = dim_and_save_kbd_backlight(db);
        let mut bt_note = "";
        if !should_skip_bluetooth_block()
            && is_bluetooth_unblocked()
            && set_bluetooth_blocked(true)
        {
            let _ = db.set_meta(META_BT_BLOCKED_BY_SLEEP, "1");
            bt_note = " & soft-blocked Bluetooth";
        }
        record_autopilot_action(
            db,
            &format!("🌙 Entering s2idle sleep on battery → power-saver{bt_note}"),
        );
    } else {
        if !on_battery {
            let _ = restore_saved_power_profile(db);
        }
        let _ = restore_saved_kbd_backlight(db);
        if db.get_meta(META_BT_BLOCKED_BY_SLEEP).as_deref() == Some("1") {
            let _ = set_bluetooth_blocked(false);
            let _ = db.delete_meta(META_BT_BLOCKED_BY_SLEEP);
            record_autopilot_action(db, "☀️ Woke from sleep → restored Bluetooth & backlight");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_autofreeze_disabled() {
        let cfg = AutopilotConfig::default();
        assert!(!cfg.auto_freeze_enabled);
    }

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
        let parsed = try_parse_config_str(&serialized).unwrap();
        assert_eq!(parsed, cfg);
        assert!(is_app_whitelisted(&parsed, "spotify"));
        assert!(is_app_whitelisted(&parsed, "App, With Comma & #Hash"));
        assert!(!is_app_whitelisted(&parsed, "Waterfox"));
    }

    #[test]
    fn whitelist_uses_exact_match_and_rejects_empty_or_substring() {
        // Probe P0-9: empty entry '' and 'Code' against 'Decoder Studio'
        let mut cfg = AutopilotConfig::default();
        cfg.whitelist = vec!["".to_string(), "Code".to_string(), "Water*".to_string()];
        assert!(!is_app_whitelisted(&cfg, "Spotify"));
        assert!(!is_app_whitelisted(&cfg, "Decoder Studio"));
        assert!(is_app_whitelisted(&cfg, "code"));
        assert!(is_app_whitelisted(&cfg, "Waterfox"));
    }

    #[test]
    fn broken_toml_returns_err_instead_of_silent_defaults() {
        // Probe P0-9: missing `]` in TOML must return Err
        let broken = "[smart_saver]\nauto_freeze_enabled = false\nwhitelist = [\"Spotify\"\n";
        assert!(try_parse_config_str(broken).is_err());
    }

    #[test]
    fn stale_unfocused_timer_is_evicted_when_app_closes() {
        // Probe P0-5: entry 3h old, app absent from deltas -> entry is evicted
        let db = Database::open_in_memory().unwrap();
        let mut cfg = AutopilotConfig::default();
        cfg.enabled = true;
        let _ = save_autopilot_config(&cfg);

        let mut ap = SmartAutopilot::new();
        ap.unfocused_since.insert(
            "ClosedApp".to_string(),
            Instant::now() - std::time::Duration::from_secs(3 * 3600),
        );

        let snap = BatterySnapshot {
            timestamp: 1000,
            device_name: "BAT0".to_string(),
            manufacturer: "Test".to_string(),
            model_name: "Test".to_string(),
            technology: "Li-ion".to_string(),
            capacity_pct: 75.0,
            capacity_int: 75,
            status: BatteryStatus::Discharging,
            power_w: 10.0,
            energy_now_wh: 45.0,
            energy_full_wh: 60.0,
            energy_design_wh: 60.0,
            health_pct: 100.0,
            voltage_v: 12.0,
            cycle_count: 10,
            ac_online: false,
            time_remaining_mins: Some(180),
            screen_on: true,
            boot_id: "b1".to_string(),
            suspended_total_secs: 0.0,
            charge_limit_pct: None,
        };
        let active_focused_delta = vec![AppDeltaUsage {
            app_name: "Ghostty".to_string(),
            category: "Terminal".to_string(),
            unit_hint: "app.service".to_string(),
            battery_pct: 0.1,
            focused_battery_pct: 0.1,
            energy_wh: 0.05,
            cpu_secs: 0.2,
            active_secs: 5.0,
            focused_secs: 5.0,
            live_power_w: 2.0,
            live_cpu_pct: 4.0,
            memory_mb: 100.0,
            pids_count: 2,
            is_frozen: false,
            is_focused: true,
        }];
        ap.evaluate(&db, &snap, &active_focused_delta);
        assert!(!ap.unfocused_since.contains_key("ClosedApp"));
    }
}
