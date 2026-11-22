use crate::apps::{detect_focused_apps, AppDeltaUsage, Focus};
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

const KNOWN_CONFIG_KEYS: &[&str] = &[
    "config_version",
    "enabled",
    "auto_switch_power_profile",
    "auto_freeze_enabled",
    "idle_freeze_mins",
    "idle_power_threshold_w",
    "low_battery_pct",
    "critical_battery_pct",
    "dim_kbd_on_low_battery",
    "s2idle_sleep_saver",
    "whitelist",
    "freeze_allowlist",
    "display_min_w",
    "display_max_w",
    "calibrated_cpu_w_per_core",
    "calibrated_baseline_w",
    "calibrated_rapl_idle_w",
    "calibrated_at",
    "app_hourly_budget_pct",
    "app_budgets",
    "notifications_enabled",
    "stop_grace_secs",
    "retention_days",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AutopilotConfig {
    /// Config schema version for forward/backward compatibility checks (`N-11`).
    pub config_version: u32,
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
    /// Minimum display power estimate in Watts (at 0% brightness).
    pub display_min_w: f64,
    /// Maximum display power estimate in Watts (at 100% brightness).
    pub display_max_w: f64,
    /// Calibrated or configured CPU package power per full core in Watts.
    pub calibrated_cpu_w_per_core: f64,
    /// Calibrated or configured system baseline power in Watts.
    pub calibrated_baseline_w: f64,
    /// Calibrated idle RAPL package power floor in Watts (`N-7`).
    pub calibrated_rapl_idle_w: f64,
    /// Unix timestamp of the last completed `battery calibrate` run.
    pub calibrated_at: Option<i64>,
    /// Optional per-app hourly battery drain budget (% of battery in 1 hour); 0.0 disables alerts.
    pub app_hourly_budget_pct: f64,
    /// Optional per-app hourly battery drain budget overrides (`[budgets]` or `app_budgets`, `N-12`).
    pub app_budgets: HashMap<String, f64>,
    /// Enable or disable background desktop alert notifications (low battery, high drain, budget, autopilot actions).
    pub notifications_enabled: bool,
    /// Grace period in seconds before forcing SIGKILL when stopping apps/services cleanly.
    pub stop_grace_secs: u64,
    /// Days of raw/historical telemetry retained before `prune` deletes rows (`N-15`).
    pub retention_days: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFileWrapper {
    smart_saver: AutopilotConfig,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            config_version: 1,
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
            display_min_w: 0.8,
            display_max_w: 4.5,
            calibrated_cpu_w_per_core: 12.0,
            calibrated_baseline_w: 1.0,
            calibrated_rapl_idle_w: 0.0,
            calibrated_at: None,
            app_hourly_budget_pct: 0.0,
            app_budgets: HashMap::new(),
            notifications_enabled: true,
            stop_grace_secs: 10,
            retention_days: 90,
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
    crate::paths::set_private_dir_mode(&dir);
    dir.join("config.toml")
}

type ConfigCacheEntry = (Option<SystemTime>, AutopilotConfig);
static CONFIG_CACHE: Mutex<Option<ConfigCacheEntry>> = Mutex::new(None);
static CONFIG_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn last_config_error() -> Option<String> {
    CONFIG_ERROR.lock().ok().and_then(|g| g.clone())
}

fn safe_fallback_config() -> AutopilotConfig {
    AutopilotConfig {
        enabled: false,
        auto_freeze_enabled: false,
        ..AutopilotConfig::default()
    }
}

fn sanitize_config(mut cfg: AutopilotConfig) -> AutopilotConfig {
    cfg.idle_freeze_mins = cfg.idle_freeze_mins.clamp(1, 120);
    cfg.idle_power_threshold_w = cfg.idle_power_threshold_w.clamp(0.2, 10.0);
    cfg.low_battery_pct = cfg.low_battery_pct.clamp(5.0, 80.0);
    cfg.critical_battery_pct = cfg
        .critical_battery_pct
        .clamp(3.0, (cfg.low_battery_pct - 2.0).max(3.0));
    cfg.whitelist.retain(|s| !s.trim().is_empty());
    cfg.freeze_allowlist.retain(|s| !s.trim().is_empty());
    cfg.display_min_w = cfg.display_min_w.clamp(0.1, 15.0);
    cfg.display_max_w = cfg.display_max_w.clamp(cfg.display_min_w + 0.2, 25.0);
    cfg.calibrated_cpu_w_per_core = cfg.calibrated_cpu_w_per_core.clamp(1.0, 45.0);
    cfg.calibrated_baseline_w = cfg.calibrated_baseline_w.clamp(0.2, 15.0);
    cfg.calibrated_rapl_idle_w = cfg.calibrated_rapl_idle_w.clamp(0.0, 15.0);
    cfg.app_hourly_budget_pct = cfg.app_hourly_budget_pct.clamp(0.0, 100.0);
    for val in cfg.app_budgets.values_mut() {
        *val = val.clamp(0.0, 100.0);
    }
    cfg.stop_grace_secs = cfg.stop_grace_secs.clamp(2, 120);
    cfg.retention_days = cfg.retention_days.clamp(7, 365);
    cfg
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a_bytes: Vec<char> = a.chars().collect();
    let b_bytes: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_bytes.len()).collect();
    let mut curr = vec![0; b_bytes.len() + 1];
    for (i, &ac) in a_bytes.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b_bytes.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_bytes.len()]
}

fn suggest_closest_key(unknown: &str) -> Option<&'static str> {
    let lower = unknown.to_lowercase();
    let mut best: Option<(usize, &'static str)> = None;
    for &key in KNOWN_CONFIG_KEYS {
        let d = levenshtein(&lower, key);
        if d <= 3 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, key));
        }
    }
    best.map(|(_, k)| k)
}

/// Parses TOML config with forward/backward compatibility (`N-11`):
/// valid keys still apply and unknown keys are collected into `warnings` with typo suggestions,
/// disabling `auto_freeze_enabled` for safety while unknown keys remain present.
pub fn parse_config_with_warnings(content: &str) -> Result<(AutopilotConfig, Vec<String>)> {
    let root: toml::Value =
        toml::from_str(content).context("Invalid TOML syntax in config.toml")?;
    let mut root_tbl = match root {
        toml::Value::Table(t) => t,
        _ => anyhow::bail!("Expected a TOML table at root of config.toml"),
    };

    let mut warnings = Vec::new();
    let mut extra_budgets: HashMap<String, f64> = HashMap::new();
    if let Some(budgets_val) = root_tbl.remove("budgets") {
        match budgets_val {
            toml::Value::Table(bt) => {
                for (k, v) in bt {
                    let pct = v
                        .as_float()
                        .or_else(|| v.as_integer().map(|i| i as f64))
                        .with_context(|| format!("Invalid budget value for [budgets].{k}"))?;
                    extra_budgets.insert(k, pct);
                }
            }
            _ => anyhow::bail!("[budgets] must be a TOML table"),
        }
    }

    let mut smart_tbl = if let Some(ss) = root_tbl.remove("smart_saver") {
        for extra_k in root_tbl.keys() {
            warnings.push(format!("Unknown top-level key/section '{extra_k}' (ignored)"));
        }
        match ss {
            toml::Value::Table(t) => t,
            _ => anyhow::bail!("[smart_saver] must be a TOML table"),
        }
    } else {
        root_tbl
    };

    let unknown_keys: Vec<String> = smart_tbl
        .keys()
        .filter(|k| !KNOWN_CONFIG_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    for uk in unknown_keys {
        smart_tbl.remove(&uk);
        if let Some(sugg) = suggest_closest_key(&uk) {
            warnings.push(format!("Unknown key '{uk}' (did you mean '{sugg}'?)"));
        } else {
            warnings.push(format!("Unknown key '{uk}' (ignored)"));
        }
    }

    let mut cfg: AutopilotConfig = toml::Value::Table(smart_tbl)
        .try_into()
        .context("Invalid Smart Saver config field values")?;
    for (k, v) in extra_budgets {
        cfg.app_budgets.insert(k, v);
    }
    if !warnings.is_empty() {
        cfg.auto_freeze_enabled = false;
    }
    Ok((sanitize_config(cfg), warnings))
}

/// Strictly parses `content` as TOML and returns `Err` on syntax, type, or unknown-key errors.
pub fn try_parse_config_str(content: &str) -> Result<AutopilotConfig> {
    let (cfg, warnings) = parse_config_with_warnings(content)?;
    if !warnings.is_empty() {
        anyhow::bail!("{}", warnings.join("; "));
    }
    Ok(cfg)
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

    let cfg = match fs::read_to_string(path) {
        Ok(content) => match parse_config_with_warnings(&content) {
            Ok((parsed, warnings)) => {
                if warnings.is_empty() {
                    if let Ok(mut err_g) = CONFIG_ERROR.lock() {
                        *err_g = None;
                    }
                } else {
                    let warn_msg = warnings.join("; ");
                    let is_new_err = if let Ok(mut err_g) = CONFIG_ERROR.lock() {
                        let changed = err_g.as_deref() != Some(warn_msg.as_str());
                        *err_g = Some(warn_msg.clone());
                        changed
                    } else {
                        false
                    };
                    if is_new_err && parsed.notifications_enabled {
                        send_autopilot_notification(
                            "⚠️ RustyBattery Config Warning",
                            &format!("{warn_msg} (auto-freeze disabled until resolved)."),
                        );
                    }
                }
                parsed
            }
            Err(e) => {
                let err_msg = format!("{e:#}");
                let is_new_err = if let Ok(mut err_g) = CONFIG_ERROR.lock() {
                    let changed = err_g.as_deref() != Some(err_msg.as_str());
                    *err_g = Some(err_msg.clone());
                    changed
                } else {
                    false
                };
                if is_new_err {
                    send_autopilot_notification(
                        "⚠️ RustyBattery Config Error",
                        &format!("config.toml error ({err_msg}); Smart Saver paused in safe mode."),
                    );
                }
                let fallback = if let Ok(guard) = CONFIG_CACHE.lock() {
                    guard
                        .as_ref()
                        .map(|(_, c)| c.clone())
                        .unwrap_or_else(safe_fallback_config)
                } else {
                    safe_fallback_config()
                };
                if let Ok(mut guard) = CONFIG_CACHE.lock() {
                    *guard = Some((mtime, fallback.clone()));
                }
                return fallback;
            }
        },
        Err(_) => {
            if let Ok(mut err_g) = CONFIG_ERROR.lock() {
                *err_g = None;
            }
            AutopilotConfig::default()
        }
    };

    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, cfg.clone()));
    }
    cfg
}

pub fn save_autopilot_config(cfg: &AutopilotConfig) -> Result<()> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        crate::paths::set_private_dir_mode(parent);
    }
    let clean = sanitize_config(cfg.clone());
    let content = format_config_str(&clean);
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, content)
        .with_context(|| format!("Failed to write temp config at {}", tmp.display()))?;
    crate::paths::set_private_file_mode(&tmp);
    fs::rename(tmp, &path)
        .with_context(|| format!("Failed to rename config into {}", path.display()))?;
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Ok(mut guard) = CONFIG_CACHE.lock() {
        *guard = Some((mtime, clean));
    }
    if let Ok(mut err_g) = CONFIG_ERROR.lock() {
        *err_g = None;
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

/// One-time cleanup for legacy `autopilot_frozen_apps` metadata left by earlier versions;
/// active freeze ownership is now tracked solely in `rb_frozen_units.json` (`N-6`).
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

/// Hardware and desktop action abstraction for testable `SmartAutopilot` evaluation (`E-4`).
pub trait SystemActions {
    fn detect_focus(&self) -> Focus;
    fn set_app_frozen(&self, app_name: &str, freeze: bool) -> Result<String>;
    fn check_freeze_guards(&self, app_name: &str) -> Option<&'static str>;
    fn save_and_switch_powersave(&self, db: &Database) -> Result<String>;
    fn restore_saved_power_profile(&self, db: &Database) -> Option<String>;
    fn dim_and_save_kbd_backlight(&self, db: &Database) -> bool;
    fn restore_saved_kbd_backlight(&self, db: &Database) -> bool;
    fn is_lid_closed(&self) -> bool;
    fn should_skip_bluetooth_block(&self) -> bool;
    fn is_bluetooth_unblocked(&self) -> bool;
    fn set_bluetooth_blocked(&self, block: bool) -> bool;
    fn thaw_owned(&self) -> usize;
    fn trigger_critical_stop(&self, db: &Database, snap: &BatterySnapshot);
}

pub struct RealSystemActions;

impl SystemActions for RealSystemActions {
    fn detect_focus(&self) -> Focus {
        detect_focused_apps()
    }
    fn set_app_frozen(&self, app_name: &str, freeze: bool) -> Result<String> {
        set_app_frozen(app_name, freeze)
    }
    fn check_freeze_guards(&self, app_name: &str) -> Option<&'static str> {
        let dirs = find_app_cgroup_dirs(app_name);
        check_freeze_guards(app_name, &dirs)
    }
    fn save_and_switch_powersave(&self, db: &Database) -> Result<String> {
        save_and_switch_powersave(db)
    }
    fn restore_saved_power_profile(&self, db: &Database) -> Option<String> {
        restore_saved_power_profile(db)
    }
    fn dim_and_save_kbd_backlight(&self, db: &Database) -> bool {
        dim_and_save_kbd_backlight(db)
    }
    fn restore_saved_kbd_backlight(&self, db: &Database) -> bool {
        restore_saved_kbd_backlight(db)
    }
    fn is_lid_closed(&self) -> bool {
        is_lid_closed()
    }
    fn should_skip_bluetooth_block(&self) -> bool {
        should_skip_bluetooth_block()
    }
    fn is_bluetooth_unblocked(&self) -> bool {
        is_bluetooth_unblocked()
    }
    fn set_bluetooth_blocked(&self, block: bool) -> bool {
        set_bluetooth_blocked(block)
    }
    fn thaw_owned(&self) -> usize {
        crate::optimize::thaw_owned_user_cgroups().len()
    }
    fn trigger_critical_stop(&self, db: &Database, snap: &BatterySnapshot) {
        let mut targets = scan_optimization_targets(snap.power_w);
        for t in &mut targets {
            t.selected = t.kind == TargetKind::UserUnit;
        }
        let db_path = db.path.clone();
        let cap_pct = snap.capacity_pct;
        std::thread::spawn(move || {
            let res = execute_optimizations(&targets);
            if !res.actions_taken.is_empty() {
                if let Ok(worker_db) = Database::open(&db_path) {
                    let action = format!(
                        "🚨 Critical Battery ({cap_pct:.0}%) → Auto-stopped {} background service(s) (~{:.1}W saved)",
                        res.actions_taken.len(),
                        res.estimated_watts_saved
                    );
                    record_autopilot_action(&worker_db, &action);
                }
            }
        });
    }
}

/// Stateful daemon engine for Automatic "Smart Battery Saver" Rules
pub struct SmartAutopilot {
    pub last_ac_online: Option<bool>,
    pub last_lid_closed: Option<bool>,
    last_eval_instant: Option<Instant>,
    pub bt_blocked_for_sleep: bool,
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
        self.thaw_all_with(db, &RealSystemActions)
    }

    pub fn thaw_all_with(&mut self, db: &Database, actions: &dyn SystemActions) -> usize {
        let mut count = 0usize;
        let apps: Vec<String> = self.auto_frozen_apps.drain().collect();
        for app in apps {
            if actions.set_app_frozen(&app, false).is_ok() {
                count += 1;
            }
        }
        let owned = actions.thaw_owned();
        count + owned + thaw_persisted_frozen_apps(db)
    }

    /// Evaluates all Smart Battery Saver rules on each daemon tick using `config.toml`.
    pub fn evaluate(
        &mut self,
        db: &Database,
        snap: &BatterySnapshot,
        deltas: &[AppDeltaUsage],
    ) {
        let cfg = load_autopilot_config();
        self.evaluate_with_actions(db, snap, deltas, &cfg, &RealSystemActions);
    }

    pub fn evaluate_with_actions(
        &mut self,
        db: &Database,
        snap: &BatterySnapshot,
        deltas: &[AppDeltaUsage],
        cfg: &AutopilotConfig,
        actions: &dyn SystemActions,
    ) {
        if !cfg.enabled {
            if !self.auto_frozen_apps.is_empty() {
                self.thaw_all_with(db, actions);
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
                if let Ok(note) = actions.save_and_switch_powersave(db) {
                    record_autopilot_action(db, &format!("Unplugged AC → switched to {note}"));
                }
            } else if !prev_ac && snap.ac_online {
                let profile_note = if cfg.auto_switch_power_profile {
                    actions
                        .restore_saved_power_profile(db)
                        .unwrap_or_else(|| "AC mode".to_string())
                } else {
                    "AC mode".to_string()
                };
                let _ = actions.restore_saved_kbd_backlight(db);
                let unfrozen_count = self.thaw_all_with(db, actions);
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
        let focus_unknown = !any_delta_focused && actions.detect_focus().is_unknown();

        if focus_unknown {
            self.unfocused_since.clear();
            if !self.auto_frozen_apps.is_empty() {
                self.thaw_all_with(db, actions);
            }
        } else {
            for d in deltas {
                if is_app_whitelisted(cfg, &d.app_name) {
                    self.unfocused_since.remove(&d.app_name);
                    if self.auto_frozen_apps.remove(&d.app_name) {
                        let _ = actions.set_app_frozen(&d.app_name, false);
                    }
                    continue;
                }

                if !is_eligible_for_autofreeze(cfg, &d.app_name, &d.category) {
                    continue;
                }

                if d.is_focused {
                    self.unfocused_since.remove(&d.app_name);
                    if (self.auto_frozen_apps.remove(&d.app_name) || d.is_frozen)
                        && actions.set_app_frozen(&d.app_name, false).is_ok()
                    {
                        record_autopilot_action(
                            db,
                            &format!("▶ Auto-resumed {} on window focus", d.app_name),
                        );
                    }
                } else if cfg.auto_freeze_enabled && on_battery && !d.is_frozen {
                    let entry = self.unfocused_since.entry(d.app_name.clone()).or_insert(now);
                    let unfocused_dur = now.duration_since(*entry).as_secs();

                    if unfocused_dur >= idle_freeze_secs
                        && (d.live_power_w >= cfg.idle_power_threshold_w || d.live_cpu_pct >= 3.5)
                    {
                        if actions.check_freeze_guards(&d.app_name).is_some() {
                            continue;
                        }
                        if actions.set_app_frozen(&d.app_name, true).is_ok() {
                            self.auto_frozen_apps.insert(d.app_name.clone());
                            self.unfocused_since.remove(&d.app_name);
                            let action = format!(
                                "❄️ Auto-froze unfocused {} (idle {}m, saving {:.1}W)",
                                d.app_name,
                                unfocused_dur / 60,
                                d.live_power_w
                            );
                            record_autopilot_action(db, &action);
                            if cfg.notifications_enabled {
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
                actions.save_and_switch_powersave(db).is_ok()
            } else {
                false
            };
            let kbd_dimmed = if cfg.dim_kbd_on_low_battery {
                actions.dim_and_save_kbd_backlight(db)
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
            actions.trigger_critical_stop(db, snap);
        }

        // ------------------------------------------------------------------
        // RULE 4: s2idle Lid-Close / Pre-Sleep Saver (Battery only, restores on open)
        // ------------------------------------------------------------------
        let lid_closed = actions.is_lid_closed();
        if cfg.s2idle_sleep_saver {
            if lid_closed && self.last_lid_closed != Some(true) && on_battery {
                let _ = actions.save_and_switch_powersave(db);
                let _ = actions.dim_and_save_kbd_backlight(db);
                let mut bt_note = "";
                if !actions.should_skip_bluetooth_block()
                    && actions.is_bluetooth_unblocked()
                    && actions.set_bluetooth_blocked(true)
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
                    let _ = actions.restore_saved_kbd_backlight(db);
                } else {
                    let _ = actions.restore_saved_power_profile(db);
                    let _ = actions.restore_saved_kbd_backlight(db);
                }
                if self.bt_blocked_for_sleep
                    || db.get_meta(META_BT_BLOCKED_BY_SLEEP).as_deref() == Some("1")
                {
                    let _ = actions.set_bluetooth_blocked(false);
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
        if let Ok(s) = fs::read_to_string(state_path) {
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
    if category == "Terminal" || category == "Desktop UI" || category == "System" {
        return false;
    }
    let lower = app_name.trim().to_lowercase();
    let tokens: Vec<&str> = lower
        .split([' ', '-', '_', '.', '/'])
        .filter(|s| !s.is_empty())
        .collect();
    for protected in [
        "gnome-shell",
        "wayland",
        "xorg",
        "pipewire",
        "wireplumber",
        "pulseaudio",
        "rustybattery",
        "systemd",
        "ptyxis",
        "terminal",
        "alacritty",
        "kitty",
        "wezterm",
        "ghostty",
        "konsole",
        "foot",
        "footclient",
    ] {
        if lower == protected || tokens.contains(&protected) {
            return false;
        }
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
    if !load_autopilot_config().notifications_enabled {
        return;
    }
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
    use crate::apps::Confidence;

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
    fn broken_toml_returns_err_and_safe_fallback_disables_autopilot() {
        // Probe F-1: missing `]` in TOML must return Err, and safe fallback has enabled == false
        let broken = "[smart_saver]\nenabled = false\nwhitelist = [\"Spotify\"\n";
        assert!(try_parse_config_str(broken).is_err());
        let safe = safe_fallback_config();
        assert!(!safe.enabled);
        assert!(!safe.auto_freeze_enabled);
    }

    #[test]
    fn unknown_key_in_toml_is_rejected() {
        // Probe F-2: misspelled key `whitelst` must return Err in strict check
        let typo = "[smart_saver]\nauto_freeze_enabled = true\nwhitelst = [\"Spotify\"]\n";
        assert!(try_parse_config_str(typo).is_err());
    }

    #[test]
    fn unknown_key_suggests_closest_name_and_disables_autofreeze() {
        let content = "\
[smart_saver]
auto_freeze_enabled = true
idle_freeze_mins = 15
whitelst = [\"Spotify\"]

[budgets]
Waterfox = 12.5
";
        let (cfg, warnings) = parse_config_with_warnings(content).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("did you mean 'whitelist'"));
        assert!(!cfg.auto_freeze_enabled);
        assert_eq!(cfg.idle_freeze_mins, 15);
        assert_eq!(cfg.app_budgets.get("Waterfox"), Some(&12.5));
    }

    #[test]
    fn default_formatted_config_roundtrips_with_zero_warnings() {
        let serialized = format_config_str(&AutopilotConfig::default());
        let (cfg, warnings) = parse_config_with_warnings(&serialized).unwrap();
        assert!(warnings.is_empty(), "warnings={warnings:?}");
        assert_eq!(cfg, AutopilotConfig::default());
    }

    #[test]
    fn stale_unfocused_timer_is_evicted_when_app_closes() {
        // Probe P0-5: entry 3h old, app absent from deltas -> entry is evicted
        let db = Database::open_in_memory().unwrap();
        let cfg = AutopilotConfig::default();

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
            confidence: Confidence::Modelled,
        }];
        ap.evaluate_with_actions(&db, &snap, &active_focused_delta, &cfg, &RealSystemActions);
        assert!(!ap.unfocused_since.contains_key("ClosedApp"));
    }
}
