use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    pub is_frozen: bool,
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
    /// Whether this app's cgroup or process group is currently frozen
    pub is_frozen: bool,
}

/// Converts a raw systemd unit name, Snap scope, Flatpak scope, or process command across any Linux
/// distribution (Ubuntu, Fedora, Arch, Debian, openSUSE, NixOS, Alpine, Void, etc.) into a
/// human-friendly `(AppName, Category)` pair.
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

    // 1. Web Browsers
    if lower.contains("waterfox") {
        return Some(("Waterfox".to_string(), "Browser".to_string()));
    }
    if lower.contains("librewolf") {
        return Some(("LibreWolf".to_string(), "Browser".to_string()));
    }
    if lower.contains("floorp") {
        return Some(("Floorp Browser".to_string(), "Browser".to_string()));
    }
    if lower.contains("zen_browser") || lower.contains("zen-browser") || lower.contains("app.zen_browser") {
        return Some(("Zen Browser".to_string(), "Browser".to_string()));
    }
    if lower.contains("firefox") {
        return Some(("Firefox".to_string(), "Browser".to_string()));
    }
    if lower.contains("vivaldi") {
        return Some(("Vivaldi".to_string(), "Browser".to_string()));
    }
    if lower.contains("opera") {
        return Some(("Opera".to_string(), "Browser".to_string()));
    }
    if lower.contains("thorium") {
        return Some(("Thorium Browser".to_string(), "Browser".to_string()));
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
    if lower.contains("epiphany") || lower.contains("org.gnome.epiphany") {
        return Some(("GNOME Web".to_string(), "Browser".to_string()));
    }

    // 2. IDEs, Editors & Development Tools
    if lower.contains("antigravity") {
        return Some(("Antigravity".to_string(), "Development".to_string()));
    }
    if lower.contains("cursor") && !lower.contains("xcursor") && !lower.contains("wayland-cursor") {
        return Some(("Cursor IDE".to_string(), "Development".to_string()));
    }
    if (lower.contains("code") && (lower.contains("visual") || lower.contains("vscode") || lower.contains("app-gnome-code") || lower.contains("snap.code") || lower == "code"))
        || lower.contains("vscodium")
    {
        return Some(("VS Code".to_string(), "Development".to_string()));
    }
    if lower.contains("zed") && (lower.starts_with("app-") || lower.contains("dev.zed")) {
        return Some(("Zed Editor".to_string(), "Development".to_string()));
    }
    if lower.contains("jetbrains") || lower.contains("intellij") || lower.contains("pycharm") || lower.contains("clion") || lower.contains("webstorm") || lower.contains("goland") || lower.contains("rustrover") {
        return Some(("JetBrains IDE".to_string(), "Development".to_string()));
    }
    if lower.contains("nvim") || lower.contains("neovim") {
        return Some(("Neovim".to_string(), "Development".to_string()));
    }
    if lower.contains("emacs") {
        return Some(("Emacs".to_string(), "Development".to_string()));
    }
    if lower.contains("docker") || lower.contains("containerd") || lower.contains("podman") || lower.contains("buildkit") {
        return Some(("Docker / Containers".to_string(), "Development".to_string()));
    }
    if lower.contains("ollama") {
        return Some(("Ollama AI Server".to_string(), "Development".to_string()));
    }
    if lower.contains("prometheus") || lower.contains("alertmanager") || lower.contains("grafana") {
        return Some(("Prometheus / Metrics".to_string(), "Development".to_string()));
    }
    if lower.contains("mysqld") || lower.contains("mariadb") || lower.contains("postgres") || lower.contains("redis-server") || lower.contains("mongod") {
        return Some(("Database Server (SQL/NoSQL)".to_string(), "Development".to_string()));
    }

    // 3. Terminals
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
    if lower.contains("konsole") {
        return Some(("KDE Konsole".to_string(), "Terminal".to_string()));
    }
    if lower.contains("foot") && (lower.starts_with("app-") || lower == "foot" || lower == "footclient") {
        return Some(("Foot Terminal".to_string(), "Terminal".to_string()));
    }
    if lower.contains("tilix") || lower.contains("terminator") || lower.contains("xfce4-terminal") || lower.contains("cosmic-term") {
        return Some(("Terminal Emulator".to_string(), "Terminal".to_string()));
    }
    if lower.contains("gnome-terminal") || lower.contains("org.gnome.terminal") || lower.contains("ptyxis") || lower.contains("kgx") {
        return Some(("GNOME Terminal".to_string(), "Terminal".to_string()));
    }

    // 4. Productivity, Notes & Office
    if lower.contains("obsidian") {
        return Some(("Obsidian".to_string(), "Productivity".to_string()));
    }
    if lower.contains("notion") || lower.contains("logseq") || lower.contains("joplin") || lower.contains("anytype") {
        return Some(("Notes & Knowledge".to_string(), "Productivity".to_string()));
    }
    if lower.contains("libreoffice") || lower.contains("soffice.bin") || lower.contains("onlyoffice") {
        return Some(("Office Suite".to_string(), "Productivity".to_string()));
    }
    if lower.contains("thunderbird") || lower.contains("geary") || lower.contains("mailspring") {
        return Some(("Mail Client".to_string(), "Productivity".to_string()));
    }

    // 5. Desktop Environments & Window Managers (GNOME, KDE Plasma, COSMIC, Hyprland, Sway, X11, XFCE)
    if lower.contains("org.gnome.shell") && !lower.contains("calendar") && !lower.contains("notifications") || lower == "gnome-shell" {
        return Some(("GNOME Shell".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("plasmashell") || lower.contains("kwin_wayland") || lower.contains("kwin_x11") || lower.contains("plasma-") || lower.contains("kded") {
        return Some(("KDE Plasma Desktop".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("hyprland") {
        return Some(("Hyprland Compositor".to_string(), "Desktop UI".to_string()));
    }
    if lower == "sway" || lower.contains("swaybg") || lower.contains("swaync") {
        return Some(("Sway Compositor".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("cosmic-comp") || lower.contains("cosmic-panel") || lower.contains("cosmic-applets") {
        return Some(("COSMIC Desktop".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("waybar") || lower.contains("polybar") || lower.contains("eww") || lower.contains("ags") {
        return Some(("Status Bar (Waybar/Polybar)".to_string(), "Desktop UI".to_string()));
    }
    if lower == "xorg" || lower == "xwayland" || lower.contains("picom") || lower.contains("i3") || lower.contains("bspwm") || lower.contains("awesome") || lower.contains("xfwm4") || lower.contains("xfce4-") || lower.contains("cinnamon") || lower.contains("marco") {
        return Some(("Window Manager / XServer".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("org.gnome.settingsdaemon") || lower.contains("gnome-session") {
        return Some(("GNOME Settings & Session".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("xdg-desktop-portal") || lower.contains("xdg-document-portal") || lower.contains("flatpak-portal") || lower.contains("flatpak-session") {
        return Some(("XDG Desktop Portals".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("ibus") || lower.contains("fcitx") {
        return Some(("Input Method (IBus/Fcitx)".to_string(), "Desktop UI".to_string()));
    }

    // 6. Audio, Video & Media
    if lower.contains("pipewire") || lower.contains("wireplumber") || lower.contains("pulseaudio") {
        return Some(("PipeWire / PulseAudio".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("spotify") {
        return Some(("Spotify".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("vlc") {
        return Some(("VLC Media Player".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("mpv") || lower.contains("celluloid") || lower.contains("haruna") {
        return Some(("mpv Video Player".to_string(), "Audio & Media".to_string()));
    }
    if lower.contains("obs") && (lower.contains("studio") || lower == "obs") {
        return Some(("OBS Studio".to_string(), "Audio & Media".to_string()));
    }

    // 7. Chat & Social
    if lower.contains("telegram") {
        return Some(("Telegram".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("discord") || lower.contains("vesktop") || lower.contains("armcord") || lower.contains("webcord") {
        return Some(("Discord".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("slack") {
        return Some(("Slack".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("signal") {
        return Some(("Signal Desktop".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("zoom") || lower.contains("teams") {
        return Some(("Video Conferencing".to_string(), "Social & Chat".to_string()));
    }

    // 8. Gaming
    if lower.contains("steam") || lower.contains("gamescope") || lower.contains("lutris") || lower.contains("heroic") || lower.contains("mangohud") {
        return Some(("Steam / Gaming".to_string(), "Gaming".to_string()));
    }

    // 9. Connectivity & Background Daemons across all Linux distros
    if lower.contains("kdeconnect") {
        return Some(("KDE Connect".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("blueman") || lower.contains("bluetooth") || lower.contains("obex") {
        return Some(("Bluetooth".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("networkmanager") || lower.contains("wpa_supplicant") || lower.contains("iwd") || lower.contains("systemd-resolved") || lower.contains("systemd-networkd") || lower.contains("tailscaled") || lower.contains("wireguard") || lower.contains("openvpn") {
        return Some(("Network & Wi-Fi".to_string(), "System".to_string()));
    }
    if lower.contains("gnome-software") || lower.contains("plasma-discover") || lower.contains("packagekit") || lower.contains("fwupd") || lower.contains("snapd") || lower.contains("dnf") || lower.contains("apt") || lower.contains("unattended-upgrade") || lower.contains("pacman") {
        return Some(("Package & Store Manager".to_string(), "System".to_string()));
    }
    if lower.contains("localsearch") || lower.contains("tracker-miner") || lower.contains("baloo_file") || lower.contains("kde-baloo") {
        return Some(("Desktop File Indexer".to_string(), "Background".to_string()));
    }
    if lower.contains("evolution-") || lower.contains("calendarserver") || lower.contains("akonadi") {
        return Some(("Calendar & Contacts Sync".to_string(), "Background".to_string()));
    }
    if lower.contains("gvfs") || lower.contains("udisks") || lower.contains("kio") {
        return Some(("Storage & Virtual FS".to_string(), "Background".to_string()));
    }
    if lower.contains("dbus") {
        return Some(("D-Bus Message Broker".to_string(), "System".to_string()));
    }
    if lower.contains("upower") || lower.contains("power-profiles-daemon") || lower.contains("thermald") || lower.contains("tuned") || lower.contains("tlp") || lower.contains("auto-cpufreq") {
        return Some(("Power Management".to_string(), "System".to_string()));
    }
    if lower.contains("systemd-journald") || lower.contains("systemd-udevd") || lower.contains("systemd-logind") || lower.contains("systemd-oomd") || lower.contains("auditd") || lower.contains("rsyslog") {
        return Some(("System Core & Logging".to_string(), "System".to_string()));
    }
    if lower.contains("polkit") || lower.contains("accounts-daemon") || lower.contains("sssd") || lower.contains("uresourced") || lower.contains("rtkit") || lower.contains("gnome-keyring") || lower.contains("kwalletd") {
        return Some(("Security, Auth & Keyring".to_string(), "System".to_string()));
    }

    // 10. Generic parsing for Snap (`snap.<pkg>.<app>.scope`), Flatpak (`app-flatpak-<id>-<pid>.scope`),
    // systemd desktop scopes (`app-<de>-<name>-<pid>.scope`), and Nix store binaries
    let mut base = unescaped
        .trim_end_matches(".scope")
        .trim_end_matches(".service")
        .trim_end_matches(".slice")
        .to_string();

    if let Some(rest) = base.strip_prefix("snap.") {
        let segs: Vec<&str> = rest.split('.').collect();
        if let Some(&first) = segs.first() {
            base = first.to_string();
        }
    }

    // Strip trailing `-<digits>` (PID or instance ID)
    if let Some(idx) = base.rfind('-') {
        let tail = &base[idx + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c.is_ascii_hexdigit()) && tail.len() >= 3 {
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
        "app-kde-",
        "app-plasma-",
        "app-systemd-",
        "app-",
        "dbus-:1.",
    ] {
        if let Some(rest) = base.strip_prefix(prefix) {
            base = rest.to_string();
            break;
        }
    }

    // Reverse-DNS cleanup (e.g. `org.mozilla.Thunderbird` or `com.obsproject.Studio`)
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
                    && l != "dev"
                    && l != "app"
                    && l != "client"
                    && l != "desktop"
                    && l != "freedesktop"
                    && l != "gnome"
                    && l != "kde"
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

fn collect_cgroup_pids(dir: &Path, out_pids: &mut HashSet<u32>) {
    if let Ok(content) = fs::read_to_string(dir.join("cgroup.procs")) {
        for line in content.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                out_pids.insert(pid);
            }
        }
    }
}

fn scan_slice_dir(
    slice_dir: &Path,
    slice_type: &str,
    map: &mut HashMap<String, RawAppCounter>,
    accounted_pids: &mut HashSet<u32>,
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
            scan_slice_dir(&path, slice_type, map, accounted_pids);
            continue;
        }
        // Generic login session scopes (`session-1.scope`, `session-c1.scope`) on non-systemd-desktop
        // WMs (Sway, Hyprland, i3, XFCE) hold many distinct user apps inside one scope;
        // let `/proc` per-process scanner split those into individual applications.
        if unit_name.starts_with("session-") && unit_name.ends_with(".scope") {
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
        let frozen = read_cgroup_u64(&path, "cgroup.freeze") == 1 && pids_count > 0;

        collect_cgroup_pids(&path, accounted_pids);

        let entry_counter = map.entry(app_name.clone()).or_insert_with(|| RawAppCounter {
            app_name,
            category,
            unit_hint: unit_name.clone(),
            usage_usec: 0,
            user_usec: 0,
            system_usec: 0,
            memory_bytes: 0,
            pids_count: 0,
            is_frozen: false,
        });
        entry_counter.usage_usec = entry_counter.usage_usec.saturating_add(usage_usec);
        entry_counter.user_usec = entry_counter.user_usec.saturating_add(user_usec);
        entry_counter.system_usec = entry_counter.system_usec.saturating_add(system_usec);
        entry_counter.memory_bytes = entry_counter.memory_bytes.saturating_add(memory_bytes);
        entry_counter.pids_count = entry_counter.pids_count.saturating_add(pids_count);
        if frozen {
            entry_counter.is_frozen = true;
        }
    }
}

/// Reads cumulative CPU microseconds, memory, and PID counts for all running apps and services
/// across all Linux distributions:
/// - Uses systemd cgroup v2 (`app.slice`, `session.slice`, `background.slice`, `system.slice`) when present
/// - Also scans `/proc/[pid]` for processes running in standalone window managers (Hyprland, Sway, i3, XFCE),
///   generic login scopes (`session-*.scope`), or non-systemd distros (Void, Alpine, Gentoo OpenRC, Artix).
pub fn read_all_app_counters() -> HashMap<String, RawAppCounter> {
    let mut map = HashMap::new();
    let mut accounted_pids = HashSet::new();

    let uid = unsafe { libc::getuid() };
    let user_slice_dir = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice");
    let user_service_dir = format!("{user_slice_dir}/user@{uid}.service");
    let user_base = Path::new(&user_service_dir);

    let mut had_user_app_slice = false;
    if user_base.exists() {
        let app_slice = user_base.join("app.slice");
        if app_slice.exists() {
            scan_slice_dir(&app_slice, "app", &mut map, &mut accounted_pids);
            had_user_app_slice = !map.is_empty();
        }
        scan_slice_dir(&user_base.join("session.slice"), "session", &mut map, &mut accounted_pids);
        scan_slice_dir(&user_base.join("background.slice"), "background", &mut map, &mut accounted_pids);
    }

    let system_slice = Path::new("/sys/fs/cgroup/system.slice");
    if system_slice.exists() {
        scan_slice_dir(system_slice, "system", &mut map, &mut accounted_pids);
    }

    // If user apps aren't organized in systemd `app.slice` (e.g. non-systemd distros, X11/Wayland
    // standalone WMs like Sway/Hyprland/i3/XFCE, or container/sandbox environments), scan `/proc`
    // for any unaccounted processes.
    if !had_user_app_slice || map.is_empty() {
        scan_proc_processes(&mut map, &accounted_pids);
    }

    map
}

fn resolve_proc_app_name(pid_dir: &Path, comm: &str) -> (String, String) {
    // Check /proc/[pid]/cmdline for browser/Electron/Flatpak/Snap/Nix executable names
    let cmdline = fs::read_to_string(pid_dir.join("cmdline"))
        .unwrap_or_default()
        .replace('\0', " ");

    // Handle Firefox / Waterfox / Zen / LibreWolf content subprocesses (`Isolated Web Co`, `WebExtensions`, `RDD Process`, `Socket Process`)
    if matches!(
        comm,
        "Isolated Web Co" | "Web Content" | "WebExtensions" | "Privileged Cont" | "RDD Process" | "Socket Process" | "Utility Process"
    ) {
        let lower_cmd = cmdline.to_lowercase();
        if lower_cmd.contains("waterfox") {
            return ("Waterfox".to_string(), "Browser".to_string());
        } else if lower_cmd.contains("librewolf") {
            return ("LibreWolf".to_string(), "Browser".to_string());
        } else if lower_cmd.contains("zen") {
            return ("Zen Browser".to_string(), "Browser".to_string());
        } else {
            return ("Firefox".to_string(), "Browser".to_string());
        }
    }

    if let Some(first_arg) = cmdline.split_whitespace().next() {
        let bin_name = Path::new(first_arg)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| comm.to_string());
        if let Some(pair) = normalize_cgroup_unit(&bin_name, "app") {
            return pair;
        }
    }

    normalize_cgroup_unit(comm, "app").unwrap_or_else(|| (comm.to_string(), "App".to_string()))
}

fn scan_proc_processes(map: &mut HashMap<String, RawAppCounter>, accounted_pids: &HashSet<u32>) {
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let usec_per_tick = 1_000_000 / clk_tck;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if accounted_pids.contains(&pid) {
            continue;
        }

        let pid_dir = entry.path();
        // Skip processes already inside a systemd app.slice/session.slice/system.slice if cgroup v2 is active
        if !accounted_pids.is_empty() {
            if let Ok(cg) = fs::read_to_string(pid_dir.join("cgroup")) {
                if cg.contains("/app.slice/")
                    || cg.contains("/session.slice/")
                    || cg.contains("/background.slice/")
                    || cg.contains("/system.slice/")
                {
                    continue;
                }
            }
        }

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
        let state_char = after[0];
        let utime = after[11].parse::<u64>().unwrap_or(0) * usec_per_tick;
        let stime = after[12].parse::<u64>().unwrap_or(0) * usec_per_tick;
        let rss_pages = after[21].parse::<u64>().unwrap_or(0);
        let usage = utime + stime;
        if usage == 0 {
            continue;
        }
        let (app_name, category) = resolve_proc_app_name(&pid_dir, comm);
        let counter = map.entry(app_name.clone()).or_insert_with(|| RawAppCounter {
            app_name,
            category,
            unit_hint: format!("proc:{comm}"),
            usage_usec: 0,
            user_usec: 0,
            system_usec: 0,
            memory_bytes: 0,
            pids_count: 0,
            is_frozen: false,
        });
        counter.usage_usec += usage;
        counter.user_usec += utime;
        counter.system_usec += stime;
        counter.memory_bytes += rss_pages * 4096;
        counter.pids_count += 1;
        if state_char == "T" {
            counter.is_frozen = true;
        }
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
                    curr.usage_usec
                }
            } else {
                curr.usage_usec.min((dt_secs * 2_000_000.0) as u64)
            };

            total_delta_usec = total_delta_usec.saturating_add(delta_usec);
            app_deltas.push((curr.clone(), delta_usec));
        }

        self.prev_counters = current;

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
                is_frozen: curr.is_frozen,
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
/// or `/proc` counters since system boot.
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
            continue;
        }
        let share = (c.usage_usec as f64) / (total_usec as f64);
        let cpu_secs = (c.usage_usec as f64) / 1_000_000.0;
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
            is_frozen: c.is_frozen,
        });
    }

    list.sort_by(|a, b| {
        b.battery_pct
            .partial_cmp(&a.battery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    list
}
