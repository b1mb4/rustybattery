use crate::apps::AppDeltaUsage;
use crate::battery::{load_upower_history, BatterySnapshot, BatteryStatus};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Raw samples are kept at full resolution for this long, then downsampled to 1 per minute.
const RAW_RETENTION_SECS: i64 = 7 * 24 * 3600;
/// Anything older than this is deleted entirely.
const MAX_RETENTION_SECS: i64 = 90 * 24 * 3600;
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
    /// Attribution confidence level (`"measured"`, `"modelled"`, `"estimated"`)
    pub confidence: String,
}

pub struct Database {
    conn: Connection,
    pub path: PathBuf,
    drain_cache: std::sync::Mutex<Option<(i64, i64, PeriodDrainStats)>>,
}

pub fn default_db_path() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rustybattery");
    let _ = fs::create_dir_all(&base);
    base.join("rustybattery.db")
}

struct DrainRow {
    ts: i64,
    pct: f64,
    status: String,
    wh: f64,
    screen_on: bool,
    boot_id: String,
    suspended: f64,
}

/// Pure interval analysis used by [`Database::get_period_drain_stats`] (unit-tested).
fn analyze_drain_rows(rows: &[DrainRow]) -> PeriodDrainStats {
    let mut stats = PeriodDrainStats::default();
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
                add_awake(&mut stats, prev, curr, dt as f64);
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
            add_awake_scaled(&mut stats, prev, curr, awake, frac);
        }
    }

    if stats.sleep_secs >= 60 {
        stats.sleep_drain_pct_per_hr = stats.sleep_drain_pct / (stats.sleep_secs as f64 / 3600.0);
    }
    stats
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
        }
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let instance = Self {
            conn,
            path: path.to_path_buf(),
            drain_cache: std::sync::Mutex::new(None),
        };
        instance.init_schema()?;
        Ok(instance)
    }

    /// In-memory database (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let instance = Self {
            conn,
            path: PathBuf::from(":memory:"),
            drain_cache: std::sync::Mutex::new(None),
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
                 suspended_secs REAL NOT NULL DEFAULT -1
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
        Ok(())
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

        self.conn.execute(
            "INSERT OR IGNORE INTO battery_samples
             (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v,
              screen_on, boot_id, suspended_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                snap.timestamp,
                snap.capacity_pct,
                snap.status.as_str(),
                snap.power_w,
                snap.energy_now_wh,
                snap.energy_full_wh,
                snap.voltage_v,
                snap.screen_on as i64,
                snap.boot_id,
                snap.suspended_total_secs
            ],
        )?;
        Ok(())
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
                 (bucket_ts, app_name, category, unit_hint, battery_pct, focused_battery_pct, focused_secs, energy_wh, cpu_secs, active_secs, peak_power_w, memory_mb, pids_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(bucket_ts, app_name) DO UPDATE SET
                     battery_pct = battery_pct + excluded.battery_pct,
                     focused_battery_pct = focused_battery_pct + excluded.focused_battery_pct,
                     focused_secs = focused_secs + excluded.focused_secs,
                     energy_wh = energy_wh + excluded.energy_wh,
                     cpu_secs = cpu_secs + excluded.cpu_secs,
                     active_secs = active_secs + excluded.active_secs,
                     peak_power_w = MAX(peak_power_w, excluded.peak_power_w),
                     memory_mb = excluded.memory_mb,
                     pids_count = excluded.pids_count;",
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
                    d.pids_count as i64
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
            if unplug_ts >= week_ago && unplug_ts <= now {
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
            "SELECT MAX(timestamp), AVG(capacity_pct), MAX(status), AVG(power_w)
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
        let max_ts: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(timestamp), 0) FROM battery_samples WHERE timestamp >= ?1",
                [since_ts],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if let Ok(cache) = self.drain_cache.lock() {
            if let Some((cached_since, cached_max, ref stats)) = *cache {
                if cached_since == since_ts && cached_max == max_ts {
                    return Ok(stats.clone());
                }
            }
        }

        let mut stmt = self.conn.prepare_cached(
            "SELECT timestamp, capacity_pct, status, energy_now_wh, screen_on, boot_id, suspended_secs
             FROM battery_samples WHERE timestamp >= ?1 ORDER BY timestamp ASC",
        )?;
        let rows: Vec<DrainRow> = stmt
            .query_map([since_ts], |r| {
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
            *cache = Some((since_ts, max_ts, stats.clone()));
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
                    MAX(peak_power_w), AVG(memory_mb), MAX(pids_count)
             FROM app_usage_buckets
             WHERE bucket_ts >= ?1
             GROUP BY app_name
             ORDER BY SUM(battery_pct) DESC",
        )?;
        let mut rows: Vec<AppSummaryRow> = stmt
            .query_map([bucket_cutoff], |r| {
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
                    confidence: "modelled".to_string(),
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
                existing.confidence = live.confidence.clone();
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
                    confidence: live.confidence.clone(),
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

    /// Downsamples old raw samples to 1/minute and deletes data past the retention window.
    pub fn prune(&self, now_ts: i64) -> Result<PruneReport> {
        let raw_cutoff = now_ts - RAW_RETENTION_SECS;
        let max_cutoff = now_ts - MAX_RETENTION_SECS;
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

        let total = report.downsampled_rows + report.deleted_battery_rows + report.deleted_app_rows;
        if total > 20_000 {
            let _ = self.exec("PRAGMA incremental_vacuum(200);");
            report.vacuumed = true;
        }
        self.set_meta("last_prune_ts", &now_ts.to_string())?;
        Ok(report)
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
        let rep = db.prune(now).unwrap();
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
}
