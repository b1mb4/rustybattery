use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BatteryStatus {
    Discharging,
    Charging,
    Full,
    NotCharging,
    Unknown,
}

impl BatteryStatus {
    pub fn from_str_raw(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "discharging" => Self::Discharging,
            "charging" => Self::Charging,
            "full" => Self::Full,
            "not charging" | "not_charging" => Self::NotCharging,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Discharging => "Discharging",
            Self::Charging => "Charging",
            Self::Full => "Full",
            Self::NotCharging => "Plugged In (Not Charging)",
            Self::Unknown => "Unknown",
        }
    }

    pub fn is_discharging(&self) -> bool {
        matches!(self, Self::Discharging)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySnapshot {
    pub timestamp: i64,
    pub device_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub technology: String,
    /// Exact capacity percentage (calculated from energy_now/energy_full when available, or capacity file)
    pub capacity_pct: f64,
    /// Integer percentage reported by kernel
    pub capacity_int: u8,
    pub status: BatteryStatus,
    /// Current power draw or charge rate in Watts
    pub power_w: f64,
    /// Current energy stored in Watt-hours (Wh)
    pub energy_now_wh: f64,
    /// Full charge capacity in Watt-hours (Wh)
    pub energy_full_wh: f64,
    /// Factory design capacity in Watt-hours (Wh)
    pub energy_design_wh: f64,
    /// Battery health percentage (energy_full / energy_design * 100)
    pub health_pct: f64,
    /// Voltage in Volts
    pub voltage_v: f64,
    /// Charge cycles count
    pub cycle_count: u32,
    /// AC adapter online status
    pub ac_online: bool,
    /// Estimated minutes remaining (until empty when discharging, or until full when charging)
    pub time_remaining_mins: Option<u32>,
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse::<u64>().ok()
}

pub fn find_battery_dir() -> Option<PathBuf> {
    let base = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(base).ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(dev_type) = read_trimmed(&path.join("type")) {
            if dev_type.eq_ignore_ascii_case("Battery") {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

pub fn is_ac_online() -> bool {
    let base = Path::new("/sys/class/power_supply");
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(dev_type) = read_trimmed(&path.join("type")) {
                if dev_type.eq_ignore_ascii_case("Mains") || dev_type.eq_ignore_ascii_case("USB") {
                    if read_u64(&path.join("online")) == Some(1) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub fn read_battery_snapshot() -> Result<BatterySnapshot> {
    let bat_dir = find_battery_dir()
        .context("No battery found in /sys/class/power_supply/BAT*")?;

    let device_name = bat_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "BAT0".to_string());

    let manufacturer = read_trimmed(&bat_dir.join("manufacturer")).unwrap_or_else(|| "Generic".to_string());
    let model_name = read_trimmed(&bat_dir.join("model_name")).unwrap_or_else(|| "Battery".to_string());
    let technology = read_trimmed(&bat_dir.join("technology")).unwrap_or_else(|| "Li-ion".to_string());

    let status_raw = read_trimmed(&bat_dir.join("status")).unwrap_or_else(|| "Unknown".to_string());
    let status = BatteryStatus::from_str_raw(&status_raw);

    let capacity_int = read_u64(&bat_dir.join("capacity")).unwrap_or(0).min(100) as u8;
    let voltage_uv = read_u64(&bat_dir.join("voltage_now")).unwrap_or(15_000_000);
    let voltage_v = voltage_uv as f64 / 1_000_000.0;

    // Support both energy_* (µWh) and charge_* (µAh) sysfs layouts
    let energy_now_wh = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_now")) {
        e_uwh as f64 / 1_000_000.0
    } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_now")) {
        (c_uah as f64 / 1_000_000.0) * voltage_v
    } else {
        0.0
    };

    let energy_full_wh = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_full")) {
        e_uwh as f64 / 1_000_000.0
    } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_full")) {
        (c_uah as f64 / 1_000_000.0) * voltage_v
    } else {
        60.0
    };

    let energy_design_wh = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_full_design")) {
        e_uwh as f64 / 1_000_000.0
    } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_full_design")) {
        (c_uah as f64 / 1_000_000.0) * voltage_v
    } else {
        energy_full_wh
    };

    let power_w = if let Some(p_uw) = read_u64(&bat_dir.join("power_now")) {
        p_uw as f64 / 1_000_000.0
    } else if let Some(i_ua) = read_u64(&bat_dir.join("current_now")) {
        (i_ua as f64 / 1_000_000.0) * voltage_v
    } else {
        0.0
    };

    let capacity_pct = if energy_full_wh > 1.0 && energy_now_wh > 0.0 {
        ((energy_now_wh / energy_full_wh) * 100.0).clamp(0.0, 100.0)
    } else {
        capacity_int as f64
    };

    let health_pct = if energy_design_wh > 1.0 {
        ((energy_full_wh / energy_design_wh) * 100.0).clamp(0.0, 120.0)
    } else {
        100.0
    };

    let cycle_count = read_u64(&bat_dir.join("cycle_count")).unwrap_or(0) as u32;
    let ac_online = is_ac_online();

    let time_remaining_mins = if power_w > 0.25 {
        match status {
            BatteryStatus::Discharging => {
                let hours = energy_now_wh / power_w;
                Some((hours * 60.0).round() as u32)
            }
            BatteryStatus::Charging => {
                let needed_wh = (energy_full_wh - energy_now_wh).max(0.0);
                let hours = needed_wh / power_w;
                Some((hours * 60.0).round() as u32)
            }
            _ => None,
        }
    } else {
        None
    };

    let timestamp = chrono::Utc::now().timestamp();

    Ok(BatterySnapshot {
        timestamp,
        device_name,
        manufacturer,
        model_name,
        technology,
        capacity_pct,
        capacity_int,
        status,
        power_w,
        energy_now_wh,
        energy_full_wh,
        energy_design_wh,
        health_pct,
        voltage_v,
        cycle_count,
        ac_online,
        time_remaining_mins,
    })
}

/// Historical sample imported from UPower `/var/lib/upower/history-*.dat`
#[derive(Debug, Clone)]
pub struct UpowerHistoryPoint {
    pub timestamp: i64,
    pub capacity_pct: f64,
    pub power_w: f64,
    pub status: String,
}

/// Reads UPower's local battery history files on Fedora (`/var/lib/upower/history-charge-*.dat` and `history-rate-*.dat`)
/// so the user immediately gets historical battery curves on first launch.
pub fn load_upower_history(energy_full_wh: f64) -> Vec<UpowerHistoryPoint> {
    let upower_dir = Path::new("/var/lib/upower");
    let Ok(entries) = fs::read_dir(upower_dir) else {
        return Vec::new();
    };

    let mut charge_file: Option<PathBuf> = None;
    let mut rate_file: Option<PathBuf> = None;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Exclude peripheral batteries like phones/controllers/mice
        if name.contains("iPhone")
            || name.contains("Controller")
            || name.contains("Pebble")
            || name.contains("MAJOR")
            || name.contains("generic")
            || name.contains("XO-")
        {
            continue;
        }
        if name.starts_with("history-charge-") && name.ends_with(".dat") {
            charge_file = Some(entry.path());
        } else if name.starts_with("history-rate-") && name.ends_with(".dat") {
            rate_file = Some(entry.path());
        }
    }

    let Some(c_path) = charge_file else {
        return Vec::new();
    };

    // Parse rate file into a sorted list of (timestamp, watts)
    let mut rates: Vec<(i64, f64)> = Vec::new();
    if let Some(r_path) = rate_file {
        if let Ok(content) = fs::read_to_string(r_path) {
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(ts), Ok(w)) = (parts[0].parse::<i64>(), parts[1].parse::<f64>()) {
                        if ts > 1_000_000_000 && w >= 0.0 && w < 250.0 {
                            rates.push((ts, w));
                        }
                    }
                }
            }
        }
    }
    rates.sort_by_key(|r| r.0);

    let mut points = Vec::new();
    if let Ok(content) = fs::read_to_string(c_path) {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                if let (Ok(ts), Ok(pct)) = (parts[0].parse::<i64>(), parts[1].parse::<f64>()) {
                    if ts <= 1_000_000_000 || pct <= 0.0 || pct > 100.0 {
                        continue;
                    }
                    let status = match parts[2] {
                        "charging" => "Charging",
                        "discharging" => "Discharging",
                        "fully-charged" => "Full",
                        _ => "Unknown",
                    }
                    .to_string();

                    // Find closest rate sample within 120 seconds
                    let power_w = match rates.binary_search_by_key(&ts, |&(t, _)| t) {
                        Ok(idx) => rates[idx].1,
                        Err(idx) => {
                            let c1 = idx.checked_sub(1).and_then(|i| rates.get(i));
                            let c2 = rates.get(idx);
                            match (c1, c2) {
                                (Some(&(t1, w1)), Some(&(t2, w2))) => {
                                    if (ts - t1).abs() <= (t2 - ts).abs() && (ts - t1).abs() < 180 {
                                        w1
                                    } else if (t2 - ts).abs() < 180 {
                                        w2
                                    } else {
                                        10.0
                                    }
                                }
                                (Some(&(t1, w1)), None) if (ts - t1).abs() < 180 => w1,
                                (None, Some(&(t2, w2))) if (t2 - ts).abs() < 180 => w2,
                                _ => 10.0,
                            }
                        }
                    };

                    let _ = energy_full_wh;
                    points.push(UpowerHistoryPoint {
                        timestamp: ts,
                        capacity_pct: pct,
                        power_w,
                        status,
                    });
                }
            }
        }
    }

    points
}
