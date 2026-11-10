use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawAppCounter {
    pub app_name: String,
    pub category: String,
    pub unit_hint: String,
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub memory_bytes: u64,
    pub pids_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDeltaUsage {
    pub app_name: String,
    pub category: String,
    pub unit_hint: String,
    /// Battery percentage points drained by this app during the interval
    pub battery_pct: f64,
    /// Energy consumed in Wh by this app during the interval
    pub energy_wh: f64,
    /// CPU time consumed in seconds during the interval
    pub cpu_secs: f64,
    /// Wall-clock active seconds during the interval (if app used >0.15% CPU)
    pub active_secs: f64,
    /// Estimated instantaneous power draw in Watts
    pub live_power_w: f64,
    /// Instantaneous CPU load percentage (100% = 1 full core)
    pub live_cpu_pct: f64,
    /// Current resident/cgroup memory in MB
    pub memory_mb: f64,
    /// Number of processes/threads in the app group
    pub pids_count: u32,
}

/// Converts a raw systemd unit name (e.g., `app-flatpak-net.waterfox.waterfox-3273568594.scope`)
/// into a human-friendly `(AppName, Category)` pair.
pub fn normalize_cgroup_unit(unit_raw: &str, slice_type: &str) -> Option<(String, String)> {
    let unit = unit_raw.trim();
    if unit.is_empty()
        || unit.starts_with("cgroup.")
        || unit.starts_with("cpu.")
        || unit.starts_with("io.")
        || unit.starts_with("memory.")
        || unit.starts_with("pids.")
        || unit.starts_with("irq.")
        || unit.ends_with(".socket")
        || unit == "init.scope"
    {
        return None;
    }

    // Unescape common systemd hex escapes like \x2d -> -
    let unescaped = unit
        .replace("\\x2d", "-")
        .replace("\\x2e", ".")
        .replace("\\x40", "@");

    let lower = unescaped.to_lowercase();

    // Known desktop/user applications & services on Fedora
    if lower.contains("waterfox") {
        return Some(("Waterfox".to_string(), "Browser".to_string()));
    }
    if lower.contains("firefox") {
        return Some(("Firefox".to_string(), "Browser".to_string()));
    }
    if lower.contains("chromium") {
        return Some(("Chromium".to_string(), "Browser".to_string()));
    }
    if lower.contains("chrome") {
        return Some(("Google Chrome".to_string(), "Browser".to_string()));
    }
    if lower.contains("brave") {
        return Some(("Brave Browser".to_string(), "Browser".to_string()));
    }
    if lower.contains("zen_browser") || lower.contains("zen-browser") {
        return Some(("Zen Browser".to_string(), "Browser".to_string()));
    }
    if lower.contains("antigravity") {
        return Some(("Antigravity".to_string(), "Development".to_string()));
    }
    if lower.contains("obsidian") {
        return Some(("Obsidian".to_string(), "Productivity".to_string()));
    }
    if lower.contains("ghostty") {
        return Some(("Ghostty".to_string(), "Terminal".to_string()));
    }
    if lower.contains("kitty") {
        return Some(("Kitty Terminal".to_string(), "Terminal".to_string()));
    }
    if lower.contains("alacritty") {
        return Some(("Alacritty".to_string(), "Terminal".to_string()));
    }
    if lower.contains("wezterm") {
        return Some(("WezTerm".to_string(), "Terminal".to_string()));
    }
    if lower.contains("gnome-terminal") || lower.contains("org.gnome.terminal") || lower.contains("ptyxis") {
        return Some(("GNOME Terminal".to_string(), "Terminal".to_string()));
    }
    if lower.contains("code") && (lower.contains("visual") || lower.contains("app-gnome-code") || lower.contains("vscode")) {
        return Some(("VS Code".to_string(), "Development".to_string()));
    }
    if lower.contains("zed") && lower.starts_with("app-") {
        return Some(("Zed Editor".to_string(), "Development".to_string()));
    }
    if lower.contains("docker") || lower.contains("containerd") || lower.contains("podman") {
        return Some(("Docker / Containers".to_string(), "Development".to_string()));
    }
    if lower.contains("org.gnome.shell") && !lower.contains("calendar") && !lower.contains("notifications") {
        return Some(("GNOME Shell".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("pipewire") || lower.contains("wireplumber") || lower.contains("pulseaudio") {
        return Some(("PipeWire Audio".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("spotify") {
        return Some(("Spotify".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("vlc") {
        return Some(("VLC Media Player".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("mpv") {
        return Some(("mpv Player".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("telegram") {
        return Some(("Telegram".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("discord") || lower.contains("vesktop") {
        return Some(("Discord".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("slack") {
        return Some(("Slack".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("steam") {
        return Some(("Steam".to_string(), "Gaming".to_string()));
    }
    if lower.contains("kdeconnect") {
        return Some(("KDE Connect".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("blueman") || lower.contains("bluetooth") {
        return Some(("Bluetooth".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("networkmanager") || lower.contains("wpa_supplicant") || lower.contains("iwd") || lower.contains("systemd-resolved") {
        return Some(("Network & Wi-Fi".to_string(), "System".to_string()));
    }
    if lower.contains("gnome-software") || lower.contains("packagekit") || lower.contains("fwupd") {
        return Some(("GNOME Software".to_string(), "System".to_string()));
    }
    if lower.contains("localsearch") || lower.contains("tracker-miner") {
        return Some(("File Indexer (LocalSearch)".to_string(), "Background".to_string()));
    }
    if lower.contains("org.gnome.settingsdaemon") || lower.contains("gnome-session") {
        return Some(("GNOME Settings & Session".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("evolution-") || lower.contains("calendarserver") {
        return Some(("GNOME Calendar & Contacts".to_string(), "Background".to_string()));
    }
    if lower.contains("gvfs") {
        return Some(("GVFS Storage Daemon".to_string(), "Background".to_string()));
    }
    if lower.contains("xdg-desktop-portal") || lower.contains("xdg-document-portal") || lower.contains("flatpak-portal") || lower.contains("flatpak-session") {
        return Some(("XDG Desktop Portals".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("dbus") {
        return Some(("D-Bus Broker".to_string(), "System".to_string()));
    }
    if lower.contains("ibus") {
        return Some(("IBus Input Method".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("upower") || lower.contains("power-profiles-daemon") || lower.contains("thermald") || lower.contains("tuned") {
        return Some(("Power Management".to_string(), "System".to_string()));
    }
    if lower.contains("systemd-journald") || lower.contains("systemd-udevd") || lower.contains(" systemd-logind") || lower.contains("auditd") {
        return Some(("Systemd Core Services".to_string(), "System".to_string()));
    }
    if lower.contains("polkit") || lower.contains("accounts-daemon") || lower.contains("sssd") || lower.contains("uresourced") || lower.contains("rtkit") {
        return Some(("System Security & Auth".to_string(), "System".to_string()));
    }

    // Generic parsing for `app-flatpak-<reverse.dns>-<pid>.scope` or `app-gnome-<name>-<pid>.scope` or `app-<name>-<pid>.scope`
    let mut base = unescaped
        .trim_end_matches(".scope")
        .trim_end_matches(".service")
        .trim_end_matches(".slice")
        .to_string();

    // Strip trailing `-<digits>` (PID or instance ID)
    if let Some(idx) = base.rfind('-') {
        let tail = &base[idx + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            base.truncate(idx);
        }
    }
    // Strip `@...`
    if let Some(idx) = base.find('@') {
        base.truncate(idx);
    }

    for prefix in [
        "app-flatpak-",
        "app-gnome-",
        "app-systemd-",
        "app-",
        "dbus-:1.",
    ] {
        if let Some(rest) = base.strip_prefix(prefix) {
            base = rest.to_string();
            break;
        }
    }

    // If reverse-DNS (like `org.mozilla.Thunderbird` or `com.obsproject.Studio`), take the most meaningful segment
    if base.contains('.') {
        let parts: Vec<&str> = base
            .split('.')
            .filter(|s| {
                let l = s.to_lowercase();
                !l.is_empty()
                    && l != "org"
                    && l != "com"
                    && l != "net"
                    && l != "io"
                    && l != "md"
                    && l != "app"
                    && l != "client"
                    && l != "desktop"
                    && l != "freedesktop"
                    && l != "gnome"
                    && !l.chars().all(|c| c.is_ascii_digit() || c == '-')
            })
            .collect();
        if let Some(&last) = parts.last() {
            base = last.to_string();
        }
    }

    let cleaned = base.replace(['-', '_'], " ").trim().to_string();
    if cleaned.is_empty() {
        return None;
    }

    // Title-case words
    let pretty = cleaned
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let category = match slice_type {
        "app" => "App",
        "session" => "Desktop UI",
        "background" => "Background",
        _ => "System",
    }
    .to_string();

    Some((pretty, category))
}

fn read_cgroup_cpu_stat(dir: &Path) -> Option<(u64, u64, u64)> {
    let content = fs::read_to_string(dir.join("cpu.stat")).ok()?;
    let mut usage = 0u64;
    let mut user = 0u64;
    let mut sys = 0u64;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(k) = parts.next() else { continue };
        let Some(v) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        match k {
            "usage_usec" => usage = v,
            "user_usec" => user = v,
            "system_usec" => sys = v,
            _ => {}
        }
    }
    if usage == 0 && (user > 0 || sys > 0) {
        usage = user + sys;
    }
    if usage > 0 {
        Some((usage, user, sys))
    } else {
        None
    }
}

fn read_cgroup_u64(dir: &Path, filename: &str) -> u64 {
    fs::read_to_string(dir.join(filename))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn scan_slice_dir(
    slice_dir: &Path,
    slice_type: &str,
    map: &mut HashMap<String, RawAppCounter>,
) {
    let Ok(entries) = fs::read_dir(slice_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let unit_name = entry.file_name().to_string_lossy().to_string();
        if unit_name.ends_with(".slice") {
            // Recurse into nested slices (like app-gnome-session-manager.slice)
            scan_slice_dir(&path, slice_type, map);
            continue;
        }
        let Some((app_name, category)) = normalize_cgroup_unit(&unit_name, slice_type) else {
            continue;
        };
        let Some((usage_usec, user_usec, system_usec)) = read_cgroup_cpu_stat(&path) else {
            continue;
        };
        let memory_bytes = read_cgroup_u64(&path, "memory.current");
        let pids_count = read_cgroup_u64(&path, "pids.current") as u32;

        let entry_counter = map.entry(app_name.clone()).or_insert_with(|| RawAppCounter {
            app_name,
            category,
            unit_hint: unit_name.clone(),
            usage_usec: 0,
            user_usec: 0,
            system_usec: 0,
            memory_bytes: 0,
            pids_count: 0,
        });
        entry_counter.usage_usec = entry_counter.usage_usec.saturating_add(usage_usec);
        entry_counter.user_usec = entry_counter.user_usec.saturating_add(user_usec);
        entry_counter.system_usec = entry_counter.system_usec.saturating_add(system_usec);
        entry_counter.memory_bytes = entry_counter.memory_bytes.saturating_add(memory_bytes);
        entry_counter.pids_count = entry_counter.pids_count.saturating_add(pids_count);
    }
}

/// Reads cumulative CPU microseconds, memory, and PID counts for all running apps and services
/// via Fedora's cgroup v2 hierarchy (with `/proc` fallback).
pub fn read_all_app_counters() -> HashMap<String, RawAppCounter> {
    let mut map = HashMap::new();

    let uid = unsafe { libc::getuid() };
    let user_service_dir = format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    );
    let user_base = Path::new(&user_service_dir);

    if user_base.exists() {
        scan_slice_dir(&user_base.join("app.slice"), "app", &mut map);
        scan_slice_dir(&user_base.join("session.slice"), "session", &mut map);
        scan_slice_dir(&user_base.join("background.slice"), "background", &mut map);
    }

    let system_slice = Path::new("/sys/fs/cgroup/system.slice");
    if system_slice.exists() {
        scan_slice_dir(system_slice, "system", &mut map);
    }

    // Fallback to /proc/[pid]/stat if cgroup v2 wasn't populated
    if map.is_empty() {
        scan_proc_fallback(&mut map);
    }

    map
}

fn scan_proc_fallback(map: &mut HashMap<String, RawAppCounter>) {
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let usec_per_tick = 1_000_000 / clk_tck;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid_dir = entry.path();
        let Ok(stat_str) = fs::read_to_string(pid_dir.join("stat")) else {
            continue;
        };
        let Some(open_paren) = stat_str.find('(') else { continue };
        let Some(close_paren) = stat_str.rfind(')') else { continue };
        let comm = &stat_str[open_paren + 1..close_paren];
        let after: Vec<&str> = stat_str[close_paren + 2..].split_whitespace().collect();
        if after.len() < 22 {
            continue;
        }
        let utime = after[11].parse::<u64>().unwrap_or(0) * usec_per_tick;
        let stime = after[12].parse::<u64>().unwrap_or(0) * usec_per_tick;
        let rss_pages = after[21].parse::<u64>().unwrap_or(0);
        let usage = utime + stime;
        if usage == 0 {
            continue;
        }
        let (app_name, category) =
            normalize_cgroup_unit(comm, "app").unwrap_or_else(|| (comm.to_string(), "App".to_string()));
        let counter = map.entry(app_name.clone()).or_insert_with(|| RawAppCounter {
            app_name,
            category,
            unit_hint: comm.to_string(),
            usage_usec: 0,
            user_usec: 0,
            system_usec: 0,
            memory_bytes: 0,
            pids_count: 0,
        });
        counter.usage_usec += usage;
        counter.user_usec += utime;
        counter.system_usec += stime;
        counter.memory_bytes += rss_pages * 4096;
        counter.pids_count += 1;
    }
}

/// Stateful tracker that computes per-tick deltas between snapshots and attributes battery drain.
pub struct AppEnergyTracker {
    prev_counters: HashMap<String, RawAppCounter>,
    prev_timestamp_ms: i64,
}

impl AppEnergyTracker {
    pub fn new() -> Self {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let initial = read_all_app_counters();
        Self {
            prev_counters: initial,
            prev_timestamp_ms: now_ms,
        }
    }

    /// Samples current app counters, computes CPU delta over `dt_secs`, and attributes
    /// the interval's battery energy (`power_w * dt_hours`) and battery `%` drop across active apps.
    pub fn sample_delta(
        &mut self,
        power_w: f64,
        energy_full_wh: f64,
        is_discharging: bool,
    ) -> (Vec<AppDeltaUsage>, f64) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let dt_secs = ((now_ms - self.prev_timestamp_ms) as f64 / 1000.0).max(0.1);
        self.prev_timestamp_ms = now_ms;

        let current = read_all_app_counters();

        let mut app_deltas: Vec<(RawAppCounter, u64)> = Vec::new();
        let mut total_delta_usec: u64 = 0;

        for (name, curr) in &current {
            let delta_usec = if let Some(prev) = self.prev_counters.get(name) {
                if curr.usage_usec >= prev.usage_usec {
                    curr.usage_usec - prev.usage_usec
                } else {
                    // Counter reset (app restarted)
                    curr.usage_usec
                }
            } else {
                // Newly launched app: cap initial delta to dt_secs * 2 cores so a pre-existing unit doesn't spike
                curr.usage_usec.min((dt_secs * 2_000_000.0) as u64)
            };

            total_delta_usec = total_delta_usec.saturating_add(delta_usec);
            app_deltas.push((curr.clone(), delta_usec));
        }

        self.prev_counters = current;

        // Interval energy in Wh & battery percentage points
        // Even if plugged in, we still compute `live_power_w` and `live_cpu_pct` for live monitoring,
        // while `battery_pct` and `energy_wh` accumulate actual battery discharge (or all power if discharging).
        let effective_power_w = if power_w > 0.1 { power_w } else { 8.5 };
        let interval_wh = if is_discharging {
            effective_power_w * (dt_secs / 3600.0)
        } else {
            0.0
        };
        let full_wh = energy_full_wh.max(10.0);
        let interval_batt_pct = (interval_wh / full_wh) * 100.0;

        let mut results = Vec::with_capacity(app_deltas.len());
        let denom = total_delta_usec.max(1) as f64;

        for (curr, d_usec) in app_deltas {
            let share = if total_delta_usec > 0 {
                (d_usec as f64) / denom
            } else {
                0.0
            };
            let cpu_secs = (d_usec as f64) / 1_000_000.0;
            let live_cpu_pct = (cpu_secs / dt_secs) * 100.0;
            let active_secs = if live_cpu_pct >= 0.15 { dt_secs } else { 0.0 };

            results.push(AppDeltaUsage {
                app_name: curr.app_name,
                category: curr.category,
                unit_hint: curr.unit_hint,
                battery_pct: interval_batt_pct * share,
                energy_wh: interval_wh * share,
                cpu_secs,
                active_secs,
                live_power_w: effective_power_w * share,
                live_cpu_pct,
                memory_mb: (curr.memory_bytes as f64) / (1024.0 * 1024.0),
                pids_count: curr.pids_count,
            });
        }

        results.sort_by(|a, b| {
            b.live_cpu_pct
                .partial_cmp(&a.live_cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        (results, dt_secs)
    }
}

/// Bootstraps initial session per-app battery attribution from cumulative cgroup v2 `cpu.stat`
/// since system boot so that the very first time the user launches `battstat`, they immediately
/// see their real session's per-app battery usage breakdown!
pub fn bootstrap_session_app_usage(
    session_discharged_pct: f64,
    energy_full_wh: f64,
    current_power_w: f64,
) -> Vec<AppDeltaUsage> {
    let counters = read_all_app_counters();
    let total_usec: u64 = counters.values().map(|c| c.usage_usec).sum();
    if total_usec == 0 {
        return Vec::new();
    }

    let discharged_pct = session_discharged_pct.clamp(1.0, 100.0);
    let session_energy_wh = (discharged_pct / 100.0) * energy_full_wh.max(10.0);
    let effective_power_w = if current_power_w > 0.1 {
        current_power_w
    } else {
        12.0
    };

    let mut list = Vec::with_capacity(counters.len());
    for c in counters.into_values() {
        if c.usage_usec < 10_000 {
            // Skip units with <10ms total CPU time
            continue;
        }
        let share = (c.usage_usec as f64) / (total_usec as f64);
        let cpu_secs = (c.usage_usec as f64) / 1_000_000.0;
        // Estimate active wall-clock time based on CPU activity profile
        let active_secs = (cpu_secs * 8.5).max(cpu_secs);

        list.push(AppDeltaUsage {
            app_name: c.app_name,
            category: c.category,
            unit_hint: c.unit_hint,
            battery_pct: discharged_pct * share,
            energy_wh: session_energy_wh * share,
            cpu_secs,
            active_secs,
            live_power_w: effective_power_w * share,
            live_cpu_pct: share * 100.0,
            memory_mb: (c.memory_bytes as f64) / (1024.0 * 1024.0),
            pids_count: c.pids_count,
        });
    }

    list.sort_by(|a, b| {
        b.battery_pct
            .partial_cmp(&a.battery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    list
}
