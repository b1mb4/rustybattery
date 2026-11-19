use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    /// User systemd service or scope under `/sys/fs/cgroup/user.slice/...` (can be stopped without root)
    UserUnit,
    /// Running Docker or Podman containers (can be stopped via `docker stop` / `podman stop`)
    Containers,
    /// System daemon under `/sys/fs/cgroup/system.slice/...`
    SystemService,
    /// Linux CPU Power Profile (`powerprofilesctl`, `tuned-adm`, `tlp`, or cpufreq sysfs)
    PowerProfile,
    /// Soft-block Bluetooth via `rfkill` so Bluetooth controller enters deep sleep during s2idle
    BluetoothRadio,
    /// Disable noisy USB/PCIe wakeup triggers in `/proc/acpi/wakeup` for deeper s2idle residency
    AcpiWakeups,
}

#[derive(Debug, Clone)]
pub struct S2IdleStatus {
    pub mem_sleep: String,
    pub lid_closed: bool,
    pub suspend_success: u64,
    pub suspend_fail: u64,
    pub bluetooth_unblocked: bool,
    pub noisy_wakeups: Vec<(String, String)>,
    pub hibernate_delay_configured: bool,
}

pub fn parse_noisy_acpi_wakeups(content: &str, want_enabled: bool) -> Vec<(String, String)> {
    let marker = if want_enabled { "*enabled" } else { "*disabled" };
    let mut out = Vec::new();
    for line in content.lines().skip(1) {
        if !line.contains(marker) {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(&dev) = parts.first() else {
            continue;
        };
        let is_noisy = dev.starts_with("XHC")
            || dev.starts_with("EHC")
            || dev.starts_with("NHI")
            || dev.starts_with("GPP")
            || dev.starts_with("GP1")
            || dev.starts_with("PTXH");
        if is_noisy {
            let sysfs_node = parts.get(3).copied().unwrap_or("").to_string();
            out.push((dev.to_string(), sysfs_node));
        }
    }
    out
}

pub fn scan_noisy_acpi_wakeups() -> Vec<(String, String)> {
    let Ok(content) = fs::read_to_string("/proc/acpi/wakeup") else {
        return Vec::new();
    };
    parse_noisy_acpi_wakeups(&content, true)
}

pub fn scan_disabled_noisy_acpi_wakeups() -> Vec<(String, String)> {
    let Ok(content) = fs::read_to_string("/proc/acpi/wakeup") else {
        return Vec::new();
    };
    parse_noisy_acpi_wakeups(&content, false)
}

/// Idempotently sets noisy ACPI wakeup devices to `target_enabled` (`false` = disable, `true` = enable),
/// checking `/proc/acpi/wakeup` before writing so calling it twice never flips devices the wrong way.
pub fn set_acpi_wakeups_state(devices: &[String], target_enabled: bool) -> Result<usize> {
    let current_matching: Vec<String> = if target_enabled {
        scan_disabled_noisy_acpi_wakeups()
    } else {
        scan_noisy_acpi_wakeups()
    }
    .into_iter()
    .map(|(d, _)| d)
    .filter(|d| devices.iter().any(|req| req == d))
    .collect();

    if current_matching.is_empty() {
        return Ok(0);
    }

    let mut toggled = 0usize;
    for dev in &current_matching {
        if fs::write("/proc/acpi/wakeup", format!("{dev}\n")).is_ok() {
            toggled += 1;
        }
    }
    if toggled == current_matching.len() {
        return Ok(toggled);
    }

    // Use `sudo -n tee /proc/acpi/wakeup` passing device names on stdin (no shell command string)
    let mut sudo_ok = true;
    for dev in &current_matching {
        use std::io::Write;
        use std::process::Stdio;
        let Ok(mut child) = Command::new("sudo")
            .args(["-n", "tee", "/proc/acpi/wakeup"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            sudo_ok = false;
            break;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{dev}\n").as_bytes());
        }
        if !child.wait().map(|s| s.success()).unwrap_or(false) {
            sudo_ok = false;
            break;
        }
    }
    if sudo_ok {
        return Ok(current_matching.len());
    }

    let verb = if target_enabled {
        "Re-enabling"
    } else {
        "Disabling"
    };
    let devs_str = current_matching.join(" ");
    anyhow::bail!(
        "{verb} ACPI wakeups requires root (note: resets on reboot). Run:\n  for d in {devs_str}; do echo $d | sudo tee /proc/acpi/wakeup; done"
    )
}

pub fn disable_noisy_acpi_wakeups(devices: &[String]) -> Result<usize> {
    set_acpi_wakeups_state(devices, false)
}

pub fn inspect_s2idle_status() -> S2IdleStatus {
    let mem_sleep = fs::read_to_string("/sys/power/mem_sleep")
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let lid_closed = crate::autopilot::is_lid_closed();
    let suspend_success = read_u64_file(Path::new("/sys/power/suspend_stats/success"));
    let suspend_fail = read_u64_file(Path::new("/sys/power/suspend_stats/fail"));
    let bluetooth_unblocked = crate::autopilot::is_bluetooth_unblocked();
    let noisy_wakeups = scan_noisy_acpi_wakeups();
    let sleep_conf = fs::read_to_string("/etc/systemd/sleep.conf").unwrap_or_default();
    let hibernate_delay_configured = sleep_conf
        .lines()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("HibernateDelaySec"));
    S2IdleStatus {
        mem_sleep,
        lid_closed,
        suspend_success,
        suspend_fail,
        bluetooth_unblocked,
        noisy_wakeups,
        hibernate_delay_configured,
    }
}

#[derive(Debug, Clone)]
pub struct OptimizationTarget {
    pub id: String,
    pub display_name: String,
    pub reason: String,
    pub kind: TargetKind,
    pub unit_names: Vec<String>,
    pub cgroup_paths: Vec<PathBuf>,
    pub memory_mb: f64,
    pub pids_count: u32,
    pub estimated_watts: f64,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct OptimizationReport {
    pub actions_taken: Vec<String>,
    pub failed_actions: Vec<String>,
    pub freed_memory_mb: f64,
    pub stopped_pids: u32,
    pub estimated_watts_saved: f64,
}

fn read_u64_file(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Checks if a cgroup directory currently has `cgroup.freeze == 1`
pub fn is_cgroup_frozen(dir: &Path) -> bool {
    read_u64_file(&dir.join("cgroup.freeze")) == 1
}

/// Finds all user PIDs in `/proc` matching `target_app_name` (used on non-systemd distros or standalone WMs).
fn find_matching_proc_pids(target_app_name: &str) -> Vec<i32> {
    let mut pids = Vec::new();
    let my_uid = unsafe { libc::getuid() };
    let my_pid = unsafe { libc::getpid() };

    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid <= 1 || pid == my_pid {
            continue;
        }
        let pid_dir = entry.path();
        // Verify process belongs to current user via /proc/[pid]/status Uid
        let Ok(status_str) = fs::read_to_string(pid_dir.join("status")) else {
            continue;
        };
        let mut proc_uid = u32::MAX;
        for line in status_str.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    proc_uid = first.parse::<u32>().unwrap_or(u32::MAX);
                }
                break;
            }
        }
        if proc_uid != my_uid {
            continue;
        }

        let Ok(comm) = fs::read_to_string(pid_dir.join("comm")) else {
            continue;
        };
        let comm = comm.trim();
        let (app_name, _) = crate::apps::resolve_proc_app_name(&pid_dir, comm);
        if app_name.eq_ignore_ascii_case(target_app_name) {
            pids.push(pid);
        }
    }
    pids
}

fn owned_frozen_units_path() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rustybattery");
    let _ = fs::create_dir_all(&base);
    base.join("rb_frozen_units.json")
}

fn load_owned_frozen_units() -> std::collections::HashSet<PathBuf> {
    let path = owned_frozen_units_path();
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn save_owned_frozen_units(set: &std::collections::HashSet<PathBuf>) {
    let path = owned_frozen_units_path();
    if set.is_empty() {
        let _ = fs::remove_file(&path);
    } else {
        let list: Vec<&PathBuf> = set.iter().collect();
        if let Ok(json) = serde_json::to_string(&list) {
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }
}

/// Thaws only the cgroup directories that RustyBattery itself froze (`rb_frozen_units.json`),
/// leaving any cgroups frozen by the user or container tools untouched (`F-3`).
pub fn thaw_owned_user_cgroups() -> Vec<String> {
    let owned = load_owned_frozen_units();
    let mut thawed = Vec::new();
    for dir in owned {
        let freeze_file = dir.join("cgroup.freeze");
        if read_u64_file(&freeze_file) == 1 && fs::write(&freeze_file, "0").is_ok() {
            let name = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| dir.display().to_string());
            thawed.push(name);
        }
    }
    save_owned_frozen_units(&std::collections::HashSet::new());
    thawed
}

fn thaw_slice_recursive(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".slice") {
            count += thaw_slice_recursive(&path);
            continue;
        }
        let freeze_file = path.join("cgroup.freeze");
        if read_u64_file(&freeze_file) == 1 && fs::write(&freeze_file, "0").is_ok() {
            count += 1;
        }
    }
    count
}

/// Force-thaws every frozen cgroup (`cgroup.freeze == 1`) under the current user's systemd slice tree
/// (`battery resume --force-all`).
pub fn thaw_all_user_cgroups() -> usize {
    let _ = thaw_owned_user_cgroups();
    let uid = unsafe { libc::getuid() };
    let user_base = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    let mut total = 0usize;
    for sub in ["app.slice", "session.slice", "background.slice"] {
        total += thaw_slice_recursive(&user_base.join(sub));
    }
    total
}

/// Freezes (`freeze = true`) or unfreezes (`freeze = false`) an application across all Linux platforms:
/// 1. Uses Linux cgroup v2 `cgroup.freeze` when the app runs in a cgroup scope/service.
/// 2. Falls back to POSIX `SIGSTOP` / `SIGCONT` for standalone processes on non-systemd distros or window managers.
pub fn set_app_frozen(app_name: &str, freeze: bool) -> Result<String> {
    if freeze && crate::guards::is_terminal_ancestor_app(app_name) {
        anyhow::bail!(
            "Refusing to freeze '{app_name}' because it hosts the current terminal session"
        );
    }
    let dirs = find_app_cgroup_dirs(app_name);
    let val = if freeze { "1" } else { "0" };
    let mut count = 0usize;
    let mut last_err = None;
    let mut owned = load_owned_frozen_units();

    for dir in &dirs {
        let freeze_file = dir.join("cgroup.freeze");
        if freeze_file.exists() {
            match fs::write(&freeze_file, val) {
                Ok(_) => {
                    count += 1;
                    if freeze {
                        owned.insert(dir.clone());
                    } else {
                        owned.remove(dir);
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
    }
    save_owned_frozen_units(&owned);

    if count > 0 {
        let action = if freeze {
            "❄ Frozen (0% CPU Deep Sleep)"
        } else {
            "▶ Resumed"
        };
        return Ok(format!("{action}: {app_name} ({count} cgroup unit(s))"));
    }

    // Fallback: POSIX SIGSTOP / SIGCONT for non-systemd or session-scoped processes
    let pids = find_matching_proc_pids(app_name);
    if !pids.is_empty() {
        let sig = if freeze { libc::SIGSTOP } else { libc::SIGCONT };
        let mut sig_count = 0usize;
        for pid in pids {
            if unsafe { libc::kill(pid, sig) } == 0 {
                sig_count += 1;
            }
        }
        if sig_count > 0 {
            let action = if freeze {
                "❄ Frozen (SIGSTOP 0% CPU)"
            } else {
                "▶ Resumed (SIGCONT)"
            };
            return Ok(format!("{action}: {app_name} ({sig_count} process(es))"));
        }
    }

    if let Some(e) = last_err {
        anyhow::bail!("Permission denied freezing '{app_name}': {e}")
    } else {
        anyhow::bail!("No active user cgroup or process found for '{app_name}'")
    }
}

/// Gracefully stops a user cgroup unit without blocking the daemon thread for 90s (`F-6`):
/// sends `systemctl --user stop --no-block` (or `SIGTERM` to PIDs), waits up to `grace_ms`
/// for processes to exit, and falls back to `cgroup.kill` only if still alive.
fn graceful_stop_user_cgroup(dir: &Path, unit_name: &str, grace_ms: u64) -> bool {
    let mut initiated = false;
    if unit_name.ends_with(".service") || unit_name.ends_with(".scope") {
        if let Ok(out) = Command::new("systemctl")
            .args(["--user", "stop", "--no-block", unit_name])
            .output()
        {
            if out.status.success() {
                initiated = true;
            }
        }
    }
    if !initiated {
        if let Ok(procs) = fs::read_to_string(dir.join("cgroup.procs")) {
            for line in procs.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    if pid > 1 && unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                        initiated = true;
                    }
                }
            }
        }
    }

    let step_ms = 100u64;
    let steps = (grace_ms / step_ms).max(1);
    for _ in 0..steps {
        if read_u64_file(&dir.join("pids.current")) == 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(step_ms));
    }

    if read_u64_file(&dir.join("pids.current")) > 0 {
        let kill_file = dir.join("cgroup.kill");
        if fs::write(&kill_file, "1").is_ok() {
            initiated = true;
        }
    }
    initiated
}

/// Stops / terminates all processes and units belonging to an application across any Linux distro.
pub fn stop_or_kill_app(app_name: &str) -> Result<String> {
    let lower = app_name.to_lowercase();
    // Safety guard: never kill core display servers, audio servers, or network
    if lower.contains("gnome shell")
        || lower.contains("kde plasma")
        || lower.contains("hyprland")
        || lower.contains("sway")
        || lower.contains("cosmic")
        || lower.contains("window manager")
        || lower.contains("pipewire")
        || lower.contains("dbus")
        || lower.contains("system core")
        || lower.contains("network & wi-fi")
    {
        anyhow::bail!("Protected system/desktop component '{app_name}' cannot be killed");
    }

    if lower.contains("docker") || lower.contains("container") {
        let stopped = stop_all_containers();
        return Ok(format!(
            "Stopped running containers & background services ({stopped} stopped)"
        ));
    }

    let dirs = find_app_cgroup_dirs(app_name);
    let mut killed_units = 0usize;
    let mut system_units_to_stop = Vec::new();

    for dir in &dirs {
        let unit_name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dir_str = dir.to_string_lossy();

        if dir_str.contains("/user.slice/") {
            if graceful_stop_user_cgroup(dir, &unit_name, 1500) {
                killed_units += 1;
            }
        } else if dir_str.contains("/system.slice/") {
            system_units_to_stop.push(unit_name);
        }
    }

    if !system_units_to_stop.is_empty() {
        let mut cmd = Command::new("systemctl");
        cmd.arg("stop");
        for u in &system_units_to_stop {
            cmd.arg(u);
        }
        if let Ok(out) = cmd.output() {
            if out.status.success() {
                killed_units += system_units_to_stop.len();
            }
        }
    }

    if killed_units > 0 {
        return Ok(format!("Stopped '{app_name}'"));
    }

    // Fallback: POSIX SIGTERM via `/proc` for non-systemd distros or standalone window manager processes
    let pids = find_matching_proc_pids(app_name);
    if !pids.is_empty() {
        let mut term_count = 0usize;
        for pid in pids {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                term_count += 1;
            }
        }
        if term_count > 0 {
            return Ok(format!("Stopped '{app_name}' ({term_count} process(es))"));
        }
    }

    anyhow::bail!(
        "Could not stop '{app_name}' (system service may require `sudo systemctl stop`)"
    )
}

fn collect_matching_cgroup_dirs(
    slice_dir: &Path,
    slice_type: &str,
    target_app_name: &str,
    out: &mut Vec<PathBuf>,
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
            collect_matching_cgroup_dirs(&path, slice_type, target_app_name, out);
            continue;
        }
        if let Some((app_name, _)) = crate::apps::normalize_cgroup_unit(&unit_name, slice_type) {
            if app_name.eq_ignore_ascii_case(target_app_name) {
                let pids = read_u64_file(&path.join("pids.current"));
                if pids > 0 {
                    out.push(path);
                }
            }
        }
    }
}

/// Finds all active cgroup directories (`pids.current > 0`) that normalize to `target_app_name`,
/// recursing into nested `.slice` subdirectories.
pub fn find_app_cgroup_dirs(target_app_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let uid = unsafe { libc::getuid() };
    let user_base = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));

    for (slice_dir, slice_type) in [
        (user_base.join("app.slice"), "app"),
        (user_base.join("session.slice"), "session"),
        (user_base.join("background.slice"), "background"),
        (PathBuf::from("/sys/fs/cgroup/system.slice"), "system"),
    ] {
        collect_matching_cgroup_dirs(&slice_dir, slice_type, target_app_name, &mut out);
    }
    out
}

/// Scans the system for unused/underused background services, containers, and power settings
/// across all Linux distributions (Ubuntu, Fedora, Arch, Debian, openSUSE, NixOS, etc.).
pub fn scan_optimization_targets(current_power_w: f64) -> Vec<OptimizationTarget> {
    let mut targets = Vec::new();
    let uid = unsafe { libc::getuid() };
    let user_base = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));

    // 1. Check Docker & Podman Containers
    let mut container_units = Vec::new();
    let mut container_paths = Vec::new();
    let mut container_mem_bytes = 0u64;
    let mut container_pids = 0u32;

    if let Ok(entries) = fs::read_dir("/sys/fs/cgroup/system.slice") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.starts_with("docker-") || name.starts_with("libpod-"))
                && name.ends_with(".scope")
            {
                let path = entry.path();
                let pids = read_u64_file(&path.join("pids.current")) as u32;
                if pids > 0 {
                    container_mem_bytes += read_u64_file(&path.join("memory.current"));
                    container_pids += pids;
                    container_units.push(name);
                    container_paths.push(path);
                }
            }
        }
    }
    for sub in ["app.slice/docker-desktop.service", "user.slice/podman.service"] {
        let dir = user_base.join(sub);
        if dir.exists() {
            let pids = read_u64_file(&dir.join("pids.current")) as u32;
            if pids > 0 {
                container_mem_bytes += read_u64_file(&dir.join("memory.current"));
                container_pids += pids;
                if let Some(f) = dir.file_name() {
                    container_units.push(f.to_string_lossy().to_string());
                }
                container_paths.push(dir);
            }
        }
    }

    if container_pids > 0 {
        let mem_mb = (container_mem_bytes as f64) / (1024.0 * 1024.0);
        let est_w = (current_power_w * 0.28).clamp(1.8, 5.5);
        targets.push(OptimizationTarget {
            id: "containers".to_string(),
            display_name: format!(
                "Docker / Podman Containers ({} running)",
                container_units
                    .iter()
                    .filter(|u| u.starts_with("docker-") || u.starts_with("libpod-"))
                    .count()
                    .max(1)
            ),
            reason: "Heavy background containers & virtualized services draining CPU & RAM"
                .to_string(),
            kind: TargetKind::Containers,
            unit_names: container_units,
            cgroup_paths: container_paths,
            memory_mb: mem_mb,
            pids_count: container_pids,
            estimated_watts: est_w,
            selected: false,
        });
    }

    // 2. Check User Background Hogs across GNOME, KDE Plasma, Ubuntu, Arch, Fedora (No root needed!)
    let user_candidates: &[(&str, &str, &[&str], &str, f64)] = &[
        (
            "gui-store-updater",
            "Desktop Software Store Updater",
            &[
                "app.slice/gnome-software.service",
                "app.slice/plasma-discover.service",
                "app.slice/snap-store.service",
            ],
            "Background package/flatpak/snap store checker holding RAM & wakeups",
            0.35,
        ),
        (
            "file-indexer",
            "Desktop File Indexer (Tracker/LocalSearch/Baloo)",
            &[
                "background.slice/localsearch-3.service",
                "background.slice/tracker-miner-fs-3.service",
                "background.slice/tracker-extract-3.service",
                "background.slice/kde-baloo.service",
            ],
            "Background disk & file content indexing daemon",
            0.40,
        ),
        (
            "pim-sync",
            "Background Calendar & Mail Sync (Evolution/Akonadi)",
            &[
                "app.slice/evolution-alarm-notify.service",
                "app.slice/evolution-calendar-factory.service",
                "app.slice/evolution-addressbook-factory.service",
                "app.slice/evolution-source-registry.service",
            ],
            "Background calendar/contacts sync daemons running when mail app is closed",
            0.25,
        ),
        (
            "crash-applet",
            "Crash Notification Applet",
            &[
                "app.slice/dbus-:1.2-org.freedesktop.problems.applet@0.service",
                "app.slice/apport-gtk.service",
            ],
            "Background crash notification monitor",
            0.10,
        ),
    ];

    for &(id, title, rel_paths, reason, est_w) in user_candidates {
        let mut unit_names = Vec::new();
        let mut cgroup_paths = Vec::new();
        let mut mem_bytes = 0u64;
        let mut pids = 0u32;

        for rel in rel_paths {
            let full = user_base.join(rel);
            if full.exists() {
                let p = read_u64_file(&full.join("pids.current")) as u32;
                if p > 0 {
                    pids += p;
                    mem_bytes += read_u64_file(&full.join("memory.current"));
                    if let Some(fname) = full.file_name() {
                        unit_names.push(fname.to_string_lossy().to_string());
                    }
                    cgroup_paths.push(full);
                }
            }
        }

        if pids > 0 {
            targets.push(OptimizationTarget {
                id: id.to_string(),
                display_name: title.to_string(),
                reason: reason.to_string(),
                kind: TargetKind::UserUnit,
                unit_names,
                cgroup_paths,
                memory_mb: (mem_bytes as f64) / (1024.0 * 1024.0),
                pids_count: pids,
                estimated_watts: est_w,
                selected: false,
            });
        }
    }

    // Check dynamic user scopes in app.slice (like KDE Connect)
    if let Ok(entries) = fs::read_dir(user_base.join("app.slice")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let pids = read_u64_file(&path.join("pids.current")) as u32;
            if pids == 0 {
                continue;
            }
            let mem_mb = (read_u64_file(&path.join("memory.current")) as f64) / (1024.0 * 1024.0);
            if name.contains("kdeconnect") {
                targets.push(OptimizationTarget {
                    id: "kdeconnect".to_string(),
                    display_name: "KDE Connect Daemon".to_string(),
                    reason: "Background network discovery & phone sync daemon".to_string(),
                    kind: TargetKind::UserUnit,
                    unit_names: vec![name],
                    cgroup_paths: vec![path],
                    memory_mb: mem_mb,
                    pids_count: pids,
                    estimated_watts: 0.25,
                    selected: true,
                });
            }
        }
    }

    // 3. Check System Background Daemons across all Linux distros (`/sys/fs/cgroup/system.slice`)
    let sys_candidates: &[(&str, &str, &[&str], &str, f64)] = &[
        (
            "prometheus",
            "Prometheus & Alertmanager",
            &["prometheus.service", "alertmanager.service"],
            "Time-series metrics server continuously scraping and writing to disk",
            0.85,
        ),
        (
            "ollama",
            "Ollama Local LLM Server",
            &["ollama.service"],
            "Background local AI inference server",
            0.55,
        ),
        (
            "sql-db",
            "Local Database Server (MySQL/PostgreSQL/Redis/Mongo)",
            &[
                "mysqld.service",
                "mariadb.service",
                "postgresql.service",
                "redis.service",
                "redis-server.service",
                "mongod.service",
            ],
            "Background database daemon running continuous worker threads",
            0.45,
        ),
        (
            "pkg-daemon",
            "Background Package Daemon (PackageKit/DNF/Unattended-Upgrades)",
            &[
                "packagekit.service",
                "dnf5daemon-server.service",
                "unattended-upgrades.service",
                "fwupd.service",
            ],
            "Idle package manager background service",
            0.15,
        ),
        (
            "crash-daemon",
            "System Crash Watchers (ABRT / Apport / Whoopsie)",
            &[
                "abrtd.service",
                "abrt-journal-core.service",
                "abrt-oops.service",
                "abrt-xorg.service",
                "apport.service",
                "whoopsie.service",
            ],
            "System crash dump monitoring daemons",
            0.15,
        ),
        (
            "pcscd",
            "Smartcard Reader Daemon (pcscd)",
            &["pcscd.service"],
            "Smartcard hardware polling service",
            0.15,
        ),
        (
            "modemmanager",
            "Cellular ModemManager",
            &["ModemManager.service"],
            "WWAN/Cellular modem polling service (unused on Wi-Fi laptops)",
            0.15,
        ),
    ];

    let sys_base = Path::new("/sys/fs/cgroup/system.slice");
    for &(id, title, units, reason, est_w) in sys_candidates {
        let mut active_units = Vec::new();
        let mut cgroup_paths = Vec::new();
        let mut mem_bytes = 0u64;
        let mut pids = 0u32;

        for &u in units {
            let full = sys_base.join(u);
            if full.exists() {
                let p = read_u64_file(&full.join("pids.current")) as u32;
                if p > 0 {
                    pids += p;
                    mem_bytes += read_u64_file(&full.join("memory.current"));
                    active_units.push(u.to_string());
                    cgroup_paths.push(full);
                }
            }
        }

        if pids > 0 {
            targets.push(OptimizationTarget {
                id: id.to_string(),
                display_name: title.to_string(),
                reason: reason.to_string(),
                kind: TargetKind::SystemService,
                unit_names: active_units,
                cgroup_paths,
                memory_mb: (mem_bytes as f64) / (1024.0 * 1024.0),
                pids_count: pids,
                estimated_watts: est_w,
                selected: false,
            });
        }
    }

    // 4. Check CPU Energy Performance Preference / Power Profile across Linux platforms
    let epp = fs::read_to_string(
        "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference",
    )
    .or_else(|_| fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"))
    .unwrap_or_default()
    .trim()
    .to_string();

    if !epp.is_empty() && epp != "power" && epp != "powersave" {
        targets.push(OptimizationTarget {
            id: "power-profile".to_string(),
            display_name: format!("CPU Power Profile (currently '{epp}' → 'powersave')"),
            reason: "Switch Linux power profile (powerprofilesctl / tuned / TLP) to 'powersave' for lower CPU voltage".to_string(),
            kind: TargetKind::PowerProfile,
            unit_names: vec!["powersave".to_string()],
            cgroup_paths: Vec::new(),
            memory_mb: 0.0,
            pids_count: 0,
            estimated_watts: 1.50,
            selected: true,
        });
    }

    // 5. Check Bluetooth radio for s2idle sleep drain reduction
    if crate::autopilot::is_bluetooth_unblocked() {
        targets.push(OptimizationTarget {
            id: "bluetooth-rfkill".to_string(),
            display_name: "Bluetooth Radio (soft-block via rfkill for s2idle)".to_string(),
            reason: "Turn off Bluetooth controller so it cannot prevent deep S0ix sleep residency".to_string(),
            kind: TargetKind::BluetoothRadio,
            unit_names: vec!["bluetooth".to_string()],
            cgroup_paths: Vec::new(),
            memory_mb: 0.0,
            pids_count: 0,
            estimated_watts: 0.45,
            selected: true,
        });
    }

    // 6. Check noisy ACPI USB/PCIe wakeup triggers that disturb s2idle
    let noisy_wakeups = scan_noisy_acpi_wakeups();
    if !noisy_wakeups.is_empty() {
        let dev_names: Vec<String> = noisy_wakeups.into_iter().map(|(d, _)| d).collect();
        targets.push(OptimizationTarget {
            id: "acpi-wakeups".to_string(),
            display_name: format!(
                "s2idle USB/PCI Wakeup Hygiene ({} noisy triggers: {})",
                dev_names.len(),
                dev_names.join(", ")
            ),
            reason: "Opt-in: disable XHC/NHI/GPP ACPI wakeups (note: may disable external USB keyboard wake)".to_string(),
            kind: TargetKind::AcpiWakeups,
            unit_names: dev_names,
            cgroup_paths: Vec::new(),
            memory_mb: 0.0,
            pids_count: 0,
            estimated_watts: 0.85,
            selected: false,
        });
    }

    targets
}

fn stop_all_containers() -> usize {
    let mut stopped_count = 0usize;

    // 1. Try Docker (`docker ps -q` -> `docker stop`) and Podman (`podman ps -q` -> `podman stop`)
    for runtime in ["docker", "podman"] {
        if let Ok(out) = Command::new(runtime).args(["ps", "-q"]).output() {
            if out.status.success() {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !ids.is_empty() {
                    let mut stop_cmd = Command::new(runtime);
                    stop_cmd.arg("stop").arg("-t").arg("10");
                    for id in &ids {
                        stop_cmd.arg(id);
                    }
                    if let Ok(res) = stop_cmd.output() {
                        if res.status.success() {
                            stopped_count += ids.len();
                        }
                    }
                }
            }
        }
    }

    // 2. Also stop user-level docker-desktop.service gracefully if running
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "docker-desktop.service"])
        .output();

    stopped_count
}

/// Executes the selected optimization targets and returns a summary report.
pub fn execute_optimizations(targets: &[OptimizationTarget]) -> OptimizationReport {
    let mut actions_taken = Vec::new();
    let mut failed_actions = Vec::new();
    let mut freed_memory_mb = 0.0;
    let mut stopped_pids = 0u32;
    let mut estimated_watts_saved = 0.0;

    let mut system_units_to_stop: Vec<String> = Vec::new();
    let mut system_targets_meta: Vec<&OptimizationTarget> = Vec::new();

    for t in targets.iter().filter(|t| t.selected) {
        match t.kind {
            TargetKind::UserUnit => {
                let mut ok = false;
                for unit in &t.unit_names {
                    if let Ok(out) = Command::new("systemctl")
                        .args(["--user", "stop", "--no-block", unit])
                        .output()
                    {
                        if out.status.success() {
                            ok = true;
                        }
                    }
                }
                for path in &t.cgroup_paths {
                    let uname = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if graceful_stop_user_cgroup(path, &uname, 1000) {
                        ok = true;
                    }
                }
                if ok {
                    freed_memory_mb += t.memory_mb;
                    stopped_pids += t.pids_count;
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Stopped {} ({:.0} MB RAM, {} threads)",
                        t.display_name, t.memory_mb, t.pids_count
                    ));
                } else {
                    failed_actions.push(format!("Could not stop {}", t.display_name));
                }
            }
            TargetKind::Containers => {
                let n = stop_all_containers();
                if n > 0 {
                    freed_memory_mb += t.memory_mb;
                    stopped_pids += t.pids_count;
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Stopped {} container(s) ({:.0} MB RAM freed, ~{:.1} W saved estimate)",
                        n, t.memory_mb, t.estimated_watts
                    ));
                } else {
                    system_units_to_stop.push("docker.service".to_string());
                    system_units_to_stop.push("docker.socket".to_string());
                    system_targets_meta.push(t);
                }
            }
            TargetKind::SystemService => {
                for u in &t.unit_names {
                    system_units_to_stop.push(u.clone());
                }
                system_targets_meta.push(t);
            }
            TargetKind::PowerProfile => match crate::power::set_power_profile(true) {
                Ok(note) => {
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Switched Linux CPU power profile ({note}, ~1.5 W saved estimate)"
                    ));
                }
                Err(e) => {
                    failed_actions.push(e.to_string());
                }
            },
            TargetKind::BluetoothRadio => {
                if crate::autopilot::set_bluetooth_blocked(true) {
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(
                        "Soft-blocked Bluetooth radio via rfkill (~0.45 W saved & deeper s2idle)"
                            .to_string(),
                    );
                } else {
                    failed_actions.push("Failed to run `rfkill block bluetooth`".to_string());
                }
            }
            TargetKind::AcpiWakeups => match disable_noisy_acpi_wakeups(&t.unit_names) {
                Ok(n) => {
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Disabled {n} noisy ACPI wakeup trigger(s) ({}) for deeper s2idle",
                        t.unit_names.join(", ")
                    ));
                }
                Err(e) => {
                    failed_actions.push(e.to_string());
                }
            },
        }
    }

    if !system_units_to_stop.is_empty() {
        let mut cmd = Command::new("systemctl");
        cmd.arg("stop");
        for u in &system_units_to_stop {
            cmd.arg(u);
        }
        let stopped_ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);

        if stopped_ok {
            for t in system_targets_meta {
                freed_memory_mb += t.memory_mb;
                stopped_pids += t.pids_count;
                estimated_watts_saved += t.estimated_watts;
                actions_taken.push(format!(
                    "Stopped {} ({:.0} MB RAM, ~{:.1} W saved)",
                    t.display_name, t.memory_mb, t.estimated_watts
                ));
            }
        } else {
            let mut sudo_cmd = Command::new("sudo");
            sudo_cmd.args(["-n", "systemctl", "stop"]);
            for u in &system_units_to_stop {
                sudo_cmd.arg(u);
            }
            if sudo_cmd.output().map(|o| o.status.success()).unwrap_or(false) {
                for t in system_targets_meta {
                    freed_memory_mb += t.memory_mb;
                    stopped_pids += t.pids_count;
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Stopped {} ({:.0} MB RAM, ~{:.1} W saved)",
                        t.display_name, t.memory_mb, t.estimated_watts
                    ));
                }
            } else {
                let units_joined = system_units_to_stop.join(" ");
                failed_actions.push(format!(
                    "System services need sudo: run `sudo systemctl stop {units_joined}`"
                ));
            }
        }
    }

    OptimizationReport {
        actions_taken,
        failed_actions,
        freed_memory_mb,
        stopped_pids,
        estimated_watts_saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_noisy_acpi_wakeups_while_ignoring_lid_and_power_button() {
        let sample = "\
Device\tS-state\t  Status   Sysfs node
LID0\t  S4\t*enabled   platform:PNP0C0D:00
PWRB\t  S4\t*enabled   platform:PNP0C0C:00
XHC0\t  S4\t*enabled   pci:0000:73:00.3
XHC1\t  S4\t*disabled  pci:0000:73:00.4
NHI1\t  S0\t*enabled   pci:0000:74:00.6
";
        let enabled = parse_noisy_acpi_wakeups(sample, true);
        assert_eq!(
            enabled,
            vec![
                ("XHC0".to_string(), "pci:0000:73:00.3".to_string()),
                ("NHI1".to_string(), "pci:0000:74:00.6".to_string()),
            ]
        );
        let disabled = parse_noisy_acpi_wakeups(sample, false);
        assert_eq!(
            disabled,
            vec![("XHC1".to_string(), "pci:0000:73:00.4".to_string())]
        );
    }
}
