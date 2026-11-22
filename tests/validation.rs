use anyhow::Result;
use rustybattery::apps::{parse_user_app_rules_str, AppDeltaUsage, Confidence, Focus};
use rustybattery::autopilot::{
    parse_config_with_warnings, AutopilotConfig, SmartAutopilot, SystemActions,
};
use rustybattery::battery::{
    read_battery_snapshot_with_paths, read_rapl_package_energy_uj_in, BatterySnapshot,
    BatteryStatus,
};
use rustybattery::db::Database;
use rustybattery::guards::{pactl_output_has_active_stream_for_pids, parse_playerctl_playing_for_app};
use rustybattery::paths::Paths;
use rustybattery::report::parse_wakeup_sources_str;
use rustybattery::trace::replay_trace_str;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn unique_temp_dir(tag: &str) -> PathBuf {
    let uniq = format!(
        "rb_val_{tag}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let p = std::env::temp_dir().join(uniq);
    fs::create_dir_all(&p).unwrap();
    p
}

fn fake_bat_snapshot(capacity_pct: f64, ac_online: bool, status: BatteryStatus) -> BatterySnapshot {
    BatterySnapshot {
        timestamp: 1_700_000_000,
        device_name: "BAT0".to_string(),
        manufacturer: "Test".to_string(),
        model_name: "Model".to_string(),
        technology: "Li-ion".to_string(),
        capacity_pct,
        capacity_int: capacity_pct.round() as u8,
        status,
        power_w: if ac_online { 0.0 } else { 12.0 },
        energy_now_wh: 45.0,
        energy_full_wh: 60.0,
        energy_design_wh: 65.0,
        health_pct: 92.3,
        voltage_v: 11.4,
        cycle_count: 10,
        ac_online,
        time_remaining_mins: Some(225),
        screen_on: true,
        boot_id: "boot-test".to_string(),
        suspended_total_secs: 0.0,
        charge_limit_pct: Some(80),
    }
}

fn fake_app_delta(app_name: &str, category: &str, live_power_w: f64) -> AppDeltaUsage {
    AppDeltaUsage {
        app_name: app_name.to_string(),
        category: category.to_string(),
        unit_hint: format!("{app_name}.service"),
        battery_pct: 1.0,
        focused_battery_pct: 0.0,
        energy_wh: 0.5,
        cpu_secs: 1.0,
        active_secs: 5.0,
        focused_secs: 0.0,
        live_power_w,
        live_cpu_pct: 10.0,
        memory_mb: 256.0,
        pids_count: 2,
        is_frozen: false,
        is_focused: false,
        confidence: Confidence::Modelled,
    }
}

struct MockActions {
    focus: RefCell<Focus>,
    guarded: RefCell<HashMap<String, &'static str>>,
    lid_closed: Cell<bool>,
    bt_unblocked: Cell<bool>,
    freeze_calls: RefCell<Vec<(String, bool)>>,
    powersave_calls: Cell<usize>,
    restore_profile_calls: Cell<usize>,
    dim_kbd_calls: Cell<usize>,
    restore_kbd_calls: Cell<usize>,
    bt_block_calls: RefCell<Vec<bool>>,
}

impl MockActions {
    fn new(focus: Focus) -> Self {
        Self {
            focus: RefCell::new(focus),
            guarded: RefCell::new(HashMap::new()),
            lid_closed: Cell::new(false),
            bt_unblocked: Cell::new(false),
            freeze_calls: RefCell::new(Vec::new()),
            powersave_calls: Cell::new(0),
            restore_profile_calls: Cell::new(0),
            dim_kbd_calls: Cell::new(0),
            restore_kbd_calls: Cell::new(0),
            bt_block_calls: RefCell::new(Vec::new()),
        }
    }
}

impl SystemActions for MockActions {
    fn detect_focus(&self) -> Focus {
        self.focus.borrow().clone()
    }
    fn set_app_frozen(&self, app_name: &str, freeze: bool) -> Result<String> {
        self.freeze_calls
            .borrow_mut()
            .push((app_name.to_string(), freeze));
        Ok("ok".to_string())
    }
    fn check_freeze_guards(&self, app_name: &str) -> Option<&'static str> {
        self.guarded.borrow().get(app_name).copied()
    }
    fn save_and_switch_powersave(&self, _db: &Database) -> Result<String> {
        self.powersave_calls.set(self.powersave_calls.get() + 1);
        Ok("power-saver".to_string())
    }
    fn restore_saved_power_profile(&self, _db: &Database) -> Option<String> {
        self.restore_profile_calls
            .set(self.restore_profile_calls.get() + 1);
        Some("balanced".to_string())
    }
    fn dim_and_save_kbd_backlight(&self, _db: &Database) -> bool {
        self.dim_kbd_calls.set(self.dim_kbd_calls.get() + 1);
        true
    }
    fn restore_saved_kbd_backlight(&self, _db: &Database) -> bool {
        self.restore_kbd_calls
            .set(self.restore_kbd_calls.get() + 1);
        true
    }
    fn is_lid_closed(&self) -> bool {
        self.lid_closed.get()
    }
    fn should_skip_bluetooth_block(&self) -> bool {
        false
    }
    fn is_bluetooth_unblocked(&self) -> bool {
        self.bt_unblocked.get()
    }
    fn set_bluetooth_blocked(&self, block: bool) -> bool {
        self.bt_block_calls.borrow_mut().push(block);
        self.bt_unblocked.set(!block);
        true
    }
    fn thaw_owned(&self) -> usize {
        0
    }
    fn trigger_critical_stop(&self, _db: &Database, _snap: &BatterySnapshot) {}
}

#[test]
fn replay_trace_satisfies_energy_conservation_and_idle_invariants() {
    let jsonl = include_str!("fixtures/sample_trace.jsonl");
    let summary = replay_trace_str(jsonl, 12.0, 1.0, 0.8).unwrap();

    assert_eq!(summary.frames, 4);
    assert!(
        (summary.total_attributed_wh - summary.total_battery_wh).abs() < 1e-6,
        "attributed={} != battery={}",
        summary.total_attributed_wh,
        summary.total_battery_wh
    );
    assert!(summary.per_app_wh["Background Daemon"] < 0.001);
    assert!(summary.per_app_wh["Compiler"] > summary.per_app_wh["Firefox"]);
    assert!(summary.per_app_wh["Firefox"] > 0.005);
    assert!(summary.display_wh > 0.01);
    assert!(summary.unattributed_wh > 0.01);

    assert_eq!(
        summary.per_app_confidence["Compiler"],
        Confidence::MeasuredTotalModelledSplit
    );
    assert_eq!(
        summary.per_app_confidence["Display & Backlight (est.)"],
        Confidence::Estimated
    );
    assert_eq!(
        summary.per_app_confidence["Unattributed / Hardware & Idle"],
        Confidence::Estimated
    );
}

#[test]
fn paths_reads_fake_sysfs_battery_and_multi_package_rapl() {
    let root = unique_temp_dir("sysfs");
    let bat0 = root.join("sys/class/power_supply/BAT0");
    let ac0 = root.join("sys/class/power_supply/AC0");
    let rapl0 = root.join("sys/class/powercap/intel-rapl:0");
    let rapl1 = root.join("sys/class/powercap/intel-rapl:1");
    let rapl0_sub = root.join("sys/class/powercap/intel-rapl:0:0");
    for d in [&bat0, &ac0, &rapl0, &rapl1, &rapl0_sub] {
        fs::create_dir_all(d).unwrap();
    }

    fs::write(bat0.join("type"), "Battery\n").unwrap();
    fs::write(bat0.join("status"), "Discharging\n").unwrap();
    fs::write(bat0.join("capacity"), "72\n").unwrap();
    fs::write(bat0.join("energy_now"), "43200000\n").unwrap();
    fs::write(bat0.join("energy_full"), "60000000\n").unwrap();
    fs::write(bat0.join("energy_full_design"), "65000000\n").unwrap();
    fs::write(bat0.join("power_now"), "12500000\n").unwrap();
    fs::write(bat0.join("voltage_now"), "11400000\n").unwrap();
    fs::write(bat0.join("cycle_count"), "42\n").unwrap();
    fs::write(bat0.join("charge_control_end_threshold"), "80\n").unwrap();

    fs::write(ac0.join("type"), "Mains\n").unwrap();
    fs::write(ac0.join("online"), "0\n").unwrap();

    fs::write(rapl0.join("name"), "package-0\n").unwrap();
    fs::write(rapl0.join("energy_uj"), "150000000\n").unwrap();
    fs::write(rapl1.join("name"), "package-1\n").unwrap();
    fs::write(rapl1.join("energy_uj"), "50000000\n").unwrap();
    fs::write(rapl0_sub.join("name"), "core\n").unwrap();
    fs::write(rapl0_sub.join("energy_uj"), "999999999\n").unwrap();

    let paths = Paths::from_root(&root);
    let snap = read_battery_snapshot_with_paths(&paths).unwrap();
    let rapl = read_rapl_package_energy_uj_in(&paths);
    let _ = fs::remove_dir_all(&root);

    assert!((snap.capacity_pct - 72.0).abs() < 0.01);
    assert!((snap.power_w - 12.5).abs() < 0.01);
    assert_eq!(snap.charge_limit_pct, Some(80));
    assert!(!snap.ac_online);
    assert_eq!(rapl, Some(200_000_000));
}

#[test]
fn autopilot_mock_focus_unknown_never_freezes_and_thaws() {
    let dir = unique_temp_dir("ap_unknown");
    let db = Database::open(&dir.join("test.db")).unwrap();

    let mut ap = SmartAutopilot::new();
    ap.auto_frozen_apps.insert("Obsidian".to_string());
    ap.unfocused_since.insert(
        "Firefox".to_string(),
        Instant::now() - Duration::from_secs(700),
    );

    let cfg = AutopilotConfig {
        auto_freeze_enabled: true,
        notifications_enabled: false,
        ..AutopilotConfig::default()
    };
    let snap = fake_bat_snapshot(80.0, false, BatteryStatus::Discharging);
    let deltas = vec![fake_app_delta("Firefox", "Browser", 4.0)];
    let actions = MockActions::new(Focus::Unknown);

    ap.evaluate_with_actions(&db, &snap, &deltas, &cfg, &actions);
    let calls = actions.freeze_calls.borrow().clone();
    drop(db);
    let _ = fs::remove_dir_all(&dir);

    assert!(calls.contains(&("Obsidian".to_string(), false)));
    assert!(!calls.iter().any(|(_, freeze)| *freeze));
}

#[test]
fn autopilot_mock_guarded_app_is_never_frozen() {
    let dir = unique_temp_dir("ap_guard");
    let db = Database::open(&dir.join("test.db")).unwrap();

    let mut ap = SmartAutopilot::new();
    let old = Instant::now() - Duration::from_secs(700);
    ap.unfocused_since.insert("Firefox".to_string(), old);
    ap.unfocused_since.insert("Obsidian".to_string(), old);

    let cfg = AutopilotConfig {
        auto_freeze_enabled: true,
        notifications_enabled: false,
        ..AutopilotConfig::default()
    };
    let snap = fake_bat_snapshot(75.0, false, BatteryStatus::Discharging);
    let deltas = vec![
        fake_app_delta("Firefox", "Browser", 3.5),
        fake_app_delta("Obsidian", "App", 3.5),
    ];

    let mut focused = HashSet::new();
    focused.insert("Terminal".to_string());
    let actions = MockActions::new(Focus::Known(focused));
    actions
        .guarded
        .borrow_mut()
        .insert("Firefox".to_string(), "active audio");

    ap.evaluate_with_actions(&db, &snap, &deltas, &cfg, &actions);
    let calls = actions.freeze_calls.borrow().clone();
    drop(db);
    let _ = fs::remove_dir_all(&dir);

    assert!(!calls.contains(&("Firefox".to_string(), true)));
    assert!(calls.contains(&("Obsidian".to_string(), true)));
}

#[test]
fn autopilot_mock_lid_open_on_ac_restores_profile_kbd_and_bt() {
    let dir = unique_temp_dir("ap_lid");
    let db = Database::open(&dir.join("test.db")).unwrap();

    let mut ap = SmartAutopilot::new();
    let cfg = AutopilotConfig {
        s2idle_sleep_saver: true,
        notifications_enabled: false,
        ..AutopilotConfig::default()
    };

    let actions = MockActions::new(Focus::Known(HashSet::new()));
    actions.lid_closed.set(true);
    actions.bt_unblocked.set(true);

    let snap_batt_closed = fake_bat_snapshot(60.0, false, BatteryStatus::Discharging);
    ap.evaluate_with_actions(&db, &snap_batt_closed, &[], &cfg, &actions);
    assert_eq!(actions.powersave_calls.get(), 1);
    assert_eq!(actions.dim_kbd_calls.get(), 1);
    assert_eq!(actions.bt_block_calls.borrow().as_slice(), &[true]);

    actions.lid_closed.set(false);
    let snap_ac_open = fake_bat_snapshot(62.0, true, BatteryStatus::Charging);
    ap.evaluate_with_actions(&db, &snap_ac_open, &[], &cfg, &actions);

    let bt_calls = actions.bt_block_calls.borrow().clone();
    let restore_prof = actions.restore_profile_calls.get();
    let restore_kbd = actions.restore_kbd_calls.get();
    drop(db);
    let _ = fs::remove_dir_all(&dir);

    assert!(restore_prof >= 1);
    assert!(restore_kbd >= 1);
    assert_eq!(bt_calls, vec![true, false]);
}

#[test]
fn parsers_never_panic_on_malformed_or_garbage_input() {
    let pids = HashSet::new();
    for bad in [
        "",
        "\0\0\0",
        "[[[[unclosed_toml",
        "enabled = \"not_a_bool\"",
        "{\"truncated\":",
        "\t\t\n\n\t",
    ] {
        let _ = parse_config_with_warnings(bad);
        let _ = parse_user_app_rules_str(bad);
        assert!(parse_wakeup_sources_str(bad).is_empty());
        assert!(!pactl_output_has_active_stream_for_pids(bad, &pids, "App", true));
        assert!(!parse_playerctl_playing_for_app(bad, "App"));
        let _ = replay_trace_str(bad, 12.0, 1.0, 0.8);
    }
    assert!(replay_trace_str("{bad json}", 12.0, 1.0, 0.8).is_err());
}
