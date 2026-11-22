use crate::apps::{AppDeltaUsage, Confidence};
use crate::battery::{load_upower_history, BatterySnapshot, BatteryStatus};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Raw samples are kept at full resolution for this long, then downsampled to 1 per minute.
const RAW_RETENTION_SECS: i64 = 7 * 24 * 3600;
/// Gaps between awake samples longer than this mean the collector wasn't running (not sleep).
const MAX_AWAKE_GAP_SECS: i64 = 90;
/// Minimum suspended time inside an interval to count it as a sleep event.
const MIN_SLEEP_SECS: f64 = 30.0;

#[derive(Debug, Clone)]
pub struct BatteryHistoryPoint {
    pub timestamp: i64,
    pub capacity_pct: f64,
    pub status: String,
    pub power_w: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeriodDrainStats {
    pub sot_secs: i64,
    pub awake_total_secs: i64,
    pub screen_off_awake_secs: i64,
    pub sot_drain_pct: f64,
    pub sleep_secs: i64,
    pub sleep_drain_pct: f64,
    pub sleep_drain_wh: f64,
    pub sleep_drain_pct_per_hr: f64,
    pub sleep_events: usize,
    /// Seconds where the machine was powered off / rebooted between samples
    pub off_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SleepSession {
    pub start_ts: i64,
    pub end_ts: i64,
    pub start_pct: f64,
    pub end_pct: f64,
    pub start_wh: f64,
    pub end_wh: f64,
    pub on_battery: bool,
}

impl SleepSession {
    pub fn duration_secs(&self) -> i64 {
        (self.end_ts - self.start_ts).max(0)
    }
    pub fn drain_pct(&self) -> f64 {
        (self.start_pct - self.end_pct).max(0.0)
    }
    pub fn drain_pct_per_hr(&self) -> f64 {
        let hrs = self.duration_secs() as f64 / 3600.0;
        if hrs > 0.01 {
            self.drain_pct() / hrs
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BatteryHealthPoint {
    pub day_ts: i64,
    pub energy_full_wh: f64,
    pub energy_design_wh: f64,
    pub health_pct: f64,
    pub cycle_count: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneReport {
    pub downsampled_rows: usize,
    pub deleted_battery_rows: usize,
    pub deleted_app_rows: usize,
    pub vacuumed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DbStats {
    pub battery_rows: i64,
    pub app_rows: i64,
    pub sleep_sessions: i64,
    pub oldest_ts: Option<i64>,
    pub file_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct AppSummaryRow {
    pub app_name: String,
    pub category: String,
    pub unit_hint: String,
    /// Total battery % points used in the selected window
    pub battery_pct: f64,
    /// Battery % points used while this app's window was actively focused in the foreground
    pub focused_battery_pct: f64,
    /// Seconds this app's window was actively focused in the foreground
    pub focused_secs: f64,
    /// Percentage share of all app battery drain in the selected window (0..100%)
    pub share_of_drain_pct: f64,
    /// Total energy in Wh used in the selected window
    pub energy_wh: f64,
    /// Total CPU seconds in the selected window
    pub cpu_secs: f64,
    /// Total active wall-clock seconds in the selected window
    pub active_secs: f64,
    /// Peak power draw (W) observed
    pub peak_power_w: f64,
    /// Current live power draw (W)
    pub live_power_w: f64,
    /// Current live CPU load (%)
    pub live_cpu_pct: f64,
    /// Memory usage (MB)
    pub memory_mb: f64,
    /// Active process/thread count
    pub pids_count: u32,
    /// Whether this app is currently frozen (deep sleep)
    pub is_frozen: bool,
    /// Whether this app's window is currently focused in the foreground
    pub is_focused: bool,
    /// Attribution confidence level (`MeasuredTotalModelledSplit`, `Modelled`, or `Estimated`)
    pub confidence: Confidence,
}

#[derive(Clone)]
struct DrainRow {
    ts: i64,
    pct: f64,
    status: String,
    wh: f64,
    screen_on: bool,
    boot_id: String,
    suspended: f64,
}

struct DrainCacheState {
    since_bucket: i64,
    last_row: Option<DrainRow>,
    stats: PeriodDrainStats,
}

pub struct Database {
    conn: Connection,
    pub path: PathBuf,
    drain_cache: std::sync::Mutex<Option<DrainCacheState>>,
    last_health_write: std::sync::Mutex<Option<(i64, f64, u32)>>,
}

pub fn default_db_path() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rustybattery");
    let _ = fs::create_dir_all(&base);
    crate::paths::set_private_dir_mode(&base);
    base.join("rustybattery.db")
}

/// Pure interval analysis used by [`Database::get_period_drain_stats`] (unit-tested).
fn analyze_drain_rows(rows: &[DrainRow]) -> PeriodDrainStats {
    let mut stats = PeriodDrainStats::default();
    fold_drain_rows(&mut stats, rows);
    stats
}

fn fold_drain_rows(stats: &mut PeriodDrainStats, rows: &[DrainRow]) {
    for w in rows.windows(2) {
        let (prev, curr) = (&w[0], &w[1]);
        let dt = curr.ts - prev.ts;
        if dt <= 0 {
            continue;
        }
        let has_clock_info = !prev.boot_id.is_empty()
            && !curr.boot_id.is_empty()
            && prev.suspended >= 0.0
            && curr.suspended >= 0.0;

        if !has_clock_info {
            // Legacy / UPower-imported rows: we can't tell sleep from "collector not running",
            // so only short gaps are counted (as awake time).
            if dt <= MAX_AWAKE_GAP_SECS {
                add_awake(stats, prev, curr, dt as f64);
            }
            continue;
        }

        if prev.boot_id != curr.boot_id {
            stats.off_secs += dt;
            continue;
        }

        let slept = (curr.suspended - prev.suspended).clamp(0.0, dt as f64);
        let awake = dt as f64 - slept;

        if slept >= MIN_SLEEP_SECS {
            stats.sleep_secs += slept.round() as i64;
            stats.sleep_events += 1;
            if prev.status == "Discharging" || curr.status == "Discharging" {
                let frac = slept / dt as f64;
                let drop_pct = (prev.pct - curr.pct).max(0.0) * frac;
                let drop_wh = (prev.wh - curr.wh).max(0.0) * frac;
                if drop_pct < 50.0 {
                    stats.sleep_drain_pct += drop_pct;
                    stats.sleep_drain_wh += drop_wh;
                }
            }
        }

        if awake > 0.0 && awake <= MAX_AWAKE_GAP_SECS as f64 {
            let frac = if dt > 0 { awake / dt as f64 } else { 1.0 };
            add_awake_scaled(stats, prev, curr, awake, frac);
        }
    }

    if stats.sleep_secs >= 60 {
        stats.sleep_drain_pct_per_hr = stats.sleep_drain_pct / (stats.sleep_secs as f64 / 3600.0);
    } else {
        stats.sleep_drain_pct_per_hr = 0.0;
    }
}

fn add_awake(stats: &mut PeriodDrainStats, prev: &DrainRow, curr: &DrainRow, secs: f64) {
    add_awake_scaled(stats, prev, curr, secs, 1.0);
}

fn add_awake_scaled(
    stats: &mut PeriodDrainStats,
    prev: &DrainRow,
    curr: &DrainRow,
    secs: f64,
    frac: f64,
) {
    let secs_i = secs.round() as i64;
    if prev.screen_on {
        stats.awake_total_secs += secs_i;
        if prev.status == "Discharging" {
            stats.sot_secs += secs_i;
            let drop_pct = (prev.pct - curr.pct).max(0.0) * frac;
            if drop_pct < 25.0 {
                stats.sot_drain_pct += drop_pct;
            }
        }
    } else if prev.status == "Discharging" {
        stats.screen_off_awake_secs += secs_i;
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
            crate::paths::set_private_dir_mode(parent);
        }
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let instance = Self {
            conn,
            path: path.to_path_buf(),
            drain_cache: std::sync::Mutex::new(None),
            last_health_write: std::sync::Mutex::new(None),
        };
        instance.init_schema()?;
        if path != Path::new(":memory:") {
            crate::paths::set_private_file_mode(path);
        }
        Ok(instance)
    }

    /// In-memory database (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let instance = Self {
            conn,
            path: PathBuf::from(":memory:"),
            drain_cache: std::sync::Mutex::new(None),
            last_health_write: std::sync::Mutex::new(None),
        };
        instance.init_schema()?;
        Ok(instance)
    }

    pub fn exec(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    fn init_schema(&self) -> Result<()> {
        self.exec(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS boots (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 boot_id TEXT UNIQUE NOT NULL
             );
             CREATE TABLE IF NOT EXISTS battery_samples (
                 timestamp INTEGER PRIMARY KEY,
                 capacity_pct REAL NOT NULL,
                 status TEXT NOT NULL,
                 power_w REAL NOT NULL,
                 energy_now_wh REAL NOT NULL,
                 energy_full_wh REAL NOT NULL,
                 voltage_v REAL NOT NULL,
                 screen_on INTEGER NOT NULL DEFAULT 1,
                 boot_id TEXT NOT NULL DEFAULT '',
                 suspended_secs REAL NOT NULL DEFAULT -1,
                 boot_ref INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS app_usage_buckets (
                 bucket_ts INTEGER NOT NULL,
                 app_name TEXT NOT NULL,
                 category TEXT NOT NULL,
                 unit_hint TEXT NOT NULL,
                 battery_pct REAL NOT NULL,
                 focused_battery_pct REAL NOT NULL DEFAULT 0.0,
                 focused_secs REAL NOT NULL DEFAULT 0.0,
                 energy_wh REAL NOT NULL,
                 cpu_secs REAL NOT NULL,
                 active_secs REAL NOT NULL,
                 peak_power_w REAL NOT NULL,
                 memory_mb REAL NOT NULL,
                 pids_count INTEGER NOT NULL,
                 confidence TEXT NOT NULL DEFAULT 'modelled',
                 PRIMARY KEY (bucket_ts, app_name)
             );
             CREATE INDEX IF NOT EXISTS idx_app_usage_ts ON app_usage_buckets(bucket_ts);
             CREATE TABLE IF NOT EXISTS sleep_sessions (
                 start_ts INTEGER PRIMARY KEY,
                 end_ts INTEGER NOT NULL,
                 start_pct REAL NOT NULL,
                 end_pct REAL NOT NULL,
                 start_wh REAL NOT NULL,
                 end_wh REAL NOT NULL,
                 on_battery INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS battery_health_daily (
                 day_ts INTEGER PRIMARY KEY,
                 energy_full_wh REAL NOT NULL,
                 energy_design_wh REAL NOT NULL,
                 health_pct REAL NOT NULL,
                 cycle_count INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let user_ver: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if user_ver < 1 {
            for stmt in [
                "ALTER TABLE app_usage_buckets ADD COLUMN focused_battery_pct REAL NOT NULL DEFAULT 0.0;",
                "ALTER TABLE app_usage_buckets ADD COLUMN focused_secs REAL NOT NULL DEFAULT 0.0;",
                "ALTER TABLE battery_samples ADD COLUMN screen_on INTEGER NOT NULL DEFAULT 1;",
                "ALTER TABLE battery_samples ADD COLUMN boot_id TEXT NOT NULL DEFAULT '';",
                "ALTER TABLE battery_samples ADD COLUMN suspended_secs REAL NOT NULL DEFAULT -1;",
            ] {
                let _ = self.conn.execute_batch(stmt);
            }
            let _ = self.conn.execute_batch("PRAGMA user_version = 1;");
        }
        if user_ver < 2 {
            let _ = self.conn.execute_batch(
                "PRAGMA auto_vacuum = INCREMENTAL;
                 VACUUM;
                 PRAGMA user_version = 2;",
            );
        }
        if user_ver < 3 {
            let _ = self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS battery_health_daily (
                     day_ts INTEGER PRIMARY KEY,
                     energy_full_wh REAL NOT NULL,
                     energy_design_wh REAL NOT NULL,
                     health_pct REAL NOT NULL,
                     cycle_count INTEGER NOT NULL
                 );
                 PRAGMA user_version = 3;",
            );
        }
        if user_ver < 4 {
            let _ = self.conn.execute_batch(
                "ALTER TABLE app_usage_buckets ADD COLUMN confidence TEXT NOT NULL DEFAULT 'modelled';",
            );
            let _ = self.conn.execute_batch(
                "ALTER TABLE battery_samples ADD COLUMN boot_ref INTEGER NOT NULL DEFAULT 0;",
            );
            let _ = self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS boots (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     boot_id TEXT UNIQUE NOT NULL
                 );
                 PRAGMA user_version = 4;",
            );
        }
        Ok(())
    }

    fn intern_boot_id(&self, boot_id: &str) -> i64 {
        if boot_id.is_empty() {
            return 0;
        }
        let _ = self.conn.execute(
            "INSERT OR IGNORE INTO boots (boot_id) VALUES (?1)",
            [boot_id],
        );
        self.conn
            .query_row("SELECT id FROM boots WHERE boot_id = ?1", [boot_id], |r| {
                r.get(0)
            })
            .unwrap_or(0)
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Imports the UPower battery curve once so charts have history on first launch.
    ///
    /// Per-app usage is intentionally **not** fabricated: it is only recorded from real samples.
    pub fn ensure_initial_seed(&self, current_bat: &BatterySnapshot) -> Result<()> {
        if self.get_meta("upower_imported").is_none() {
            let points = load_upower_history(current_bat.energy_full_wh);
            if !points.is_empty() {
                let tx = self.conn.unchecked_transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO battery_samples
                         (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?;
                    for p in &points {
                        let e_now = (p.capacity_pct / 100.0) * current_bat.energy_full_wh;
                        stmt.execute(params![
                            p.timestamp,
                            p.capacity_pct,
                            p.status,
                            p.power_w,
                            e_now,
                            current_bat.energy_full_wh,
                            current_bat.voltage_v
                        ])?;
                    }
                }
                tx.commit()?;
            }
            self.set_meta("upower_imported", "1")?;
        }
        Ok(())
    }

    pub fn record_battery_snapshot(&self, snap: &BatterySnapshot) -> Result<()> {
        let full_threshold = snap
            .charge_limit_pct
            .unwrap_or(95)
            .saturating_sub(2)
            .max(50) as f64;
        let _ = self.set_meta("last_full_threshold", &format!("{:.1}", full_threshold));
        if (snap.capacity_pct >= full_threshold && !snap.status.is_discharging())
            || snap.status == BatteryStatus::Full
        {
            let _ = self.set_meta("last_full_charge_ts", &snap.timestamp.to_string());
        }

        let prev_discharging = self.get_meta("last_sample_discharging").as_deref() == Some("1");
        let now_discharging = snap.status.is_discharging();
        if now_discharging && !prev_discharging {
            let prev_unplug_ts = self
                .get_meta("last_unplug_ts")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let near_full = snap.capacity_pct >= (full_threshold - 5.0).max(50.0);
            let long_since_prev_unplug = snap.timestamp - prev_unplug_ts >= 30 * 60;
            if near_full || long_since_prev_unplug {
                let _ = self.set_meta("last_unplug_ts", &snap.timestamp.to_string());
            }
        }
        let _ = self.set_meta(
            "last_sample_discharging",
            if now_discharging { "1" } else { "0" },
        );

        let boot_ref = self.intern_boot_id(&snap.boot_id);
        self.conn.execute(
            "INSERT OR IGNORE INTO battery_samples
             (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v,
              screen_on, boot_id, suspended_secs, boot_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', ?9, ?10)",
            params![
                snap.timestamp,
                snap.capacity_pct,
                snap.status.as_str(),
                snap.power_w,
                snap.energy_now_wh,
                snap.energy_full_wh,
                snap.voltage_v,
                snap.screen_on as i64,
                snap.suspended_total_secs,
                boot_ref
            ],
        )?;

        if snap.energy_full_wh > 1.0 && snap.energy_design_wh > 1.0 {
            let should_write = if let Ok(guard) = self.last_health_write.lock() {
                match *guard {
                    Some((prev_ts, prev_full, prev_cycles)) => {
                        snap.timestamp - prev_ts >= 3600
                            || (snap.energy_full_wh - prev_full).abs() > 0.05
                            || snap.cycle_count != prev_cycles
                    }
                    None => true,
                }
            } else {
                true
            };
            if should_write {
                let day_ts = (snap.timestamp / 86_400) * 86_400;
                if self
                    .conn
                    .execute(
                        "INSERT INTO battery_health_daily
                         (day_ts, energy_full_wh, energy_design_wh, health_pct, cycle_count)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(day_ts) DO UPDATE SET
                             energy_full_wh = MAX(energy_full_wh, excluded.energy_full_wh),
                             energy_design_wh = excluded.energy_design_wh,
                             health_pct = MAX(health_pct, excluded.health_pct),
                             cycle_count = MAX(cycle_count, excluded.cycle_count)",
                        params![
                            day_ts,
                            snap.energy_full_wh,
                            snap.energy_design_wh,
                            snap.health_pct,
                            snap.cycle_count as i64
                        ],
                    )
                    .is_ok()
                {
                    if let Ok(mut guard) = self.last_health_write.lock() {
                        *guard = Some((snap.timestamp, snap.energy_full_wh, snap.cycle_count));
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns daily battery health snapshots (oldest first) for degradation tracking.
    pub fn get_health_history(&self, days: i64) -> Vec<BatteryHealthPoint> {
        let cutoff = chrono::Utc::now().timestamp() - days.max(1) * 86_400;
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT day_ts, energy_full_wh, energy_design_wh, health_pct, cycle_count
             FROM battery_health_daily
             WHERE day_ts >= ?1
             ORDER BY day_ts ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([cutoff], |r| {
            Ok(BatteryHealthPoint {
                day_ts: r.get(0)?,
                energy_full_wh: r.get(1)?,
                energy_design_wh: r.get(2)?,
                health_pct: r.get(3)?,
                cycle_count: r.get::<_, i64>(4)? as u32,
            })
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn record_app_deltas(&self, deltas: &[AppDeltaUsage]) -> Result<()> {
        let now_ts = chrono::Utc::now().timestamp();
        let bucket_ts = (now_ts / 300) * 300;
        self.record_app_deltas_at(bucket_ts, deltas)
    }

    pub fn record_app_deltas_at(&self, bucket_ts: i64, deltas: &[AppDeltaUsage]) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO app_usage_buckets
                 (bucket_ts, app_name, category, unit_hint, battery_pct, focused_battery_pct, focused_secs, energy_wh, cpu_secs, active_secs, peak_power_w, memory_mb, pids_count, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(bucket_ts, app_name) DO UPDATE SET
                     battery_pct = battery_pct + excluded.battery_pct,
                     focused_battery_pct = focused_battery_pct + excluded.focused_battery_pct,
                     focused_secs = focused_secs + excluded.focused_secs,
                     energy_wh = energy_wh + excluded.energy_wh,
                     cpu_secs = cpu_secs + excluded.cpu_secs,
                     active_secs = active_secs + excluded.active_secs,
                     peak_power_w = MAX(peak_power_w, excluded.peak_power_w),
                     memory_mb = excluded.memory_mb,
                     pids_count = excluded.pids_count,
                     confidence = CASE
                         WHEN app_usage_buckets.confidence = 'estimated' OR excluded.confidence = 'estimated' THEN 'estimated'
                         WHEN app_usage_buckets.confidence = 'modelled' OR excluded.confidence = 'modelled' THEN 'modelled'
                         ELSE excluded.confidence
                     END;",
            )?;
            for d in deltas {
                if d.cpu_secs <= 0.0 && d.battery_pct <= 0.0 {
                    continue;
                }
                stmt.execute(params![
                    bucket_ts,
                    d.app_name,
                    d.category,
                    d.unit_hint,
                    d.battery_pct,
                    d.focused_battery_pct,
                    d.focused_secs,
                    d.energy_wh,
                    d.cpu_secs,
                    d.active_secs,
                    d.live_power_w,
                    d.memory_mb,
                    d.pids_count as i64,
                    d.confidence.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Timestamp the battery was last unplugged or reached full/charge-limit capacity.
    /// Falls back to the last 24 hours when no recent charge/unplug event is recorded.
    pub fn get_last_full_charge_ts(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        let fallback = now - 24 * 3600;
        let week_ago = now - 7 * 24 * 3600;

        if let Some(unplug_ts) = self
            .get_meta("last_unplug_ts")
            .and_then(|s| s.parse::<i64>().ok())
        {
            if (week_ago..=now).contains(&unplug_ts) {
                return unplug_ts;
            }
        }

        if let Some(full_ts) = self
            .get_meta("last_full_charge_ts")
            .and_then(|s| s.parse::<i64>().ok())
        {
            if full_ts >= week_ago && now - full_ts >= 60 {
                return full_ts;
            }
        }

        let full_threshold = self
            .get_meta("last_full_threshold")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(78.0);
        let ts: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(timestamp) FROM battery_samples
                 WHERE capacity_pct >= ?2 AND status != 'Discharging' AND timestamp >= ?1",
                params![week_ago, full_threshold],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        match ts {
            Some(t) if now - t >= 60 => t,
            _ => fallback,
        }
    }

    pub fn get_battery_history(&self, since_ts: i64) -> Result<Vec<BatteryHistoryPoint>> {
        let now = chrono::Utc::now().timestamp();
        let span = (now - since_ts).max(60);
        let bucket_secs = (span / 500).max(1);
        if bucket_secs <= 5 {
            let mut stmt = self.conn.prepare_cached(
                "SELECT timestamp, capacity_pct, status, power_w
                 FROM battery_samples WHERE timestamp >= ?1 ORDER BY timestamp ASC",
            )?;
            let rows = stmt.query_map([since_ts], |r| {
                Ok(BatteryHistoryPoint {
                    timestamp: r.get(0)?,
                    capacity_pct: r.get(1)?,
                    status: r.get(2)?,
                    power_w: r.get(3)?,
                })
            })?;
            return Ok(rows.filter_map(|r| r.ok()).collect());
        }

        let mut stmt = self.conn.prepare_cached(
            "SELECT
                 MAX(timestamp),
                 AVG(capacity_pct),
                 CASE
                     WHEN SUM(CASE WHEN status = 'Discharging' THEN 1 ELSE 0 END)
                          >= SUM(CASE WHEN status = 'Charging' THEN 1 ELSE 0 END)
                          AND SUM(CASE WHEN status = 'Discharging' THEN 1 ELSE 0 END)
                          >= SUM(CASE WHEN status = 'Full' THEN 1 ELSE 0 END)
                          AND SUM(CASE WHEN status = 'Discharging' THEN 1 ELSE 0 END) > 0
                         THEN 'Discharging'
                     WHEN SUM(CASE WHEN status = 'Charging' THEN 1 ELSE 0 END)
                          >= SUM(CASE WHEN status = 'Full' THEN 1 ELSE 0 END)
                          AND SUM(CASE WHEN status = 'Charging' THEN 1 ELSE 0 END) > 0
                         THEN 'Charging'
                     WHEN SUM(CASE WHEN status = 'Full' THEN 1 ELSE 0 END) > 0
                         THEN 'Full'
                     ELSE MIN(status)
                 END,
                 AVG(power_w)
             FROM battery_samples
             WHERE timestamp >= ?1
             GROUP BY timestamp / ?2
             ORDER BY MAX(timestamp) ASC",
        )?;
        let rows = stmt.query_map(params![since_ts, bucket_secs], |r| {
            Ok(BatteryHistoryPoint {
                timestamp: r.get(0)?,
                capacity_pct: r.get(1)?,
                status: r.get(2)?,
                power_w: r.get(3)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_period_drain_stats(&self, since_ts: i64) -> Result<PeriodDrainStats> {
        let since_bucket = if since_ts > 0 {
            (since_ts / 300) * 300
        } else {
            since_ts
        };

        if let Ok(mut cache) = self.drain_cache.lock() {
            if let Some(ref mut state) = *cache {
                if state.since_bucket == since_bucket {
                    if let Some(ref last_row) = state.last_row {
                        let mut stmt = self.conn.prepare_cached(
                            "SELECT s.timestamp, s.capacity_pct, s.status, s.energy_now_wh, s.screen_on,
                                    COALESCE(NULLIF(s.boot_id, ''), b.boot_id, ''), s.suspended_secs
                             FROM battery_samples s
                             LEFT JOIN boots b ON b.id = s.boot_ref
                             WHERE s.timestamp > ?1
                             ORDER BY s.timestamp ASC",
                        )?;
                        let new_rows: Vec<DrainRow> = stmt
                            .query_map([last_row.ts], |r| {
                                Ok(DrainRow {
                                    ts: r.get(0)?,
                                    pct: r.get(1)?,
                                    status: r.get(2)?,
                                    wh: r.get(3)?,
                                    screen_on: r.get::<_, i64>(4)? != 0,
                                    boot_id: r.get(5)?,
                                    suspended: r.get(6)?,
                                })
                            })?
                            .filter_map(|r| r.ok())
                            .collect();
                        if !new_rows.is_empty() {
                            let mut chunk = Vec::with_capacity(new_rows.len() + 1);
                            chunk.push(last_row.clone());
                            chunk.extend(new_rows);
                            fold_drain_rows(&mut state.stats, &chunk);
                            state.last_row = chunk.pop();
                        }
                        return Ok(state.stats.clone());
                    }
                }
            }
        }

        let mut stmt = self.conn.prepare_cached(
            "SELECT s.timestamp, s.capacity_pct, s.status, s.energy_now_wh, s.screen_on,
                    COALESCE(NULLIF(s.boot_id, ''), b.boot_id, ''), s.suspended_secs
             FROM battery_samples s
             LEFT JOIN boots b ON b.id = s.boot_ref
             WHERE s.timestamp >= ?1
             ORDER BY s.timestamp ASC",
        )?;
        let rows: Vec<DrainRow> = stmt
            .query_map([since_bucket], |r| {
                Ok(DrainRow {
                    ts: r.get(0)?,
                    pct: r.get(1)?,
                    status: r.get(2)?,
                    wh: r.get(3)?,
                    screen_on: r.get::<_, i64>(4)? != 0,
                    boot_id: r.get(5)?,
                    suspended: r.get(6)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        let stats = analyze_drain_rows(&rows);
        if let Ok(mut cache) = self.drain_cache.lock() {
            *cache = Some(DrainCacheState {
                since_bucket,
                last_row: rows.into_iter().last(),
                stats: stats.clone(),
            });
        }
        Ok(stats)
    }

    pub fn record_sleep_session(&self, s: &SleepSession) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sleep_sessions
             (start_ts, end_ts, start_pct, end_pct, start_wh, end_wh, on_battery)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.start_ts,
                s.end_ts,
                s.start_pct,
                s.end_pct,
                s.start_wh,
                s.end_wh,
                s.on_battery as i64
            ],
        )?;
        Ok(())
    }

    pub fn get_sleep_sessions(&self, limit: usize) -> Vec<SleepSession> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT start_ts, end_ts, start_pct, end_pct, start_wh, end_wh, on_battery
             FROM sleep_sessions ORDER BY start_ts DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map([limit as i64], |r| {
            Ok(SleepSession {
                start_ts: r.get(0)?,
                end_ts: r.get(1)?,
                start_pct: r.get(2)?,
                end_pct: r.get(3)?,
                start_wh: r.get(4)?,
                end_wh: r.get(5)?,
                on_battery: r.get::<_, i64>(6)? != 0,
            })
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn get_app_breakdown(
        &self,
        since_ts: i64,
        live_deltas: &[AppDeltaUsage],
    ) -> Result<Vec<AppSummaryRow>> {
        let bucket_cutoff = (since_ts / 300) * 300;
        let mut stmt = self.conn.prepare_cached(
            "SELECT app_name, MAX(category), MAX(unit_hint),
                    SUM(battery_pct), SUM(focused_battery_pct), SUM(focused_secs),
                    SUM(energy_wh), SUM(cpu_secs), SUM(active_secs),
                    MAX(peak_power_w), AVG(memory_mb), MAX(pids_count),
                    CASE
                        WHEN SUM(CASE WHEN confidence = 'estimated' THEN 1 ELSE 0 END) > 0 THEN 'estimated'
                        WHEN SUM(CASE WHEN confidence = 'modelled' THEN 1 ELSE 0 END) > 0 THEN 'modelled'
                        ELSE COALESCE(MAX(confidence), 'modelled')
                    END
             FROM app_usage_buckets
             WHERE bucket_ts >= ?1
             GROUP BY app_name
             ORDER BY SUM(battery_pct) DESC",
        )?;
        let mut rows: Vec<AppSummaryRow> = stmt
            .query_map([bucket_cutoff], |r| {
                let conf_str: String = r.get(12)?;
                Ok(AppSummaryRow {
                    app_name: r.get(0)?,
                    category: r.get(1)?,
                    unit_hint: r.get(2)?,
                    battery_pct: r.get(3)?,
                    focused_battery_pct: r.get(4)?,
                    focused_secs: r.get(5)?,
                    share_of_drain_pct: 0.0,
                    energy_wh: r.get(6)?,
                    cpu_secs: r.get(7)?,
                    active_secs: r.get(8)?,
                    peak_power_w: r.get(9)?,
                    live_power_w: 0.0,
                    live_cpu_pct: 0.0,
                    memory_mb: r.get(10)?,
                    pids_count: r.get::<_, i64>(11)? as u32,
                    is_frozen: false,
                    is_focused: false,
                    confidence: Confidence::from_str_lossy(&conf_str),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Overlay current live metrics (live_power_w, live_cpu_pct, current memory_mb, pids, frozen & focused state)
        for live in live_deltas {
            if let Some(existing) = rows.iter_mut().find(|r| r.app_name == live.app_name) {
                existing.live_power_w = live.live_power_w;
                existing.live_cpu_pct = live.live_cpu_pct;
                existing.memory_mb = live.memory_mb;
                existing.pids_count = live.pids_count;
                existing.is_frozen = live.is_frozen;
                existing.is_focused = live.is_focused;
                existing.confidence = live.confidence;
                if live.live_power_w > existing.peak_power_w {
                    existing.peak_power_w = live.live_power_w;
                }
            } else if live.live_cpu_pct > 0.05 || live.memory_mb > 10.0 || live.live_power_w > 0.05 {
                rows.push(AppSummaryRow {
                    app_name: live.app_name.clone(),
                    category: live.category.clone(),
                    unit_hint: live.unit_hint.clone(),
                    battery_pct: live.battery_pct,
                    focused_battery_pct: live.focused_battery_pct,
                    focused_secs: live.focused_secs,
                    share_of_drain_pct: 0.0,
                    energy_wh: live.energy_wh,
                    cpu_secs: live.cpu_secs,
                    active_secs: live.active_secs,
                    peak_power_w: live.live_power_w,
                    live_power_w: live.live_power_w,
                    live_cpu_pct: live.live_cpu_pct,
                    memory_mb: live.memory_mb,
                    pids_count: live.pids_count,
                    is_frozen: live.is_frozen,
                    is_focused: live.is_focused,
                    confidence: live.confidence,
                });
            }
        }

        let total_batt_pct: f64 = rows.iter().map(|r| r.battery_pct).sum();
        if total_batt_pct > 0.0 {
            for r in &mut rows {
                r.share_of_drain_pct = (r.battery_pct / total_batt_pct) * 100.0;
            }
        }
        Ok(rows)
    }

    /// Returns 5-minute bucket series `(bucket_ts, battery_pct, energy_wh)` for a specific app
    pub fn get_app_buckets(&self, app_name: &str, since_ts: i64) -> Vec<(i64, f64, f64)> {
        let bucket_cutoff = (since_ts / 300) * 300;
        let Ok(mut stmt) = self.conn.prepare_cached(
            "SELECT bucket_ts, battery_pct, energy_wh
             FROM app_usage_buckets
             WHERE app_name = ?1 AND bucket_ts >= ?2
             ORDER BY bucket_ts ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![app_name, bucket_cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Downsamples old raw samples to 1/minute and deletes data past the configured retention window.
    pub fn prune(&self, now_ts: i64) -> Result<PruneReport> {
        let retention_days = crate::autopilot::load_autopilot_config().retention_days;
        self.prune_with_retention(now_ts, retention_days)
    }

    pub fn prune_with_retention(&self, now_ts: i64, retention_days: u64) -> Result<PruneReport> {
        let raw_cutoff = now_ts - RAW_RETENTION_SECS;
        let max_cutoff = now_ts - (retention_days.clamp(7, 365) as i64) * 86_400;
        let mut report = PruneReport::default();

        let tx = self.conn.unchecked_transaction()?;
        report.deleted_battery_rows =
            tx.execute("DELETE FROM battery_samples WHERE timestamp < ?1", [max_cutoff])?;
        report.downsampled_rows = tx.execute(
            "DELETE FROM battery_samples
             WHERE timestamp < ?1
               AND timestamp NOT IN (
                   SELECT MIN(timestamp) FROM battery_samples
                   WHERE timestamp < ?1 GROUP BY timestamp / 60
               )",
            [raw_cutoff],
        )?;
        report.deleted_app_rows =
            tx.execute("DELETE FROM app_usage_buckets WHERE bucket_ts < ?1", [max_cutoff])?;
        tx.execute("DELETE FROM sleep_sessions WHERE start_ts < ?1", [max_cutoff])?;
        tx.commit()?;

        if let Ok(mut cache) = self.drain_cache.lock() {
            *cache = None;
        }

        let total = report.downsampled_rows + report.deleted_battery_rows + report.deleted_app_rows;
        if total > 20_000 {
            let _ = self.exec("PRAGMA incremental_vacuum(200);");
            report.vacuumed = true;
        }
        self.set_meta("last_prune_ts", &now_ts.to_string())?;
        Ok(report)
    }

    /// Monotonic cumulative energy (Wh) per application across all retained buckets (used for Prometheus counter `N-10`).
    pub fn get_cumulative_app_energy(&self, top: usize) -> Vec<(String, String, f64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT app_name, MAX(category), SUM(energy_wh)
             FROM app_usage_buckets
             GROUP BY app_name
             ORDER BY SUM(energy_wh) DESC
             LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map([top.max(1) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn get_latest_sample_timestamp(&self) -> Option<i64> {
        self.conn
            .query_row("SELECT MAX(timestamp) FROM battery_samples", [], |r| r.get(0))
            .ok()
            .flatten()
    }

    /// Runs [`Database::prune`] at most once per 24 hours.
    pub fn prune_if_due(&self, now_ts: i64) -> Option<PruneReport> {
        let last = self
            .get_meta("last_prune_ts")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        if now_ts - last < 24 * 3600 {
            return None;
        }
        self.prune(now_ts).ok()
    }

    /// Deletes all per-app usage history (keeps the battery curve).
    pub fn reset_app_usage(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM app_usage_buckets", [])?)
    }

    pub fn stats(&self) -> DbStats {
        let count = |sql: &str| -> i64 {
            self.conn
                .query_row(sql, [], |r| r.get(0))
                .unwrap_or(0)
        };
        DbStats {
            battery_rows: count("SELECT COUNT(*) FROM battery_samples"),
            app_rows: count("SELECT COUNT(*) FROM app_usage_buckets"),
            sleep_sessions: count("SELECT COUNT(*) FROM sleep_sessions"),
            oldest_ts: self
                .conn
                .query_row("SELECT MIN(timestamp) FROM battery_samples", [], |r| r.get(0))
                .ok()
                .flatten(),
            file_size_bytes: fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        }
    }

    pub fn schema_version(&self) -> i64 {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, pct: f64, status: &str, boot: &str, susp: f64) -> DrainRow {
        DrainRow {
            ts,
            pct,
            status: status.to_string(),
            wh: pct * 0.6,
            screen_on: true,
            boot_id: boot.to_string(),
            suspended: susp,
        }
    }

    #[test]
    fn awake_samples_count_as_screen_on_time() {
        let rows: Vec<DrainRow> = (0..13)
            .map(|i| row(i * 5, 80.0 - i as f64 * 0.01, "Discharging", "b1", 0.0))
            .collect();
        let s = analyze_drain_rows(&rows);
        assert_eq!(s.sot_secs, 60);
        assert_eq!(s.sleep_secs, 0);
    }

    #[test]
    fn suspend_gap_is_sleep_not_awake() {
        let rows = vec![
            row(0, 80.0, "Discharging", "b1", 0.0),
            // 8h gap, of which 8h - 5s was suspended
            row(8 * 3600, 76.0, "Discharging", "b1", 8.0 * 3600.0 - 5.0),
        ];
        let s = analyze_drain_rows(&rows);
        assert_eq!(s.sleep_events, 1);
        assert!((s.sleep_secs - 8 * 3600).abs() <= 5);
        assert!(s.sleep_drain_pct > 3.9 && s.sleep_drain_pct <= 4.0);
        assert!(s.sleep_drain_pct_per_hr > 0.45 && s.sleep_drain_pct_per_hr < 0.55);
        assert!(s.sot_secs <= 5);
    }

    #[test]
    fn reboot_gap_is_not_sleep() {
        let rows = vec![
            row(0, 80.0, "Discharging", "b1", 100.0),
            row(10 * 3600, 70.0, "Discharging", "b2", 0.0),
        ];
        let s = analyze_drain_rows(&rows);
        assert_eq!(s.sleep_secs, 0);
        assert_eq!(s.off_secs, 10 * 3600);
    }

    #[test]
    fn collector_downtime_is_not_sleep() {
        // Same boot, no suspend, but a 2h gap: collector simply wasn't running
        let rows = vec![
            row(0, 80.0, "Discharging", "b1", 0.0),
            row(2 * 3600, 60.0, "Discharging", "b1", 0.0),
        ];
        let s = analyze_drain_rows(&rows);
        assert_eq!(s.sleep_secs, 0);
        assert_eq!(s.sot_secs, 0);
    }

    #[test]
    fn legacy_rows_never_produce_sleep() {
        let rows = vec![
            row(0, 80.0, "Discharging", "", -1.0),
            row(5 * 3600, 70.0, "Discharging", "", -1.0),
        ];
        let s = analyze_drain_rows(&rows);
        assert_eq!(s.sleep_secs, 0);
    }

    #[test]
    fn prune_downsamples_and_deletes() {
        let db = Database::open_in_memory().unwrap();
        let now = 200 * 24 * 3600;
        let ins = |ts: i64| {
            db.conn
                .execute(
                    "INSERT INTO battery_samples (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v)
                     VALUES (?1, 50, 'Discharging', 5, 30, 60, 12)",
                    [ts],
                )
                .unwrap();
        };
        // very old (deleted), old (downsampled 12 -> 1 per minute), recent (kept)
        for i in 0..12 {
            ins(now - 100 * 24 * 3600 + i * 5);
            ins(now - 10 * 24 * 3600 + i * 5);
            ins(now - 3600 + i * 5);
        }
        let rep = db.prune_with_retention(now, 90).unwrap();
        assert_eq!(rep.deleted_battery_rows, 12);
        assert_eq!(rep.downsampled_rows, 11);
        assert_eq!(db.stats().battery_rows, 13);
    }

    #[test]
    fn sleep_session_rate() {
        let s = SleepSession {
            start_ts: 0,
            end_ts: 4 * 3600,
            start_pct: 80.0,
            end_pct: 78.0,
            start_wh: 48.0,
            end_wh: 46.8,
            on_battery: true,
        };
        assert!((s.drain_pct_per_hr() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn auto_vacuum_is_incremental() {
        let db = Database::open_in_memory().unwrap();
        let mode: i64 = db
            .conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 2, "auto_vacuum must be 2 (INCREMENTAL)");
    }

    #[test]
    fn history_bucket_status_uses_mode_not_max_string() {
        let db = Database::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp();
        let base = now - 3 * 86_400; // span > 500*5 so SQL bucketing path runs
        for (offset, status) in [
            (0, "Discharging"),
            (10, "Discharging"),
            (20, "Discharging"),
            (30, "Unknown"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO battery_samples (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v)
                     VALUES (?1, 80.0, ?2, 8.0, 48.0, 60.0, 12.0)",
                    params![base + offset, status],
                )
                .unwrap();
        }
        let hist = db.get_battery_history(base - 60).unwrap();
        assert!(!hist.is_empty());
        assert_eq!(hist[0].status, "Discharging");
    }

    #[test]
    fn daily_health_history_records_and_updates_cycles() {
        let db = Database::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp();
        let snap = BatterySnapshot {
            timestamp: now,
            device_name: "BAT0".to_string(),
            manufacturer: "Test".to_string(),
            model_name: "Test".to_string(),
            technology: "Li-ion".to_string(),
            capacity_pct: 80.0,
            capacity_int: 80,
            status: BatteryStatus::Discharging,
            power_w: 8.0,
            energy_now_wh: 44.0,
            energy_full_wh: 55.0,
            energy_design_wh: 60.0,
            health_pct: 91.7,
            voltage_v: 12.0,
            cycle_count: 142,
            ac_online: false,
            time_remaining_mins: Some(300),
            screen_on: true,
            boot_id: "b1".to_string(),
            suspended_total_secs: 0.0,
            charge_limit_pct: Some(80),
        };
        db.record_battery_snapshot(&snap).unwrap();
        let hist = db.get_health_history(7);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].cycle_count, 142);
        assert!((hist[0].health_pct - 91.7).abs() < 1e-6);
    }

    #[test]
    fn confidence_persists_and_keeps_worst_in_bucket() {
        let db = Database::open_in_memory().unwrap();
        let make_delta = |conf: Confidence| AppDeltaUsage {
            app_name: "Firefox".to_string(),
            category: "Browser".to_string(),
            unit_hint: "firefox.scope".to_string(),
            battery_pct: 1.0,
            focused_battery_pct: 0.5,
            energy_wh: 0.5,
            cpu_secs: 2.0,
            active_secs: 5.0,
            focused_secs: 2.5,
            live_power_w: 6.0,
            live_cpu_pct: 20.0,
            memory_mb: 300.0,
            pids_count: 8,
            is_frozen: false,
            is_focused: true,
            confidence: conf,
        };

        db.record_app_deltas_at(1_200, &[make_delta(Confidence::MeasuredTotalModelledSplit)])
            .unwrap();
        let rows1 = db.get_app_breakdown(900, &[]).unwrap();
        assert_eq!(rows1[0].confidence, Confidence::MeasuredTotalModelledSplit);

        db.record_app_deltas_at(1_200, &[make_delta(Confidence::Estimated)])
            .unwrap();
        let rows2 = db.get_app_breakdown(900, &[]).unwrap();
        assert_eq!(rows2[0].confidence, Confidence::Estimated);
    }

    #[test]
    fn incremental_drain_stats_matches_full_recompute() {
        let db = Database::open_in_memory().unwrap();
        let base = 1_700_000_000 / 300 * 300;
        let mk_snap = |ts: i64, pct: f64, susp: f64| BatterySnapshot {
            timestamp: ts,
            device_name: "BAT0".to_string(),
            manufacturer: "Test".to_string(),
            model_name: "Test".to_string(),
            technology: "Li-ion".to_string(),
            capacity_pct: pct,
            capacity_int: pct.round() as u8,
            status: BatteryStatus::Discharging,
            power_w: 8.0,
            energy_now_wh: pct * 0.6,
            energy_full_wh: 60.0,
            energy_design_wh: 60.0,
            health_pct: 100.0,
            voltage_v: 12.0,
            cycle_count: 10,
            ac_online: false,
            time_remaining_mins: Some(200),
            screen_on: true,
            boot_id: "boot-uuid-1234".to_string(),
            suspended_total_secs: susp,
            charge_limit_pct: None,
        };

        db.record_battery_snapshot(&mk_snap(base, 80.0, 0.0)).unwrap();
        db.record_battery_snapshot(&mk_snap(base + 30, 79.9, 0.0)).unwrap();
        db.record_battery_snapshot(&mk_snap(base + 60, 79.8, 0.0)).unwrap();
        let s1 = db.get_period_drain_stats(base).unwrap();
        assert_eq!(s1.sot_secs, 60);

        // Append awake + suspend samples and test incremental update vs fresh recompute
        db.record_battery_snapshot(&mk_snap(base + 90, 79.7, 0.0)).unwrap();
        db.record_battery_snapshot(&mk_snap(base + 3690, 78.7, 3595.0))
            .unwrap();
        let s2_inc = db.get_period_drain_stats(base).unwrap();

        let fresh = Database::open_in_memory().unwrap();
        for s in [
            mk_snap(base, 80.0, 0.0),
            mk_snap(base + 30, 79.9, 0.0),
            mk_snap(base + 60, 79.8, 0.0),
            mk_snap(base + 90, 79.7, 0.0),
            mk_snap(base + 3690, 78.7, 3595.0),
        ] {
            fresh.record_battery_snapshot(&s).unwrap();
        }
        let s2_full = fresh.get_period_drain_stats(base).unwrap();
        assert_eq!(s2_inc.sot_secs, s2_full.sot_secs);
        assert_eq!(s2_inc.sleep_secs, s2_full.sleep_secs);
        assert_eq!(s2_inc.sleep_events, s2_full.sleep_events);
        assert!((s2_inc.sleep_drain_pct - s2_full.sleep_drain_pct).abs() < 1e-6);
    }
}
