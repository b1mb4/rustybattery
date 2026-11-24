use crate::db::Database;
use anyhow::{bail, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const META_SAVED_PROFILE: &str = "saved_power_profile";
const META_SAVED_KBD: &str = "saved_kbd_backlight";

fn parse_dbus_profile_variant(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let start = trimmed.find('\'')?;
    let rest = &trimmed[start + 1..];
    let end = rest.find('\'')?;
    let val = rest[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

fn get_dbus_power_profile() -> Option<(String, String)> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--system",
            "--dest",
            "net.hadess.PowerProfiles",
            "--object-path",
            "/net/hadess/PowerProfiles",
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "net.hadess.PowerProfiles",
            "ActiveProfile",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let prof = parse_dbus_profile_variant(&String::from_utf8_lossy(&out.stdout))?;
    Some(("tuned-ppd".to_string(), prof))
}

fn is_safe_profile_name(profile: &str) -> bool {
    !profile.is_empty()
        && profile.len() <= 64
        && profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn set_dbus_power_profile(profile: &str) -> bool {
    if !is_safe_profile_name(profile) {
        return false;
    }
    let variant = format!("<'{profile}'>");
    Command::new("gdbus")
        .args([
            "call",
            "--system",
            "--dest",
            "net.hadess.PowerProfiles",
            "--object-path",
            "/net/hadess/PowerProfiles",
            "--method",
            "org.freedesktop.DBus.Properties.Set",
            "net.hadess.PowerProfiles",
            "ActiveProfile",
            &variant,
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

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
    if let Some(res) = get_dbus_power_profile() {
        return Some(res);
    }
    if let Ok(out) = Command::new("tuned-adm")
        .args(["--timeout", "3", "active"])
        .output()
    {
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

    if set_dbus_power_profile(target_ppd) {
        return Ok(format!("tuned-ppd={target_ppd}"));
    }

    let target_tuned = if powersave { "powersave" } else { "balanced" };
    if let Ok(out) = Command::new("tuned-adm")
        .args(["--async", "--timeout", "3", "profile", target_tuned])
        .output()
    {
        if out.status.success() {
            return Ok(format!("tuned={target_tuned}"));
        }
    }

    bail!("No writable power-profile daemon (powerprofilesctl / tuned-ppd / tuned-adm) available")
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
/// Only clears the saved metadata key after the restore command actually succeeds so transient
/// D-Bus/resume failures never permanently lose the user's preferred profile.
pub fn restore_saved_power_profile(db: &Database) -> Option<String> {
    let saved = db.get_meta(META_SAVED_PROFILE)?;
    let Some((backend, prof)) = saved.split_once(':') else {
        let _ = db.delete_meta(META_SAVED_PROFILE);
        return None;
    };
    if !is_safe_profile_name(prof) {
        let _ = db.delete_meta(META_SAVED_PROFILE);
        return None;
    }

    let restored = if backend == "tuned-ppd" {
        if set_dbus_power_profile(prof) {
            Some(format!("tuned-ppd={prof}"))
        } else {
            Command::new("tuned-adm")
                .args(["--async", "--timeout", "3", "profile", prof])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| format!("tuned-adm={prof}"))
        }
    } else {
        let args: Vec<&str> = match backend {
            "powerprofilesctl" => vec!["set", prof],
            "tuned-adm" => vec!["--async", "--timeout", "3", "profile", prof],
            _ => {
                let _ = db.delete_meta(META_SAVED_PROFILE);
                return None;
            }
        };
        Command::new(backend)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| format!("{backend}={prof}"))
    };

    if restored.is_some() {
        let _ = db.delete_meta(META_SAVED_PROFILE);
    }
    restored
}

/// Restores the saved power profile when plugging into AC, or falls back to `balanced` if no saved
/// profile exists and the system is currently stuck in `power-saver` / `powersave`.
pub fn restore_or_balanced_on_ac(db: &Database) -> Option<String> {
    if let Some(restored) = restore_saved_power_profile(db) {
        return Some(restored);
    }
    if let Some((_, active)) = get_power_profile() {
        if active == "power-saver" || active == "powersave" {
            if let Ok(note) = set_power_profile(false) {
                return Some(note);
            }
        }
    }
    None
}

/// Parses `gdbus` UPower `KbdBacklight.GetBrightness` output such as `"(2,)"` or `"(0,)"`.
pub fn parse_upower_brightness(raw: &str) -> Option<u32> {
    let trimmed = raw
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim_end_matches(',')
        .trim();
    trimmed.parse::<u32>().ok()
}

fn get_upower_kbd_brightness() -> Option<u32> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--system",
            "--dest",
            "org.freedesktop.UPower",
            "--object-path",
            "/org/freedesktop/UPower/KbdBacklight",
            "--method",
            "org.freedesktop.UPower.KbdBacklight.GetBrightness",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_upower_brightness(&String::from_utf8_lossy(&out.stdout))
}

fn set_upower_kbd_brightness(level: u32) -> bool {
    Command::new("gdbus")
        .args([
            "call",
            "--system",
            "--dest",
            "org.freedesktop.UPower",
            "--object-path",
            "/org/freedesktop/UPower/KbdBacklight",
            "--method",
            "org.freedesktop.UPower.KbdBacklight.SetBrightness",
            &level.to_string(),
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Saves current keyboard backlight brightness in `meta` and dims it to `0`.
/// Prefers unprivileged UPower D-Bus (`org.freedesktop.UPower.KbdBacklight`) before falling back
/// to direct `/sys/class/leds/*kbd_backlight*/brightness` writes.
pub fn dim_and_save_kbd_backlight(db: &Database) -> bool {
    // 1. Prefer unprivileged UPower D-Bus (works without root on Fedora/Ubuntu/Arch/GNOME/KDE)
    if let Some(cur_val) = get_upower_kbd_brightness() {
        if cur_val == 0 {
            return true;
        }
        if set_upower_kbd_brightness(0) {
            if db.get_meta(META_SAVED_KBD).is_none() {
                let _ = db.set_meta(META_SAVED_KBD, &format!("upower:{cur_val}"));
            }
            return true;
        }
    }

    // 2. Fallback: direct sysfs write (when udev grants user write access or running as root)
    let Ok(entries) = fs::read_dir("/sys/class/leds") else {
        return false;
    };
    let mut dimmed = false;
    let mut saved_entries = Vec::new();
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
                saved_entries.push(format!("{}:{}", b_path.display(), cur_val));
            }
        }
    }
    if !saved_entries.is_empty() && db.get_meta(META_SAVED_KBD).is_none() {
        let _ = db.set_meta(META_SAVED_KBD, &saved_entries.join("\n"));
    }
    dimmed
}

/// Restores keyboard backlight brightness saved before low-battery or sleep dimming.
/// Only clears `META_SAVED_KBD` once restoration succeeds.
pub fn restore_saved_kbd_backlight(db: &Database) -> bool {
    let Some(saved) = db.get_meta(META_SAVED_KBD) else {
        return false;
    };
    let trimmed = saved.trim();
    if let Some(level_str) = trimmed.strip_prefix("upower:") {
        let Ok(level) = level_str.trim().parse::<u32>() else {
            let _ = db.delete_meta(META_SAVED_KBD);
            return false;
        };
        if set_upower_kbd_brightness(level) {
            let _ = db.delete_meta(META_SAVED_KBD);
            return true;
        }
        return false;
    }

    let mut any_ok = false;
    let mut all_ok = true;
    for line in trimmed.lines() {
        let Some((path_str, val_str)) = line.rsplit_once(':') else {
            continue;
        };
        if fs::write(PathBuf::from(path_str), val_str).is_ok() {
            any_ok = true;
        } else {
            all_ok = false;
        }
    }
    if any_ok && all_ok {
        let _ = db.delete_meta(META_SAVED_KBD);
    }
    any_ok
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upower_gdbus_brightness_tuple() {
        assert_eq!(parse_upower_brightness("(0,)\n"), Some(0));
        assert_eq!(parse_upower_brightness("(3,)"), Some(3));
        assert_eq!(parse_upower_brightness("(2)"), Some(2));
        assert_eq!(parse_upower_brightness("error"), None);
    }

    #[test]
    fn validates_profile_names_for_gvariant_safety() {
        assert!(is_safe_profile_name("balanced"));
        assert!(is_safe_profile_name("power-saver"));
        assert!(is_safe_profile_name("balanced_battery"));
        assert!(!is_safe_profile_name(""));
        assert!(!is_safe_profile_name("balanced'>; id"));
    }
}
