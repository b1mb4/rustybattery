use crate::db::Database;
use anyhow::{bail, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const META_SAVED_PROFILE: &str = "saved_power_profile";
const META_SAVED_KBD: &str = "saved_kbd_backlight";

/// Queries the currently active power profile (`("powerprofilesctl", "balanced")`, etc.).
pub fn get_power_profile() -> Option<(String, String)> {
    if let Ok(out) = Command::new("powerprofilesctl").arg("get").output() {
        if out.status.success() {
            let prof = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !prof.is_empty() {
                return Some(("powerprofilesctl".to_string(), prof));
            }
        }
    }
    if let Ok(out) = Command::new("tuned-adm").arg("active").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(prof) = s.split(':').nth(1).map(|p| p.trim().to_string()) {
                if !prof.is_empty() {
                    return Some(("tuned-adm".to_string(), prof));
                }
            }
        }
    }
    None
}

/// Switches the system power profile (`powersave = true` -> power-saver, `false` -> balanced)
/// and returns `Err` when no backend succeeded instead of claiming false success.
pub fn set_power_profile(powersave: bool) -> Result<String> {
    let target_ppd = if powersave { "power-saver" } else { "balanced" };
    if let Ok(out) = Command::new("powerprofilesctl")
        .args(["set", target_ppd])
        .output()
    {
        if out.status.success() {
            return Ok(format!("powerprofilesctl={target_ppd}"));
        }
    }

    let target_tuned = if powersave { "powersave" } else { "balanced" };
    if let Ok(out) = Command::new("tuned-adm")
        .args(["profile", target_tuned])
        .output()
    {
        if out.status.success() {
            return Ok(format!("tuned={target_tuned}"));
        }
    }

    bail!("No writable power-profile daemon (powerprofilesctl / tuned-adm) available")
}

/// Saves the user's current power profile in `meta` (if not already saved) before switching to `power-saver`.
pub fn save_and_switch_powersave(db: &Database) -> Result<String> {
    if db.get_meta(META_SAVED_PROFILE).is_none() {
        if let Some((backend, prof)) = get_power_profile() {
            if prof != "power-saver" && prof != "powersave" {
                let _ = db.set_meta(META_SAVED_PROFILE, &format!("{backend}:{prof}"));
            }
        }
    }
    set_power_profile(true)
}

/// Restores the exact power profile that was active before RustyBattery switched to `power-saver`.
pub fn restore_saved_power_profile(db: &Database) -> Option<String> {
    let saved = db.get_meta(META_SAVED_PROFILE)?;
    let _ = db.delete_meta(META_SAVED_PROFILE);
    let (backend, prof) = saved.split_once(':')?;
    let args: Vec<&str> = match backend {
        "powerprofilesctl" => vec!["set", prof],
        "tuned-adm" => vec!["profile", prof],
        _ => return None,
    };
    if let Ok(out) = Command::new(backend).args(args).output() {
        if out.status.success() {
            return Some(format!("{backend}={prof}"));
        }
    }
    None
}

/// Saves current keyboard backlight brightness in `meta` and dims it to `0`.
pub fn dim_and_save_kbd_backlight(db: &Database) -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/leds") else {
        return false;
    };
    let mut dimmed = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains("kbd_backlight") {
            continue;
        }
        let b_path = entry.path().join("brightness");
        let prev_val = fs::read_to_string(&b_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&v| v > 0);
        if fs::write(&b_path, "0").is_ok() {
            dimmed = true;
            if let Some(cur_val) = prev_val {
                if db.get_meta(META_SAVED_KBD).is_none() {
                    let _ = db.set_meta(
                        META_SAVED_KBD,
                        &format!("{}:{}", b_path.display(), cur_val),
                    );
                }
            }
        }
    }
    dimmed
}

/// Restores keyboard backlight brightness saved before low-battery or sleep dimming.
pub fn restore_saved_kbd_backlight(db: &Database) -> bool {
    let Some(saved) = db.get_meta(META_SAVED_KBD) else {
        return false;
    };
    let _ = db.delete_meta(META_SAVED_KBD);
    let Some((path_str, val_str)) = saved.rsplit_once(':') else {
        return false;
    };
    fs::write(PathBuf::from(path_str), val_str).is_ok()
}

/// Returns `true` if Bluetooth should NOT be blocked on lid-close (e.g., an external display is connected
/// for clamshell mode, or an active Bluetooth peripheral/audio device is connected).
pub fn should_skip_bluetooth_block() -> bool {
    // 1. Check external display connected in /sys/class/drm
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("eDP")
                || name.contains("LVDS")
                || name.contains("DSI")
                || name.contains("Writeback")
            {
                continue;
            }
            if let Ok(st) = fs::read_to_string(entry.path().join("status")) {
                if st.trim().eq_ignore_ascii_case("connected") {
                    return true;
                }
            }
        }
    }

    // 2. Check active Bluetooth device connection (`bluetoothctl devices Connected`)
    if let Ok(out) = Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
    {
        if out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            return true;
        }
    }

    false
}
