use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppRule {
    pub pattern: String,
    pub name: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct UserAppsFile {
    pub rules: Vec<UserAppRule>,
}

static USER_APPS_CACHE: Mutex<Option<(Option<SystemTime>, Vec<UserAppRule>)>> = Mutex::new(None);

pub fn user_apps_file_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".config")
        })
        .join("rustybattery");
    dir.join("apps.toml")
}

pub fn load_user_app_rules() -> Vec<UserAppRule> {
    let path = user_apps_file_path();
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Ok(guard) = USER_APPS_CACHE.lock() {
        if let Some((cached_mtime, rules)) = &*guard {
            if *cached_mtime == mtime {
                return rules.clone();
            }
        }
    }
    let rules = fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<UserAppsFile>(&s).ok())
        .map(|f| f.rules)
        .unwrap_or_default();
    if let Ok(mut guard) = USER_APPS_CACHE.lock() {
        *guard = Some((mtime, rules.clone()));
    }
    rules
}

fn default_confidence() -> String {
    "modelled".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Known(HashSet<String>),
    Unknown,
}

impl Focus {
    pub fn contains(&self, app_name: &str) -> bool {
        match self {
            Focus::Known(set) => set.contains(app_name),
            Focus::Unknown => false,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Focus::Unknown)
    }
}

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
    pub is_focused: bool,
    /// Per-unit / per-process CPU counter (`unit_key -> usage_usec`) so exiting containers/tabs
    /// never cause aggregate counter drops.
    #[serde(default)]
    pub unit_usage: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDeltaUsage {
    pub app_name: String,
    pub category: String,
    pub unit_hint: String,
    /// Battery percentage points drained by this app during the interval
    pub battery_pct: f64,
    /// Battery percentage points drained while this app's window was focused in the foreground
    pub focused_battery_pct: f64,
    /// Energy consumed in Wh by this app during the interval
    pub energy_wh: f64,
    /// CPU time consumed in seconds during the interval
    pub cpu_secs: f64,
    /// Wall-clock active seconds during the interval (if app used >0.15% CPU)
    pub active_secs: f64,
    /// Wall-clock seconds this app's window was focused during the interval
    pub focused_secs: f64,
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
    /// Whether this app's window is currently focused
    pub is_focused: bool,
    /// Attribution confidence level: `"measured"` (RAPL hardware counter), `"modelled"` (cgroup CPU model), or `"estimated"` (backlight/idle heuristic)
    #[serde(default = "default_confidence")]
    pub confidence: String,
}

fn strip_systemd_launcher_prefixes(raw: &str) -> String {
    let mut base = raw
        .trim_end_matches(".scope")
        .trim_end_matches(".service")
        .trim_end_matches(".slice")
        .to_string();

    if let Some(idx) = base.find('@') {
        base.truncate(idx);
    }

    // Strip `dbus-:1.<N>-` prefix (e.g. `dbus-:1.2-org.gnome.Nautilus@0.service`)
    if let Some(rest) = base.strip_prefix("dbus-:") {
        if let Some(dash_idx) = rest.find('-') {
            base = rest[dash_idx + 1..].to_string();
        }
    }

    for prefix in [
        "app-flatpak-",
        "app-gnome-",
        "app-kde-",
        "app-plasma-",
        "app-systemd-",
        "app-",
    ] {
        if let Some(rest) = base.strip_prefix(prefix) {
            base = rest.to_string();
            break;
        }
    }

    if let Some(idx) = base.rfind('-') {
        let tail = &base[idx + 1..];
        if tail.len() >= 3 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            base.truncate(idx);
        }
    }
    base
}

fn has_token(tokens: &[&str], target: &str) -> bool {
    tokens.iter().any(|&t| t == target)
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

    let unescaped = unit
        .replace("\\x2d", "-")
        .replace("\\x2e", ".")
        .replace("\\x40", "@");

    let stripped = strip_systemd_launcher_prefixes(&unescaped);
    let lower = stripped.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(['-', '_', '.', '@', '/'])
        .filter(|s| !s.is_empty())
        .collect();

    // 0. User-configured override rules from `~/.config/rustybattery/apps.toml`
    for rule in load_user_app_rules() {
        let pat = rule.pattern.trim().to_lowercase();
        if !pat.is_empty() && (lower == pat || has_token(&tokens, &pat) || lower.contains(&pat)) {
            return Some((rule.name, rule.category));
        }
    }

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
    if lower.contains("zen_browser")
        || lower.contains("zen-browser")
        || lower.contains("app.zen_browser")
    {
        return Some(("Zen Browser".to_string(), "Browser".to_string()));
    }
    if has_token(&tokens, "firefox") {
        return Some(("Firefox".to_string(), "Browser".to_string()));
    }
    if lower.contains("vivaldi") {
        return Some(("Vivaldi".to_string(), "Browser".to_string()));
    }
    if has_token(&tokens, "opera") {
        return Some(("Opera".to_string(), "Browser".to_string()));
    }
    if lower.contains("thorium") {
        return Some(("Thorium Browser".to_string(), "Browser".to_string()));
    }
    if has_token(&tokens, "chromium") {
        return Some(("Chromium".to_string(), "Browser".to_string()));
    }
    if (has_token(&tokens, "chrome") || lower.contains("google-chrome"))
        && !lower.contains("chrome-remote-desktop")
        && !lower.contains("chromecast")
    {
        return Some(("Google Chrome".to_string(), "Browser".to_string()));
    }
    if has_token(&tokens, "brave") {
        return Some(("Brave Browser".to_string(), "Browser".to_string()));
    }
    if lower.contains("epiphany") || lower.contains("org.gnome.epiphany") {
        return Some(("GNOME Web".to_string(), "Browser".to_string()));
    }

    // 2. IDEs, Editors & Development Tools
    if lower.contains("antigravity") {
        return Some(("Antigravity".to_string(), "Development".to_string()));
    }
    if lower.contains("cursor")
        && !lower.contains("xcursor")
        && !lower.contains("wayland-cursor")
    {
        return Some(("Cursor IDE".to_string(), "Development".to_string()));
    }
    if (lower.contains("code")
        && (lower.contains("visual")
            || lower.contains("vscode")
            || lower.contains("app-gnome-code")
            || lower.contains("snap.code")
            || lower == "code"))
        || lower.contains("vscodium")
    {
        return Some(("VS Code".to_string(), "Development".to_string()));
    }
    if lower.contains("zed") && (lower.starts_with("app-") || lower.contains("dev.zed")) {
        return Some(("Zed Editor".to_string(), "Development".to_string()));
    }
    if lower.contains("jetbrains")
        || lower.contains("intellij")
        || lower.contains("pycharm")
        || lower.contains("clion")
        || lower.contains("webstorm")
        || lower.contains("goland")
        || lower.contains("rustrover")
    {
        return Some(("JetBrains IDE".to_string(), "Development".to_string()));
    }
    if lower.contains("nvim") || lower.contains("neovim") {
        return Some(("Neovim".to_string(), "Development".to_string()));
    }
    if lower.contains("emacs") {
        return Some(("Emacs".to_string(), "Development".to_string()));
    }
    if lower.contains("docker")
        || lower.contains("containerd")
        || lower.contains("podman")
        || lower.contains("buildkit")
    {
        return Some(("Docker / Containers".to_string(), "Development".to_string()));
    }
    if lower.contains("ollama") {
        return Some(("Ollama AI Server".to_string(), "Development".to_string()));
    }
    if lower.contains("prometheus") || lower.contains("alertmanager") || lower.contains("grafana") {
        return Some(("Prometheus / Metrics".to_string(), "Development".to_string()));
    }
    if lower.contains("mysqld")
        || lower.contains("mariadb")
        || lower.contains("postgres")
        || lower.contains("redis-server")
        || lower.contains("mongod")
    {
        return Some((
            "Database Server (SQL/NoSQL)".to_string(),
            "Development".to_string(),
        ));
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
    if lower.contains("foot")
        && (lower.starts_with("app-") || lower == "foot" || lower == "footclient")
    {
        return Some(("Foot Terminal".to_string(), "Terminal".to_string()));
    }
    if lower.contains("tilix")
        || lower.contains("terminator")
        || lower.contains("xfce4-terminal")
        || lower.contains("cosmic-term")
    {
        return Some(("Terminal Emulator".to_string(), "Terminal".to_string()));
    }
    if lower.contains("gnome-terminal")
        || lower.contains("org.gnome.terminal")
        || lower.contains("ptyxis")
        || lower.contains("kgx")
    {
        return Some(("GNOME Terminal".to_string(), "Terminal".to_string()));
    }

    // 4. Productivity, Notes & Office
    if lower.contains("obsidian") {
        return Some(("Obsidian".to_string(), "Productivity".to_string()));
    }
    if lower.contains("notion")
        || lower.contains("logseq")
        || lower.contains("joplin")
        || lower.contains("anytype")
    {
        return Some(("Notes & Knowledge".to_string(), "Productivity".to_string()));
    }
    if lower.contains("libreoffice")
        || lower.contains("soffice.bin")
        || lower.contains("onlyoffice")
    {
        return Some(("Office Suite".to_string(), "Productivity".to_string()));
    }
    if lower.contains("thunderbird") || lower.contains("geary") || lower.contains("mailspring") {
        return Some(("Mail Client".to_string(), "Productivity".to_string()));
    }

    // 5. Desktop Environments & Window Managers
    if lower.contains("org.gnome.shell")
        && !lower.contains("calendar")
        && !lower.contains("notifications")
        || lower == "gnome-shell"
    {
        return Some(("GNOME Shell".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("plasmashell")
        || lower.contains("kwin_wayland")
        || lower.contains("kwin_x11")
        || lower.contains("plasma-")
        || lower.contains("kded")
    {
        return Some(("KDE Plasma Desktop".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("hyprland") {
        return Some(("Hyprland Compositor".to_string(), "Desktop UI".to_string()));
    }
    if lower == "sway" || lower.contains("swaybg") || lower.contains("swaync") {
        return Some(("Sway Compositor".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("cosmic-comp")
        || lower.contains("cosmic-panel")
        || lower.contains("cosmic-applets")
    {
        return Some(("COSMIC Desktop".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("waybar")
        || lower.contains("polybar")
        || has_token(&tokens, "eww")
        || has_token(&tokens, "ags")
    {
        return Some((
            "Status Bar (Waybar/Polybar)".to_string(),
            "Desktop UI".to_string(),
        ));
    }
    if lower == "xorg"
        || lower == "xwayland"
        || has_token(&tokens, "picom")
        || has_token(&tokens, "i3")
        || has_token(&tokens, "bspwm")
        || has_token(&tokens, "awesome")
        || lower.contains("xfwm4")
        || lower.contains("xfce4-")
        || lower.contains("cinnamon")
        || has_token(&tokens, "marco")
    {
        return Some((
            "Window Manager / XServer".to_string(),
            "Desktop UI".to_string(),
        ));
    }
    if lower.contains("org.gnome.settingsdaemon") || lower.contains("gnome-session") {
        return Some((
            "GNOME Settings & Session".to_string(),
            "Desktop UI".to_string(),
        ));
    }
    if lower.contains("xdg-desktop-portal")
        || lower.contains("xdg-document-portal")
        || lower.contains("flatpak-portal")
        || lower.contains("flatpak-session")
    {
        return Some(("XDG Desktop Portals".to_string(), "Desktop UI".to_string()));
    }
    if lower.contains("ibus") || lower.contains("fcitx") {
        return Some((
            "Input Method (IBus/Fcitx)".to_string(),
            "Desktop UI".to_string(),
        ));
    }

    // 6. Audio, Video & Media
    if lower.contains("pipewire") || lower.contains("wireplumber") || lower.contains("pulseaudio") {
        return Some((
            "PipeWire / PulseAudio".to_string(),
            "Audio & Media".to_string(),
        ));
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
    if lower.contains("discord")
        || lower.contains("vesktop")
        || lower.contains("armcord")
        || lower.contains("webcord")
    {
        return Some(("Discord".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("slack") {
        return Some(("Slack".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("signal") {
        return Some(("Signal Desktop".to_string(), "Social & Chat".to_string()));
    }
    if lower.contains("zoom") || lower.contains("teams") {
        return Some((
            "Video Conferencing".to_string(),
            "Social & Chat".to_string(),
        ));
    }

    // 8. Gaming
    if lower.contains("steam")
        || lower.contains("gamescope")
        || lower.contains("lutris")
        || lower.contains("heroic")
        || lower.contains("mangohud")
    {
        return Some(("Steam / Gaming".to_string(), "Gaming".to_string()));
    }

    // 9. Connectivity & Background Daemons
    if lower.contains("kdeconnect") {
        return Some(("KDE Connect".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("blueman") || lower.contains("bluetooth") || lower.contains("obex") {
        return Some(("Bluetooth".to_string(), "Connectivity".to_string()));
    }
    if lower.contains("networkmanager")
        || lower.contains("wpa_supplicant")
        || lower.contains("iwd")
        || lower.contains("systemd-resolved")
        || lower.contains("systemd-networkd")
        || lower.contains("tailscaled")
        || lower.contains("wireguard")
        || lower.contains("openvpn")
    {
        return Some(("Network & Wi-Fi".to_string(), "System".to_string()));
    }
    if lower.contains("gnome-software")
        || lower.contains("plasma-discover")
        || lower.contains("packagekit")
        || lower.contains("fwupd")
        || has_token(&tokens, "snapd")
        || has_token(&tokens, "dnf")
        || has_token(&tokens, "apt")
        || lower.contains("unattended-upgrade")
        || has_token(&tokens, "pacman")
    {
        return Some(("Package & Store Manager".to_string(), "System".to_string()));
    }
    if lower.contains("localsearch")
        || lower.contains("tracker-miner")
        || lower.contains("baloo_file")
        || lower.contains("kde-baloo")
    {
        return Some(("Desktop File Indexer".to_string(), "Background".to_string()));
    }
    if lower.contains("evolution-")
        || lower.contains("calendarserver")
        || lower.contains("akonadi")
    {
        return Some((
            "Calendar & Contacts Sync".to_string(),
            "Background".to_string(),
        ));
    }
    if lower.contains("gvfs") || lower.contains("udisks") || has_token(&tokens, "kio") {
        return Some(("Storage & Virtual FS".to_string(), "Background".to_string()));
    }
    if has_token(&tokens, "dbus")
        || lower.starts_with("dbus-daemon")
        || lower.starts_with("dbus-broker")
    {
        return Some(("D-Bus Message Broker".to_string(), "System".to_string()));
    }
    if lower.contains("upower")
        || lower.contains("power-profiles-daemon")
        || lower.contains("thermald")
        || lower.contains("tuned")
        || lower.contains("tlp")
        || lower.contains("auto-cpufreq")
    {
        return Some(("Power Management".to_string(), "System".to_string()));
    }
    if lower.contains("systemd-journald")
        || lower.contains("systemd-udevd")
        || lower.contains("systemd-logind")
        || lower.contains("systemd-oomd")
        || lower.contains("auditd")
        || lower.contains("rsyslog")
    {
        return Some(("System Core & Logging".to_string(), "System".to_string()));
    }
    if lower.contains("polkit")
        || lower.contains("accounts-daemon")
        || lower.contains("sssd")
        || lower.contains("uresourced")
        || lower.contains("rtkit")
        || lower.contains("gnome-keyring")
        || lower.contains("kwalletd")
    {
        return Some(("Security, Auth & Keyring".to_string(), "System".to_string()));
    }

    let mut base = stripped;

    if let Some(rest) = base.strip_prefix("snap.") {
        let segs: Vec<&str> = rest.split('.').collect();
        if let Some(&first) = segs.first() {
            base = first.to_string();
        }
    }

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

/// Detects the currently focused window's application name across Linux desktop environments:
/// 1. GNOME / Fedora / Ubuntu / Arch `uresourced` cgroup v2 boost (`cpu.weight > 100` or `memory.min > 0` in `app.slice`)
/// 2. Hyprland (`hyprctl activewindow -j`)
/// 3. Sway (`swaymsg -t get_tree`)
/// 4. KDE Plasma (`kdotool`)
/// 5. X11 / XWayland (`xprop -root _NET_ACTIVE_WINDOW`)
fn scan_focused_slice_dir(dir: &Path, focused: &mut HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let unit_name = entry.file_name().to_string_lossy().to_string();
        if unit_name.ends_with(".slice") {
            scan_focused_slice_dir(&path, focused);
            continue;
        }
        let weight = read_cgroup_u64(&path, "cpu.weight");
        if weight > 100 {
            if let Some((app_name, _)) = normalize_cgroup_unit(&unit_name, "app") {
                focused.insert(app_name);
            }
        }
    }
}

fn find_sway_focused_class(val: &serde_json::Value) -> Option<String> {
    if val.get("focused").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(app_id) = val.get("app_id").and_then(|v| v.as_str()) {
            if !app_id.is_empty() {
                return Some(app_id.to_string());
            }
        }
        if let Some(cls) = val
            .get("window_properties")
            .and_then(|w| w.get("class"))
            .and_then(|v| v.as_str())
        {
            if !cls.is_empty() {
                return Some(cls.to_string());
            }
        }
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
            for child in arr {
                if let Some(found) = find_sway_focused_class(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

pub fn detect_focused_apps() -> Focus {
    let mut focused = HashSet::new();

    // 1. Check cgroup v2 `cpu.weight > 100` in user `app.slice` (set by `uresourced` on GNOME/systemd desktops)
    let uid = unsafe { libc::getuid() };
    let app_slice_path = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ));
    scan_focused_slice_dir(&app_slice_path, &mut focused);

    if !focused.is_empty() {
        return Focus::Known(focused);
    }

    static SUBPROC_FOCUS_CACHE: std::sync::Mutex<
        Option<(std::time::Instant, Focus)>,
    > = std::sync::Mutex::new(None);
    if let Ok(guard) = SUBPROC_FOCUS_CACHE.lock() {
        if let Some((ts, cached)) = &*guard {
            if ts.elapsed().as_millis() < 2500 {
                return cached.clone();
            }
        }
    }

    // 2. Check Hyprland (`HYPRLAND_INSTANCE_SIGNATURE`)
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        if let Ok(out) = Command::new("hyprctl").args(["activewindow", "-j"]).output() {
            if out.status.success() {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    if let Some(cls) = val.get("class").and_then(|v| v.as_str()) {
                        if let Some((app_name, _)) = normalize_cgroup_unit(cls, "app") {
                            focused.insert(app_name);
                        }
                    }
                }
            }
        }
    }

    // 3. Check Sway (`SWAYSOCK` via `swaymsg -t get_tree`)
    if focused.is_empty() && std::env::var_os("SWAYSOCK").is_some() {
        if let Ok(out) = Command::new("swaymsg").args(["-t", "get_tree"]).output() {
            if out.status.success() {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    if let Some(cls) = find_sway_focused_class(&val) {
                        if let Some((app_name, _)) = normalize_cgroup_unit(&cls, "app") {
                            focused.insert(app_name);
                        }
                    }
                }
            }
        }
    }

    // 4. Check KDE Plasma `kdotool` if available
    if focused.is_empty()
        && std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_lowercase()
            .contains("kde")
    {
        if let Ok(out) = Command::new("kdotool")
            .args(["getactivewindow", "getwindowclassname"])
            .output()
        {
            if out.status.success() {
                let cls = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if let Some((app_name, _)) = normalize_cgroup_unit(&cls, "app") {
                    focused.insert(app_name);
                }
            }
        }
    }

    // 5. Check X11 `_NET_ACTIVE_WINDOW` via `xprop`
    if focused.is_empty() && std::env::var_os("DISPLAY").is_some() {
        if let Ok(out) = Command::new("xprop")
            .args(["-root", "_NET_ACTIVE_WINDOW"])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(win_id) = s.split_whitespace().last() {
                    if win_id.starts_with("0x") && win_id != "0x0" {
                        if let Ok(w_out) = Command::new("xprop")
                            .args(["-id", win_id, "WM_CLASS"])
                            .output()
                        {
                            let ws = String::from_utf8_lossy(&w_out.stdout);
                            if ws.contains('=') {
                                for part in ws.split('"').skip(1).step_by(2) {
                                    if let Some((app_name, _)) = normalize_cgroup_unit(part, "app")
                                    {
                                        focused.insert(app_name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let result = if focused.is_empty() {
        Focus::Unknown
    } else {
        Focus::Known(focused)
    };

    if let Ok(mut guard) = SUBPROC_FOCUS_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), result.clone()));
    }

    result
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
    focused_set: &Focus,
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
            scan_slice_dir(&path, slice_type, map, focused_set);
            continue;
        }
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
        let is_focused = focused_set.contains(&app_name);
        let unit_key = path.to_string_lossy().to_string();

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
            is_focused: false,
            unit_usage: HashMap::new(),
        });
        entry_counter.usage_usec = entry_counter.usage_usec.saturating_add(usage_usec);
        entry_counter.user_usec = entry_counter.user_usec.saturating_add(user_usec);
        entry_counter.system_usec = entry_counter.system_usec.saturating_add(system_usec);
        entry_counter.memory_bytes = entry_counter.memory_bytes.saturating_add(memory_bytes);
        entry_counter.pids_count = entry_counter.pids_count.saturating_add(pids_count);
        entry_counter.unit_usage.insert(unit_key, usage_usec);
        if frozen {
            entry_counter.is_frozen = true;
        }
        if is_focused {
            entry_counter.is_focused = true;
        }
    }
}

/// Reads cumulative CPU microseconds, memory, PID counts, and focus state for all running apps and services.
pub fn read_all_app_counters() -> HashMap<String, RawAppCounter> {
    let mut map = HashMap::new();
    let focused_set = detect_focused_apps();

    let uid = unsafe { libc::getuid() };
    let user_slice_dir = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice");
    let user_service_dir = format!("{user_slice_dir}/user@{uid}.service");
    let user_base = Path::new(&user_service_dir);

    let mut had_user_app_slice = false;
    if user_base.exists() {
        let app_slice = user_base.join("app.slice");
        if app_slice.exists() {
            scan_slice_dir(&app_slice, "app", &mut map, &focused_set);
            had_user_app_slice = !map.is_empty();
        }
        scan_slice_dir(
            &user_base.join("session.slice"),
            "session",
            &mut map,
            &focused_set,
        );
        scan_slice_dir(
            &user_base.join("background.slice"),
            "background",
            &mut map,
            &focused_set,
        );
    }

    let system_slice = Path::new("/sys/fs/cgroup/system.slice");
    if system_slice.exists() {
        scan_slice_dir(system_slice, "system", &mut map, &focused_set);
    }

    if !had_user_app_slice || map.is_empty() {
        scan_proc_processes(&mut map, &focused_set);
    }

    map
}

pub fn resolve_proc_app_name(pid_dir: &Path, comm: &str) -> (String, String) {
    let cmdline = fs::read_to_string(pid_dir.join("cmdline"))
        .unwrap_or_default()
        .replace('\0', " ");
    let first_bin = cmdline
        .split_whitespace()
        .next()
        .and_then(|arg| Path::new(arg).file_name())
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if matches!(
        comm,
        "Isolated Web Co"
            | "Web Content"
            | "WebExtensions"
            | "Privileged Cont"
            | "RDD Process"
            | "Socket Process"
            | "Utility Process"
    ) {
        if first_bin.contains("waterfox") {
            return ("Waterfox".to_string(), "Browser".to_string());
        } else if first_bin.contains("librewolf") {
            return ("LibreWolf".to_string(), "Browser".to_string());
        } else if first_bin == "zen" || first_bin.contains("zen-browser") {
            return ("Zen Browser".to_string(), "Browser".to_string());
        } else {
            return ("Firefox".to_string(), "Browser".to_string());
        }
    }

    if !first_bin.is_empty() {
        if let Some(pair) = normalize_cgroup_unit(&first_bin, "app") {
            return pair;
        }
    }

    normalize_cgroup_unit(comm, "app").unwrap_or_else(|| (comm.to_string(), "App".to_string()))
}

fn scan_proc_processes(
    map: &mut HashMap<String, RawAppCounter>,
    focused_set: &Focus,
) {
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let usec_per_tick = 1_000_000 / clk_tck;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let pid_dir = entry.path();
        if let Ok(cg) = fs::read_to_string(pid_dir.join("cgroup")) {
            if cg.contains("/app.slice/")
                || cg.contains("/session.slice/")
                || cg.contains("/background.slice/")
                || cg.contains("/system.slice/")
            {
                continue;
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
        let starttime = after[19].parse::<u64>().unwrap_or(0);
        let rss_pages = after[21].parse::<u64>().unwrap_or(0);
        let usage = utime + stime;
        if usage == 0 {
            continue;
        }
        let (app_name, category) = resolve_proc_app_name(&pid_dir, comm);
        let is_focused = focused_set.contains(&app_name);
        let unit_key = format!("proc:{pid}:{starttime}");
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
            is_focused: false,
            unit_usage: HashMap::new(),
        });
        counter.usage_usec += usage;
        counter.user_usec += utime;
        counter.system_usec += stime;
        counter.memory_bytes += rss_pages * page_size;
        counter.pids_count += 1;
        counter.unit_usage.insert(unit_key, usage);
        if state_char == "T" {
            counter.is_frozen = true;
        }
        if is_focused {
            counter.is_focused = true;
        }
    }
}

/// Default maximum seconds a single awake tick can attribute (`max(15, 3 * interval)`).
const DEFAULT_MAX_SAMPLE_DT_SECS: f64 = 45.0;

#[derive(Debug, Clone)]
pub struct SampleInput {
    pub raw_dt_secs: f64,
    pub slept_delta_secs: f64,
    pub power_w: f64,
    pub energy_full_wh: f64,
    pub is_discharging: bool,
    pub raw_display_w: f64,
    pub rapl_power_w: Option<f64>,
    pub cpu_w_per_core: f64,
    pub baseline_max_w: f64,
}

/// Stateful tracker that computes per-tick deltas between snapshots and attributes battery drain.
pub struct AppEnergyTracker {
    prev_counters: HashMap<String, RawAppCounter>,
    prev_monotonic_s: f64,
    prev_suspended_s: f64,
    prev_rapl_uj: Option<u64>,
    pub max_dt_secs: f64,
}

impl Default for AppEnergyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AppEnergyTracker {
    pub fn new() -> Self {
        let initial = read_all_app_counters();
        Self {
            prev_counters: initial,
            prev_monotonic_s: crate::battery::monotonic_secs(),
            prev_suspended_s: crate::battery::suspended_total_secs(),
            prev_rapl_uj: crate::battery::read_rapl_package_energy_uj(),
            max_dt_secs: DEFAULT_MAX_SAMPLE_DT_SECS,
        }
    }

    pub fn with_interval(interval_secs: u64) -> Self {
        let mut t = Self::new();
        t.max_dt_secs = (interval_secs.saturating_mul(3).max(15)) as f64;
        t
    }

    /// Samples current app counters, computes per-unit CPU delta over monotonic `dt_secs`,
    /// and attributes battery energy across active apps, Display & Backlight, System Baseline,
    /// and Unattributed / Hardware & Idle.
    pub fn sample_delta(
        &mut self,
        power_w: f64,
        energy_full_wh: f64,
        is_discharging: bool,
    ) -> (Vec<AppDeltaUsage>, f64) {
        let now_mono = crate::battery::monotonic_secs();
        let now_susp = crate::battery::suspended_total_secs();
        let raw_dt = (now_mono - self.prev_monotonic_s).max(0.1);
        let slept_delta = (now_susp - self.prev_suspended_s).max(0.0);
        self.prev_monotonic_s = now_mono;
        self.prev_suspended_s = now_susp;

        let now_rapl = crate::battery::read_rapl_package_energy_uj();
        let rapl_power_w = match (self.prev_rapl_uj, now_rapl) {
            (Some(prev), Some(curr)) if curr >= prev && raw_dt >= 0.5 => {
                let w = ((curr - prev) as f64 / 1_000_000.0) / raw_dt;
                if w > 0.05 && w < 200.0 {
                    Some(w)
                } else {
                    None
                }
            }
            _ => None,
        };
        self.prev_rapl_uj = now_rapl;

        let cfg = crate::autopilot::load_autopilot_config();
        let current = read_all_app_counters();
        let display_w = crate::battery::estimate_display_power_w();
        self.compute_delta_from_snapshots(
            current,
            SampleInput {
                raw_dt_secs: raw_dt,
                slept_delta_secs: slept_delta,
                power_w,
                energy_full_wh,
                is_discharging,
                raw_display_w: display_w,
                rapl_power_w,
                cpu_w_per_core: cfg.calibrated_cpu_w_per_core,
                baseline_max_w: cfg.calibrated_baseline_w,
            },
        )
    }

    pub fn compute_delta_from_snapshots(
        &mut self,
        current: HashMap<String, RawAppCounter>,
        inp: SampleInput,
    ) -> (Vec<AppDeltaUsage>, f64) {
        if inp.slept_delta_secs >= 5.0 {
            self.prev_counters = current;
            return (Vec::new(), inp.raw_dt_secs.min(self.max_dt_secs));
        }

        let dt_secs = inp.raw_dt_secs.clamp(0.1, self.max_dt_secs);
        let new_unit_cap = (dt_secs * 2_000_000.0) as u64;
        let mut app_deltas: Vec<(RawAppCounter, u64)> = Vec::new();
        let mut total_delta_usec: u64 = 0;

        for (name, curr) in &current {
            let delta_usec = if let Some(prev) = self.prev_counters.get(name) {
                if !curr.unit_usage.is_empty() || !prev.unit_usage.is_empty() {
                    let mut sum = 0u64;
                    for (ukey, &u_curr) in &curr.unit_usage {
                        let d = match prev.unit_usage.get(ukey) {
                            Some(&u_prev) if u_curr >= u_prev => u_curr - u_prev,
                            Some(_) => u_curr,
                            None => u_curr.min(new_unit_cap),
                        };
                        sum = sum.saturating_add(d);
                    }
                    sum
                } else if curr.usage_usec >= prev.usage_usec {
                    curr.usage_usec - prev.usage_usec
                } else {
                    0
                }
            } else {
                curr.usage_usec.min(new_unit_cap)
            };

            total_delta_usec = total_delta_usec.saturating_add(delta_usec);
            app_deltas.push((curr.clone(), delta_usec));
        }

        self.prev_counters = current;

        let effective_power_w = inp.power_w.max(0.0);
        let display_w = if inp.is_discharging {
            inp.raw_display_w.min(effective_power_w * 0.45)
        } else {
            0.0
        };
        let base_cap = if inp.baseline_max_w > 0.0 {
            inp.baseline_max_w
        } else {
            1.0
        };
        let baseline_w = if inp.is_discharging {
            base_cap.min((effective_power_w - display_w).max(0.0) * 0.30)
        } else {
            0.0
        };
        let app_pool_w = (effective_power_w - display_w - baseline_w).max(0.0);

        // Use hardware RAPL package power when readable, or calibrated `cpu_w_per_core` (default 12 W/core),
        // placing remaining non-CPU hardware draw into `Unattributed / Hardware & Idle`.
        let cpu_cores_used = (total_delta_usec as f64 / 1_000_000.0) / dt_secs;
        let w_per_core = if inp.cpu_w_per_core > 0.0 {
            inp.cpu_w_per_core
        } else {
            12.0
        };
        let (attributed_app_w, app_confidence) = if inp.is_discharging {
            if let Some(rapl_w) = inp.rapl_power_w {
                (app_pool_w.min(rapl_w), "measured")
            } else {
                (app_pool_w.min(cpu_cores_used * w_per_core), "modelled")
            }
        } else {
            (0.0, "modelled")
        };
        let unattributed_w = if inp.is_discharging {
            (app_pool_w - attributed_app_w).max(0.0)
        } else {
            0.0
        };

        let interval_app_wh = attributed_app_w * (dt_secs / 3600.0);
        let full_wh = inp.energy_full_wh.max(10.0);
        let interval_app_batt_pct = (interval_app_wh / full_wh) * 100.0;

        let mut results = Vec::with_capacity(app_deltas.len() + 3);
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
            let batt_pct = interval_app_batt_pct * share;

            let (focused_battery_pct, focused_secs) = if curr.is_focused {
                (batt_pct, dt_secs)
            } else {
                (0.0, 0.0)
            };

            results.push(AppDeltaUsage {
                app_name: curr.app_name,
                category: curr.category,
                unit_hint: curr.unit_hint,
                battery_pct: batt_pct,
                focused_battery_pct,
                energy_wh: interval_app_wh * share,
                cpu_secs,
                active_secs,
                focused_secs,
                live_power_w: attributed_app_w * share,
                live_cpu_pct,
                memory_mb: (curr.memory_bytes as f64) / (1024.0 * 1024.0),
                pids_count: curr.pids_count,
                is_frozen: curr.is_frozen,
                is_focused: curr.is_focused,
                confidence: app_confidence.to_string(),
            });
        }

        if inp.is_discharging && display_w > 0.05 {
            let disp_wh = display_w * (dt_secs / 3600.0);
            results.push(AppDeltaUsage {
                app_name: "Display & Backlight (est.)".to_string(),
                category: "System".to_string(),
                unit_hint: "sysfs-backlight".to_string(),
                battery_pct: (disp_wh / full_wh) * 100.0,
                focused_battery_pct: 0.0,
                energy_wh: disp_wh,
                cpu_secs: 0.0,
                active_secs: dt_secs,
                focused_secs: 0.0,
                live_power_w: display_w,
                live_cpu_pct: 0.0,
                memory_mb: 0.0,
                pids_count: 0,
                is_frozen: false,
                is_focused: false,
                confidence: "estimated".to_string(),
            });
        }

        if inp.is_discharging && (baseline_w + unattributed_w) > 0.05 {
            let idle_w = baseline_w + unattributed_w;
            let idle_wh = idle_w * (dt_secs / 3600.0);
            results.push(AppDeltaUsage {
                app_name: "Unattributed / Hardware & Idle".to_string(),
                category: "System".to_string(),
                unit_hint: "soc-idle".to_string(),
                battery_pct: (idle_wh / full_wh) * 100.0,
                focused_battery_pct: 0.0,
                energy_wh: idle_wh,
                cpu_secs: 0.0,
                active_secs: dt_secs,
                focused_secs: 0.0,
                live_power_w: idle_w,
                live_cpu_pct: 0.0,
                memory_mb: 0.0,
                pids_count: 0,
                is_frozen: false,
                is_focused: false,
                confidence: "estimated".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_counter(name: &str, usec: u64, focused: bool) -> RawAppCounter {
        let mut unit_usage = HashMap::new();
        unit_usage.insert(format!("{name}.service"), usec);
        RawAppCounter {
            app_name: name.to_string(),
            category: "App".to_string(),
            unit_hint: "app.service".to_string(),
            usage_usec: usec,
            user_usec: usec,
            system_usec: 0,
            memory_bytes: 100 * 1024 * 1024,
            pids_count: 2,
            is_frozen: false,
            is_focused: focused,
            unit_usage,
        }
    }

    #[test]
    fn suspend_gap_skips_app_attribution() {
        let mut init = HashMap::new();
        init.insert("Waterfox".to_string(), fake_counter("Waterfox", 1_000_000, false));
        let mut tracker = AppEnergyTracker {
            prev_counters: init,
            prev_monotonic_s: 100.0,
            prev_suspended_s: 0.0,
            prev_rapl_uj: None,
            max_dt_secs: 45.0,
        };

        let mut next = HashMap::new();
        next.insert("Waterfox".to_string(), fake_counter("Waterfox", 2_000_000, false));
        let (deltas, _) = tracker.compute_delta_from_snapshots(
            next,
            SampleInput {
                raw_dt_secs: 5.0,
                slept_delta_secs: 28_800.0,
                power_w: 18.0,
                energy_full_wh: 60.0,
                is_discharging: true,
                raw_display_w: 2.0,
                rapl_power_w: None,
                cpu_w_per_core: 12.0,
                baseline_max_w: 1.0,
            },
        );
        assert!(deltas.is_empty());
    }

    #[test]
    fn interval_30s_attributes_full_duration_while_capping_huge_pauses() {
        let mut init = HashMap::new();
        init.insert("Code".to_string(), fake_counter("Code", 1_000_000, true));
        let mut tracker = AppEnergyTracker {
            prev_counters: init,
            prev_monotonic_s: 100.0,
            prev_suspended_s: 0.0,
            prev_rapl_uj: None,
            max_dt_secs: 45.0,
        };

        let mut next = HashMap::new();
        next.insert("Code".to_string(), fake_counter("Code", 31_000_000, true));
        let (deltas, dt) = tracker.compute_delta_from_snapshots(
            next,
            SampleInput {
                raw_dt_secs: 30.0,
                slept_delta_secs: 0.0,
                power_w: 12.0,
                energy_full_wh: 60.0,
                is_discharging: true,
                raw_display_w: 2.0,
                rapl_power_w: None,
                cpu_w_per_core: 12.0,
                baseline_max_w: 1.0,
            },
        );
        assert!((dt - 30.0).abs() < 1e-6);
        let total_wh: f64 = deltas.iter().map(|d| d.energy_wh).sum();
        // 12W * (30s / 3600) = 0.10 Wh
        assert!((total_wh - 0.10).abs() < 1e-4);
    }

    #[test]
    fn exiting_sub_unit_does_not_inflate_remaining_unit_delta() {
        // Probe P1-1: Docker has 2 containers (c1 = 100s, c2 = 10s). c1 exits, c2 advances by 0.5s (to 10.5s).
        // Editor advances by 1.0s.
        let mut docker_prev = fake_counter("Docker / Containers", 110_000_000, false);
        docker_prev.unit_usage.clear();
        docker_prev.unit_usage.insert("c1".to_string(), 100_000_000);
        docker_prev.unit_usage.insert("c2".to_string(), 10_000_000);

        let mut docker_curr = fake_counter("Docker / Containers", 10_500_000, false);
        docker_curr.unit_usage.clear();
        docker_curr.unit_usage.insert("c2".to_string(), 10_500_000);

        let mut init = HashMap::new();
        init.insert("Docker / Containers".to_string(), docker_prev);
        init.insert("Code".to_string(), fake_counter("Code", 5_000_000, true));

        let mut tracker = AppEnergyTracker {
            prev_counters: init,
            prev_monotonic_s: 0.0,
            prev_suspended_s: 0.0,
            prev_rapl_uj: None,
            max_dt_secs: 45.0,
        };

        let mut next = HashMap::new();
        next.insert("Docker / Containers".to_string(), docker_curr);
        next.insert("Code".to_string(), fake_counter("Code", 6_000_000, true));

        let (deltas, _) = tracker.compute_delta_from_snapshots(
            next,
            SampleInput {
                raw_dt_secs: 5.0,
                slept_delta_secs: 0.0,
                power_w: 12.0,
                energy_full_wh: 60.0,
                is_discharging: true,
                raw_display_w: 2.0,
                rapl_power_w: None,
                cpu_w_per_core: 12.0,
                baseline_max_w: 1.0,
            },
        );
        let docker = deltas.iter().find(|d| d.app_name == "Docker / Containers").unwrap();
        let code = deltas.iter().find(|d| d.app_name == "Code").unwrap();
        assert!((docker.cpu_secs - 0.5).abs() < 1e-6);
        assert!((code.cpu_secs - 1.0).abs() < 1e-6);
        assert!(code.live_power_w > docker.live_power_w);
    }

    #[test]
    fn idle_machine_puts_power_in_unattributed_instead_of_tiny_process() {
        // Probe P1-2: one process at 0.04% CPU (2_000 usec over 5s), battery at 9W
        let mut init = HashMap::new();
        init.insert("TinyDaemon".to_string(), fake_counter("TinyDaemon", 100_000, false));
        let mut tracker = AppEnergyTracker {
            prev_counters: init,
            prev_monotonic_s: 10.0,
            prev_suspended_s: 0.0,
            prev_rapl_uj: None,
            max_dt_secs: 45.0,
        };
        let mut next = HashMap::new();
        next.insert("TinyDaemon".to_string(), fake_counter("TinyDaemon", 102_000, false));
        let (deltas, _) = tracker.compute_delta_from_snapshots(
            next,
            SampleInput {
                raw_dt_secs: 5.0,
                slept_delta_secs: 0.0,
                power_w: 9.0,
                energy_full_wh: 60.0,
                is_discharging: true,
                raw_display_w: 2.0,
                rapl_power_w: None,
                cpu_w_per_core: 12.0,
                baseline_max_w: 1.0,
            },
        );
        let daemon = deltas.iter().find(|d| d.app_name == "TinyDaemon").unwrap();
        let unattr = deltas
            .iter()
            .find(|d| d.app_name == "Unattributed / Hardware & Idle")
            .unwrap();
        assert!(daemon.live_power_w < 0.05);
        assert!(unattr.live_power_w > 6.5);
    }

    #[test]
    fn classification_strips_dbus_launcher_and_matches_tokens() {
        // Probe P1-6: D-Bus activated GNOME apps and Flatpak apps containing "apt" substring
        let (name1, cat1) =
            normalize_cgroup_unit("dbus-:1.2-org.gnome.Nautilus@0.service", "app").unwrap();
        assert_eq!(name1, "Nautilus");
        assert_eq!(cat1, "App");

        let (name2, cat2) =
            normalize_cgroup_unit("app-flatpak-net.sapples.LiveCaptions-1234.scope", "app")
                .unwrap();
        assert_eq!(name2, "LiveCaptions");
        assert_eq!(cat2, "App");

        let (name3, _) = normalize_cgroup_unit("dbus-broker.service", "system").unwrap();
        assert_eq!(name3, "D-Bus Message Broker");

        let (crd_name, crd_cat) =
            normalize_cgroup_unit("chrome-remote-desktop.service", "system").unwrap();
        assert_ne!(crd_name, "Google Chrome");
        assert_ne!(crd_cat, "Browser");

        let (mkc_name, mkc_cat) =
            normalize_cgroup_unit("mkchromecast.service", "app").unwrap();
        assert_ne!(mkc_name, "Google Chrome");
        assert_ne!(mkc_cat, "Browser");
    }

    #[test]
    fn attributed_energy_never_exceeds_total_battery_energy() {
        for power_w in [2.5, 8.0, 15.0, 35.0] {
            for cpu_ms in [0u64, 50, 500, 5_000, 40_000] {
                let mut init = HashMap::new();
                init.insert("AppA".to_string(), fake_counter("AppA", 1_000_000, true));
                init.insert("AppB".to_string(), fake_counter("AppB", 1_000_000, false));
                let mut tracker = AppEnergyTracker {
                    prev_counters: init,
                    prev_monotonic_s: 0.0,
                    prev_suspended_s: 0.0,
                    prev_rapl_uj: None,
                    max_dt_secs: 45.0,
                };
                let mut next = HashMap::new();
                next.insert(
                    "AppA".to_string(),
                    fake_counter("AppA", 1_000_000 + cpu_ms * 1_000, true),
                );
                next.insert(
                    "AppB".to_string(),
                    fake_counter("AppB", 1_000_000 + (cpu_ms / 2) * 1_000, false),
                );
                let (deltas, dt) = tracker.compute_delta_from_snapshots(
                    next,
                    SampleInput {
                        raw_dt_secs: 5.0,
                        slept_delta_secs: 0.0,
                        power_w,
                        energy_full_wh: 60.0,
                        is_discharging: true,
                        raw_display_w: 2.5,
                        rapl_power_w: None,
                        cpu_w_per_core: 12.0,
                        baseline_max_w: 1.0,
                    },
                );
                let sum_wh: f64 = deltas.iter().map(|d| d.energy_wh).sum();
                let budget_wh = power_w * (dt / 3600.0);
                assert!(
                    sum_wh <= budget_wh + 1e-6,
                    "attributed {sum_wh} Wh exceeded budget {budget_wh} Wh at power={power_w}W"
                );
            }
        }
    }
}
