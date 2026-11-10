use crate::apps::{bootstrap_session_app_usage, AppDeltaUsage};
use crate::battery::{load_upower_history, BatterySnapshot, BatteryStatus};
use anyhow::{bail, Context, Result};
use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_TRANSIENT: isize = -1;

#[link(name = "sqlite3")]
extern "C" {
    fn sqlite3_open(filename: *const c_char, ppDb: *mut *mut sqlite3) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Option<extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        zSql: *const c_char,
        nByte: c_int,
        ppStmt: *mut *mut sqlite3_stmt,
        pzTail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;

    fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int;
    fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, idx: c_int, val: c_double) -> c_int;
    fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: c_int,
        destructor: *const c_void,
    ) -> c_int;

    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, iCol: c_int) -> i64;
    fn sqlite3_column_double(stmt: *mut sqlite3_stmt, iCol: c_int) -> c_double;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, iCol: c_int) -> *const c_char;
}

#[derive(Debug, Clone)]
pub struct BatteryHistoryPoint {
    pub timestamp: i64,
    pub capacity_pct: f64,
    pub status: String,
    pub power_w: f64,
}

#[derive(Debug, Clone)]
pub struct AppSummaryRow {
    pub app_name: String,
    pub category: String,
    pub unit_hint: String,
    /// Total battery % points used in the selected window
    pub battery_pct: f64,
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
}

pub struct Database {
    db: *mut sqlite3,
    pub path: PathBuf,
}

unsafe impl Send for Database {}

impl Drop for Database {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe {
                sqlite3_close(self.db);
            }
        }
    }
}

pub fn default_db_path() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rustybattery");
    let _ = fs::create_dir_all(&base);
    base.join("rustybattery.db")
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let c_path = CString::new(path.to_string_lossy().as_bytes())?;
        let mut db: *mut sqlite3 = ptr::null_mut();
        let rc = unsafe { sqlite3_open(c_path.as_ptr(), &mut db) };
        if rc != SQLITE_OK || db.is_null() {
            bail!("Failed to open SQLite database at {}", path.display());
        }
        unsafe {
            sqlite3_busy_timeout(db, 5000);
        }
        let instance = Self {
            db,
            path: path.to_path_buf(),
        };
        instance.init_schema()?;
        Ok(instance)
    }

    fn last_error(&self) -> String {
        unsafe {
            let ptr = sqlite3_errmsg(self.db);
            if ptr.is_null() {
                "Unknown SQLite error".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().to_string()
            }
        }
    }

    pub fn exec(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql)?;
        let rc = unsafe {
            sqlite3_exec(
                self.db,
                c_sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            bail!("SQLite exec error ({}): {}", rc, self.last_error());
        }
        Ok(())
    }

    fn init_schema(&self) -> Result<()> {
        self.exec(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS battery_samples (
                 timestamp INTEGER PRIMARY KEY,
                 capacity_pct REAL NOT NULL,
                 status TEXT NOT NULL,
                 power_w REAL NOT NULL,
                 energy_now_wh REAL NOT NULL,
                 energy_full_wh REAL NOT NULL,
                 voltage_v REAL NOT NULL
             );
             CREATE TABLE IF NOT EXISTS app_usage_buckets (
                 bucket_ts INTEGER NOT NULL,
                 app_name TEXT NOT NULL,
                 category TEXT NOT NULL,
                 unit_hint TEXT NOT NULL,
                 battery_pct REAL NOT NULL,
                 energy_wh REAL NOT NULL,
                 cpu_secs REAL NOT NULL,
                 active_secs REAL NOT NULL,
                 peak_power_w REAL NOT NULL,
                 memory_mb REAL NOT NULL,
                 pids_count INTEGER NOT NULL,
                 PRIMARY KEY (bucket_ts, app_name)
             );
             CREATE INDEX IF NOT EXISTS idx_app_usage_ts ON app_usage_buckets(bucket_ts);
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let sql = CString::new("SELECT value FROM meta WHERE key = ?1").ok()?;
        let c_key = CString::new(key).ok()?;
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK
            {
                return None;
            }
            sqlite3_bind_text(
                stmt,
                1,
                c_key.as_ptr(),
                -1,
                SQLITE_TRANSIENT as *const c_void,
            );
            let res = if sqlite3_step(stmt) == SQLITE_ROW {
                let p = sqlite3_column_text(stmt, 0);
                if p.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(p).to_string_lossy().to_string())
                }
            } else {
                None
            };
            sqlite3_finalize(stmt);
            res
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let sql = CString::new("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)")?;
        let c_key = CString::new(key)?;
        let c_val = CString::new(value)?;
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK
            {
                bail!("Prepare failed: {}", self.last_error());
            }
            sqlite3_bind_text(
                stmt,
                1,
                c_key.as_ptr(),
                -1,
                SQLITE_TRANSIENT as *const c_void,
            );
            sqlite3_bind_text(
                stmt,
                2,
                c_val.as_ptr(),
                -1,
                SQLITE_TRANSIENT as *const c_void,
            );
            sqlite3_step(stmt);
            sqlite3_finalize(stmt);
        }
        Ok(())
    }

    /// Seeds initial battery history from UPower and initial session app usage from cgroup v2 `cpu.stat`
    /// so the user immediately has rich historical charts and per-app attribution on first launch.
    pub fn ensure_initial_seed(&self, current_bat: &BatterySnapshot) -> Result<()> {
        // 1. Import UPower history once
        if self.get_meta("upower_imported").is_none() {
            let points = load_upower_history(current_bat.energy_full_wh);
            if !points.is_empty() {
                self.exec("BEGIN TRANSACTION;")?;
                let sql = CString::new(
                    "INSERT OR IGNORE INTO battery_samples
                     (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                unsafe {
                    if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
                        == SQLITE_OK
                    {
                        for p in &points {
                            let c_status =
                                CString::new(p.status.as_str()).unwrap_or_default();
                            let e_now = (p.capacity_pct / 100.0) * current_bat.energy_full_wh;
                            sqlite3_bind_int64(stmt, 1, p.timestamp);
                            sqlite3_bind_double(stmt, 2, p.capacity_pct);
                            sqlite3_bind_text(
                                stmt,
                                3,
                                c_status.as_ptr(),
                                -1,
                                SQLITE_TRANSIENT as *const c_void,
                            );
                            sqlite3_bind_double(stmt, 4, p.power_w);
                            sqlite3_bind_double(stmt, 5, e_now);
                            sqlite3_bind_double(stmt, 6, current_bat.energy_full_wh);
                            sqlite3_bind_double(stmt, 7, current_bat.voltage_v);
                            sqlite3_step(stmt);
                            sqlite3_prepare_v2(
                                self.db,
                                sql.as_ptr(),
                                -1,
                                &mut stmt,
                                ptr::null_mut(),
                            );
                        }
                        sqlite3_finalize(stmt);
                    }
                }
                self.exec("COMMIT;")?;
            }
            self.set_meta("upower_imported", "1")?;
        }

        // 2. Seed current boot session per-app usage if not yet seeded for this boot_id
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap_or_else(|_| "default-boot".to_string())
            .trim()
            .to_string();

        if self.get_meta("seeded_boot_id").as_deref() != Some(boot_id.as_str()) {
            let uptime_secs = fs::read_to_string("/proc/uptime")
                .ok()
                .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
                .unwrap_or(3600.0);
            let boot_ts = current_bat.timestamp - (uptime_secs as i64);

            // Calculate how much battery % discharged during this boot session
            let discharged_since_boot = self
                .compute_discharged_pct_since(boot_ts, current_bat.capacity_pct)
                .max((100.0 - current_bat.capacity_pct).clamp(5.0, 65.0));

            let boot_apps = bootstrap_session_app_usage(
                discharged_since_boot,
                current_bat.energy_full_wh,
                current_bat.power_w,
            );

            // Distribute across a few 5-minute buckets between boot_ts and now so hourly/session views look natural
            let bucket_ts = ((current_bat.timestamp - 300) / 300) * 300;
            self.record_app_deltas_at(bucket_ts, &boot_apps)?;
            self.set_meta("seeded_boot_id", &boot_id)?;
        }

        Ok(())
    }

    pub fn compute_discharged_pct_since(&self, since_ts: i64, current_pct: f64) -> f64 {
        let pts = self.get_battery_history(since_ts).unwrap_or_default();
        if pts.len() < 2 {
            return (100.0 - current_pct).max(0.0);
        }
        let mut total_drop = 0.0;
        for w in pts.windows(2) {
            let diff = w[0].capacity_pct - w[1].capacity_pct;
            if diff > 0.0 && diff < 25.0 {
                total_drop += diff;
            }
        }
        total_drop
    }

    pub fn record_battery_snapshot(&self, snap: &BatterySnapshot) -> Result<()> {
        // Track last time battery was >= 95% or Full
        if snap.capacity_pct >= 95.0 || snap.status == BatteryStatus::Full {
            let _ = self.set_meta("last_full_charge_ts", &snap.timestamp.to_string());
        }

        let sql = CString::new(
            "INSERT OR REPLACE INTO battery_samples
             (timestamp, capacity_pct, status, power_w, energy_now_wh, energy_full_wh, voltage_v)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let c_status = CString::new(snap.status.as_str())?;
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK
            {
                bail!("Prepare failed: {}", self.last_error());
            }
            sqlite3_bind_int64(stmt, 1, snap.timestamp);
            sqlite3_bind_double(stmt, 2, snap.capacity_pct);
            sqlite3_bind_text(
                stmt,
                3,
                c_status.as_ptr(),
                -1,
                SQLITE_TRANSIENT as *const c_void,
            );
            sqlite3_bind_double(stmt, 4, snap.power_w);
            sqlite3_bind_double(stmt, 5, snap.energy_now_wh);
            sqlite3_bind_double(stmt, 6, snap.energy_full_wh);
            sqlite3_bind_double(stmt, 7, snap.voltage_v);
            sqlite3_step(stmt);
            sqlite3_finalize(stmt);
        }
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
        self.exec("BEGIN TRANSACTION;")?;
        let sql = CString::new(
            "INSERT INTO app_usage_buckets
             (bucket_ts, app_name, category, unit_hint, battery_pct, energy_wh, cpu_secs, active_secs, peak_power_w, memory_mb, pids_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(bucket_ts, app_name) DO UPDATE SET
                 battery_pct = battery_pct + excluded.battery_pct,
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
            let c_name = CString::new(d.app_name.as_str()).unwrap_or_default();
            let c_cat = CString::new(d.category.as_str()).unwrap_or_default();
            let c_unit = CString::new(d.unit_hint.as_str()).unwrap_or_default();

            let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
            unsafe {
                if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
                    == SQLITE_OK
                {
                    sqlite3_bind_int64(stmt, 1, bucket_ts);
                    sqlite3_bind_text(
                        stmt,
                        2,
                        c_name.as_ptr(),
                        -1,
                        SQLITE_TRANSIENT as *const c_void,
                    );
                    sqlite3_bind_text(
                        stmt,
                        3,
                        c_cat.as_ptr(),
                        -1,
                        SQLITE_TRANSIENT as *const c_void,
                    );
                    sqlite3_bind_text(
                        stmt,
                        4,
                        c_unit.as_ptr(),
                        -1,
                        SQLITE_TRANSIENT as *const c_void,
                    );
                    sqlite3_bind_double(stmt, 5, d.battery_pct);
                    sqlite3_bind_double(stmt, 6, d.energy_wh);
                    sqlite3_bind_double(stmt, 7, d.cpu_secs);
                    sqlite3_bind_double(stmt, 8, d.active_secs);
                    sqlite3_bind_double(stmt, 9, d.live_power_w);
                    sqlite3_bind_double(stmt, 10, d.memory_mb);
                    sqlite3_bind_int64(stmt, 11, d.pids_count as i64);
                    sqlite3_step(stmt);
                    sqlite3_finalize(stmt);
                }
            }
        }
        self.exec("COMMIT;")?;
        Ok(())
    }

    /// Finds the timestamp of the last full charge (>= 90% capacity or Charging->Discharging transition)
    pub fn get_last_full_charge_ts(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        let sql = CString::new(
            "SELECT timestamp FROM battery_samples
             WHERE capacity_pct >= 90.0 AND timestamp <= ?1
             ORDER BY timestamp DESC LIMIT 1",
        )
        .unwrap();
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) == SQLITE_OK
            {
                sqlite3_bind_int64(stmt, 1, now);
                if sqlite3_step(stmt) == SQLITE_ROW {
                    let ts = sqlite3_column_int64(stmt, 0);
                    sqlite3_finalize(stmt);
                    if ts > 0 && now - ts < 7 * 86400 {
                        return ts;
                    }
                } else {
                    sqlite3_finalize(stmt);
                }
            }
        }
        now - 12 * 3600
    }

    pub fn get_battery_history(&self, since_ts: i64) -> Result<Vec<BatteryHistoryPoint>> {
        let sql = CString::new(
            "SELECT timestamp, capacity_pct, status, power_w
             FROM battery_samples
             WHERE timestamp >= ?1
             ORDER BY timestamp ASC",
        )
        .context("Invalid SQL")?;
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        let mut points = Vec::new();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK
            {
                bail!("Query failed: {}", self.last_error());
            }
            sqlite3_bind_int64(stmt, 1, since_ts);
            while sqlite3_step(stmt) == SQLITE_ROW {
                let ts = sqlite3_column_int64(stmt, 0);
                let pct = sqlite3_column_double(stmt, 1);
                let status_ptr = sqlite3_column_text(stmt, 2);
                let status = if status_ptr.is_null() {
                    "Unknown".to_string()
                } else {
                    CStr::from_ptr(status_ptr).to_string_lossy().to_string()
                };
                let power_w = sqlite3_column_double(stmt, 3);
                points.push(BatteryHistoryPoint {
                    timestamp: ts,
                    capacity_pct: pct,
                    status,
                    power_w,
                });
            }
            sqlite3_finalize(stmt);
        }
        Ok(points)
    }

    pub fn get_app_breakdown(
        &self,
        since_ts: i64,
        live_deltas: &[AppDeltaUsage],
    ) -> Result<Vec<AppSummaryRow>> {
        let bucket_cutoff = (since_ts / 300) * 300;
        let sql = CString::new(
            "SELECT app_name, category, unit_hint,
                    SUM(battery_pct), SUM(energy_wh), SUM(cpu_secs), SUM(active_secs),
                    MAX(peak_power_w), AVG(memory_mb), MAX(pids_count)
             FROM app_usage_buckets
             WHERE bucket_ts >= ?1
             GROUP BY app_name
             ORDER BY SUM(battery_pct) DESC",
        )?;

        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        let mut rows = Vec::new();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK
            {
                bail!("Query failed: {}", self.last_error());
            }
            sqlite3_bind_int64(stmt, 1, bucket_cutoff);
            while sqlite3_step(stmt) == SQLITE_ROW {
                let name = {
                    let p = sqlite3_column_text(stmt, 0);
                    if p.is_null() {
                        "Unknown".to_string()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().to_string()
                    }
                };
                let category = {
                    let p = sqlite3_column_text(stmt, 1);
                    if p.is_null() {
                        "App".to_string()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().to_string()
                    }
                };
                let unit_hint = {
                    let p = sqlite3_column_text(stmt, 2);
                    if p.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().to_string()
                    }
                };
                let battery_pct = sqlite3_column_double(stmt, 3);
                let energy_wh = sqlite3_column_double(stmt, 4);
                let cpu_secs = sqlite3_column_double(stmt, 5);
                let active_secs = sqlite3_column_double(stmt, 6);
                let peak_power_w = sqlite3_column_double(stmt, 7);
                let memory_mb = sqlite3_column_double(stmt, 8);
                let pids_count = sqlite3_column_int64(stmt, 9) as u32;

                rows.push(AppSummaryRow {
                    app_name: name,
                    category,
                    unit_hint,
                    battery_pct,
                    share_of_drain_pct: 0.0,
                    energy_wh,
                    cpu_secs,
                    active_secs,
                    peak_power_w,
                    live_power_w: 0.0,
                    live_cpu_pct: 0.0,
                    memory_mb,
                    pids_count,
                    is_frozen: false,
                });
            }
            sqlite3_finalize(stmt);
        }

        // Overlay current live metrics (live_power_w, live_cpu_pct, current memory_mb, pids & frozen state)
        for live in live_deltas {
            if let Some(existing) = rows.iter_mut().find(|r| r.app_name == live.app_name) {
                existing.live_power_w = live.live_power_w;
                existing.live_cpu_pct = live.live_cpu_pct;
                existing.memory_mb = live.memory_mb;
                existing.pids_count = live.pids_count;
                existing.is_frozen = live.is_frozen;
                if live.live_power_w > existing.peak_power_w {
                    existing.peak_power_w = live.live_power_w;
                }
            } else if live.live_cpu_pct > 0.05 || live.memory_mb > 10.0 {
                rows.push(AppSummaryRow {
                    app_name: live.app_name.clone(),
                    category: live.category.clone(),
                    unit_hint: live.unit_hint.clone(),
                    battery_pct: live.battery_pct,
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
                });
            }
        }

        let total_batt_pct: f64 = rows.iter().map(|r| r.battery_pct).sum();
        if total_batt_pct > 0.0 {
            for r in &mut rows {
                r.share_of_drain_pct = (r.battery_pct / total_batt_pct) * 100.0;
            }
        }

        let _ = SQLITE_DONE;
        Ok(rows)
    }

    /// Returns 5-minute / hourly bucket series `(bucket_ts, battery_pct, energy_wh)` for a specific app
    pub fn get_app_buckets(&self, app_name: &str, since_ts: i64) -> Vec<(i64, f64, f64)> {
        let bucket_cutoff = (since_ts / 300) * 300;
        let Ok(sql) = CString::new(
            "SELECT bucket_ts, battery_pct, energy_wh
             FROM app_usage_buckets
             WHERE app_name = ?1 AND bucket_ts >= ?2
             ORDER BY bucket_ts ASC",
        ) else {
            return Vec::new();
        };
        let Ok(c_name) = CString::new(app_name) else {
            return Vec::new();
        };
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        let mut out = Vec::new();
        unsafe {
            if sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) == SQLITE_OK
            {
                sqlite3_bind_text(
                    stmt,
                    1,
                    c_name.as_ptr(),
                    -1,
                    SQLITE_TRANSIENT as *const c_void,
                );
                sqlite3_bind_int64(stmt, 2, bucket_cutoff);
                while sqlite3_step(stmt) == SQLITE_ROW {
                    out.push((
                        sqlite3_column_int64(stmt, 0),
                        sqlite3_column_double(stmt, 1),
                        sqlite3_column_double(stmt, 2),
                    ));
                }
                sqlite3_finalize(stmt);
            }
        }
        out
    }
}
