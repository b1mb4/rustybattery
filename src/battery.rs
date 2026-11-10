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
            "not charging" | "not_charging" | "idle" => Self::NotCharging,
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

fn read_i64(path: &Path) -> Option<i64> {
    read_trimmed(path)?.parse::<i64>().ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    read_i64(path).map(|v| v.unsigned_abs())
}

/// Finds all system laptop battery directories in `/sys/class/power_supply/`
/// across all Linux hardware platforms (x86_64 ACPI `BAT0`/`BAT1`/`CMB0`, ThinkPad dual batteries,
/// Apple Silicon `macsmc-battery`, ARM/Chromebook `sbs-*`, etc.), while filtering out peripheral
/// HID/Bluetooth batteries (mice, keyboards, headphones, controllers where `scope == "Device"`).
pub fn find_battery_dirs() -> Vec<PathBuf> {
    let base = Path::new("/sys/class/power_supply");
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(dev_type) = read_trimmed(&path.join("type")) {
            if dev_type.eq_ignore_ascii_case("Battery") {
                // Skip peripheral device batteries (Bluetooth mice, gamepads, headphones)
                if let Some(scope) = read_trimmed(&path.join("scope")) {
                    if scope.eq_ignore_ascii_case("Device") {
                        continue;
                    }
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if name.starts_with("hid-") || name.contains("mouse") || name.contains("keyboard") {
                    continue;
                }
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates
}

pub fn is_ac_online() -> bool {
    let base = Path::new("/sys/class/power_supply");
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(dev_type) = read_trimmed(&path.join("type")) {
                if dev_type.eq_ignore_ascii_case("Mains")
                    || dev_type.eq_ignore_ascii_case("USB")
                    || dev_type.eq_ignore_ascii_case("USB_PD")
                {
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
    let bat_dirs = find_battery_dirs();
    let first_dir = bat_dirs
        .first()
        .context("No system battery found in /sys/class/power_supply/")?;

    let device_name = bat_dirs
        .iter()
        .filter_map(|d| d.file_name().map(|s| s.to_string_lossy().to_string()))
        .collect::<Vec<_>>()
        .join("+");

    let manufacturer =
        read_trimmed(&first_dir.join("manufacturer")).unwrap_or_else(|| "Linux".to_string());
    let model_name =
        read_trimmed(&first_dir.join("model_name")).unwrap_or_else(|| "System Battery".to_string());
    let technology =
        read_trimmed(&first_dir.join("technology")).unwrap_or_else(|| "Li-ion".to_string());

    let mut total_energy_now_wh = 0.0;
    let mut total_energy_full_wh = 0.0;
    let mut total_energy_design_wh = 0.0;
    let mut total_power_w = 0.0;
    let mut max_cycles = 0u32;
    let mut avg_voltage_v = 0.0;
    let mut cap_sum = 0u64;
    let mut overall_status = BatteryStatus::Unknown;

    for bat_dir in &bat_dirs {
        let status_raw =
            read_trimmed(&bat_dir.join("status")).unwrap_or_else(|| "Unknown".to_string());
        let st = BatteryStatus::from_str_raw(&status_raw);
        if st == BatteryStatus::Discharging || overall_status == BatteryStatus::Unknown {
            overall_status = st;
        } else if st == BatteryStatus::Charging && overall_status != BatteryStatus::Discharging {
            overall_status = BatteryStatus::Charging;
        }

        let cap = read_u64(&bat_dir.join("capacity")).unwrap_or(0).min(100);
        cap_sum += cap;

        let voltage_uv = read_u64(&bat_dir.join("voltage_now"))
            .or_else(|| read_u64(&bat_dir.join("voltage_min_design")))
            .unwrap_or(12_000_000);
        let voltage_v = (voltage_uv as f64 / 1_000_000.0).clamp(3.0, 30.0);
        avg_voltage_v = voltage_v;

        let e_now = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_now")) {
            e_uwh as f64 / 1_000_000.0
        } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_now")) {
            (c_uah as f64 / 1_000_000.0) * voltage_v
        } else {
            0.0
        };

        let e_full = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_full")) {
            e_uwh as f64 / 1_000_000.0
        } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_full")) {
            (c_uah as f64 / 1_000_000.0) * voltage_v
        } else {
            60.0
        };

        let e_design = if let Some(e_uwh) = read_u64(&bat_dir.join("energy_full_design")) {
            e_uwh as f64 / 1_000_000.0
        } else if let Some(c_uah) = read_u64(&bat_dir.join("charge_full_design")) {
            (c_uah as f64 / 1_000_000.0) * voltage_v
        } else {
            e_full
        };

        let p_w = if let Some(p_uw) = read_u64(&bat_dir.join("power_now"))
            .or_else(|| read_u64(&bat_dir.join("power_avg")))
        {
            p_uw as f64 / 1_000_000.0
        } else if let Some(i_ua) = read_u64(&bat_dir.join("current_now"))
            .or_else(|| read_u64(&bat_dir.join("current_avg")))
        {
            (i_ua as f64 / 1_000_000.0) * voltage_v
        } else {
            0.0
        };

        total_energy_now_wh += e_now;
        total_energy_full_wh += e_full;
        total_energy_design_wh += e_design;
        total_power_w += p_w;

        let cyc = read_u64(&bat_dir.join("cycle_count")).unwrap_or(0) as u32;
        if cyc > max_cycles {
            max_cycles = cyc;
        }
    }

    let capacity_int = (cap_sum / (bat_dirs.len().max(1) as u64)).min(100) as u8;

    let capacity_pct = if total_energy_full_wh > 1.0 && total_energy_now_wh > 0.0 {
        ((total_energy_now_wh / total_energy_full_wh) * 100.0).clamp(0.0, 100.0)
    } else {
        capacity_int as f64
    };

    let health_pct = if total_energy_design_wh > 1.0 {
        ((total_energy_full_wh / total_energy_design_wh) * 100.0).clamp(0.0, 120.0)
    } else {
        100.0
    };

    let ac_online = is_ac_online();

    let time_remaining_mins = if total_power_w > 0.25 {
        match overall_status {
            BatteryStatus::Discharging => {
                let hours = total_energy_now_wh / total_power_w;
                Some((hours * 60.0).round() as u32)
            }
            BatteryStatus::Charging => {
                let needed_wh = (total_energy_full_wh - total_energy_now_wh).max(0.0);
                let hours = needed_wh / total_power_w;
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
        status: overall_status,
        power_w: total_power_w,
        energy_now_wh: total_energy_now_wh,
        energy_full_wh: total_energy_full_wh,
        energy_design_wh: total_energy_design_wh,
        health_pct,
        voltage_v: avg_voltage_v,
        cycle_count: max_cycles,
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

/// Reads UPower's local battery history files across Linux distributions (`/var/lib/upower` or `/var/lib/UPower`)
/// so the user immediately gets historical battery curves on first launch.
pub fn load_upower_history(energy_full_wh: f64) -> Vec<UpowerHistoryPoint> {
    let upower_dir = if Path::new("/var/lib/upower").exists() {
        Path::new("/var/lib/upower")
    } else if Path::new("/var/lib/UPower").exists() {
        Path::new("/var/lib/UPower")
    } else {
        return Vec::new();
    };

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
            || name.contains("mouse")
            || name.contains("keyboard")
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
