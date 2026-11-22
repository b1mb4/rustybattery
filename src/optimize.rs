use anyhow::Result;
use serde::{Deserialize, Serialize};
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
    .filter(|d| devices.contains(d))
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrozenPidEntry {
    pub pid: i32,
    pub starttime: u64,
    pub app_name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnedFrozenState {
    pub cgroups: Vec<PathBuf>,
    pub pids: Vec<FrozenPidEntry>,
}

fn owned_frozen_units_path() -> Option<PathBuf> {
    let base = dirs::data_local_dir()?.join("rustybattery");
    let _ = fs::create_dir_all(&base);
    Some(base.join("rb_frozen_units.json"))
}

pub fn load_owned_frozen_state_from(path: &Path) -> OwnedFrozenState {
    let Ok(s) = fs::read_to_string(path) else {
        return OwnedFrozenState::default();
    };
    if let Ok(state) = serde_json::from_str::<OwnedFrozenState>(&s) {
        return state;
    }
    if let Ok(cgroups) = serde_json::from_str::<Vec<PathBuf>>(&s) {
        return OwnedFrozenState {
            cgroups,
            pids: Vec::new(),
        };
    }
    OwnedFrozenState::default()
}

fn save_owned_frozen_state_to(path: &Path, state: &OwnedFrozenState) {
    if state.cgroups.is_empty() && state.pids.is_empty() {
        let _ = fs::remove_file(path);
    } else if let Ok(json) = serde_json::to_string(state) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
            crate::paths::set_private_dir_mode(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            crate::paths::set_private_file_mode(&tmp);
            let _ = fs::rename(tmp, path);
        }
    }
}

fn read_proc_starttime(pid: i32) -> Option<u64> {
    if pid <= 1 {
        return None;
    }
    let stat_str = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close_paren = stat_str.rfind(')')?;
    let rest = stat_str.get(close_paren + 2..)?;
    rest.split_whitespace().nth(19)?.parse::<u64>().ok()
}

fn is_pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let Ok(stat_str) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(close_paren) = stat_str.rfind(')') else {
        return false;
    };
    let Some(rest) = stat_str.get(close_paren + 2..) else {
        return false;
    };
    let Some(state) = rest.split_whitespace().next() else {
        return false;
    };
    state != "Z" && state != "X"
}

/// Thaws only the cgroup directories and SIGSTOP fallback PIDs recorded in `state_path`,
/// keeping any unit whose `cgroup.freeze` write failed so ownership is never lost (`N-6`).
pub fn thaw_owned_state_at(state_path: &Path) -> Vec<String> {
    let state = load_owned_frozen_state_from(state_path);
    if state.cgroups.is_empty() && state.pids.is_empty() {
        return Vec::new();
    }

    let mut thawed = Vec::new();
    let mut remaining_cgroups = Vec::new();
    for dir in state.cgroups {
        let freeze_file = dir.join("cgroup.freeze");
        if !dir.exists() || !freeze_file.exists() {
            continue;
        }
        if read_u64_file(&freeze_file) == 0 {
            continue;
        }
        match fs::write(freeze_file, "0") {
            Ok(_) => {
                let name = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| dir.display().to_string());
                thawed.push(name);
            }
            Err(_) => remaining_cgroups.push(dir),
        }
    }

    let mut remaining_pids = Vec::new();
    for entry in state.pids {
        if read_proc_starttime(entry.pid) == Some(entry.starttime) {
            if unsafe { libc::kill(entry.pid, libc::SIGCONT) } == 0 {
                thawed.push(format!("{} (pid {})", entry.app_name, entry.pid));
            } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                remaining_pids.push(entry);
            }
        }
    }

    save_owned_frozen_state_to(
        state_path,
        &OwnedFrozenState {
            cgroups: remaining_cgroups,
            pids: remaining_pids,
        },
    );
    thawed
}

/// Thaws only the cgroup directories and PIDs that RustyBattery itself froze (`rb_frozen_units.json`),
/// leaving any cgroups frozen by the user or container tools untouched (`F-3`, `N-6`).
pub fn thaw_owned_user_cgroups() -> Vec<String> {
    owned_frozen_units_path()
        .map(|p| thaw_owned_state_at(&p))
        .unwrap_or_default()
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
        if read_u64_file(&freeze_file) == 1 && fs::write(freeze_file, "0").is_ok() {
            count += 1;
        }
    }
    count
}

/// Force-thaws every frozen cgroup (`cgroup.freeze == 1`) under the current user's systemd slice tree
/// (`battery resume --force-all`).
pub fn thaw_all_user_cgroups() -> usize {
    let owned_count = thaw_owned_user_cgroups().len();
    let uid = unsafe { libc::getuid() };
    let user_base = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    let mut total = owned_count;
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
    let state_path = owned_frozen_units_path();
    let mut state = state_path
        .as_deref()
        .map(load_owned_frozen_state_from)
        .unwrap_or_default();

    for dir in &dirs {
        let freeze_file = dir.join("cgroup.freeze");
        if freeze_file.exists() {
            match fs::write(freeze_file, val) {
                Ok(_) => {
                    count += 1;
                    if freeze {
                        if !state.cgroups.iter().any(|p| p == dir) {
                            state.cgroups.push(dir.clone());
                        }
                    } else {
                        state.cgroups.retain(|p| p != dir);
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
    }

    if count > 0 {
        if let Some(ref p) = state_path {
            save_owned_frozen_state_to(p, &state);
        }
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
                if freeze {
                    if let Some(starttime) = read_proc_starttime(pid) {
                        if !state
                            .pids
                            .iter()
                            .any(|e| e.pid == pid && e.starttime == starttime)
                        {
                            state.pids.push(FrozenPidEntry {
                                pid,
                                starttime,
                                app_name: app_name.to_string(),
                            });
                        }
                    }
                } else {
                    state.pids.retain(|e| e.pid != pid);
                }
            }
        }
        if sig_count > 0 {
            if let Some(ref p) = state_path {
                save_owned_frozen_state_to(p, &state);
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    NotRunning,
    StoppedCleanly,
    ForcedKill { waited_secs: u64 },
    Failed,
}

/// Parses systemd timespan strings such as `"1min 30s"`, `"15s"`, `"500ms"`, `"250us"`, `"infinity"`.
pub fn parse_systemd_duration_ms(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("infinity") {
        return None;
    }
    let mut total_ms = 0.0_f64;
    let mut parsed_any = false;
    for tok in trimmed.split_whitespace() {
        let split_at = tok
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(tok.len());
        let (num_str, unit) = tok.split_at(split_at);
        let Ok(val) = num_str.parse::<f64>() else {
            return None;
        };
        let mult = match unit {
            "us" => 0.001,
            "ms" => 1.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "min" | "m" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            "" => return None,
            _ => return None,
        };
        total_ms += val * mult;
        parsed_any = true;
    }
    if !parsed_any || total_ms <= 0.0 {
        None
    } else {
        Some(total_ms.round() as u64)
    }
}

fn read_unit_timeout_stop_ms(unit_name: &str) -> Option<u64> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "-p", "TimeoutStopUSec", "--value", unit_name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_systemd_duration_ms(&String::from_utf8_lossy(&out.stdout))
}

/// Gracefully stops a user cgroup unit without blocking PID 1 / daemon shutdown (`N-1`):
/// sends `systemctl --user stop --no-block` (or `SIGTERM` to PIDs), polls `pids.current`
/// up to `TimeoutStopUSec` or `default_grace_ms`, and forces `cgroup.kill`/`SIGKILL` only after timeout.
fn graceful_stop_user_cgroup(
    dir: &Path,
    unit_name: &str,
    default_grace_ms: u64,
    use_unit_timeout: bool,
) -> StopOutcome {
    if read_u64_file(&dir.join("pids.current")) == 0 {
        return StopOutcome::NotRunning;
    }
    let effective_grace_ms = if use_unit_timeout
        && (unit_name.ends_with(".service") || unit_name.ends_with(".scope"))
    {
        read_unit_timeout_stop_ms(unit_name)
            .unwrap_or(default_grace_ms)
            .clamp(2_000, 90_000)
    } else {
        default_grace_ms.max(2_000)
    };

    let pids_file = dir.join("cgroup.procs");
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
        if let Ok(procs) = fs::read_to_string(&pids_file) {
            for line in procs.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    if pid > 1 && unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                        initiated = true;
                    }
                }
            }
        }
    }
    if !initiated {
        return StopOutcome::Failed;
    }

    let step_ms = 100u64;
    let steps = (effective_grace_ms / step_ms).max(1);
    for _ in 0..steps {
        if read_u64_file(&dir.join("pids.current")) == 0 {
            return StopOutcome::StoppedCleanly;
        }
        if crate::service::is_stop_requested() {
            return StopOutcome::StoppedCleanly;
        }
        std::thread::sleep(std::time::Duration::from_millis(step_ms));
    }

    if read_u64_file(&dir.join("pids.current")) > 0 {
        let kill_file = dir.join("cgroup.kill");
        if fs::write(kill_file, "1").is_err() {
            if let Ok(procs) = fs::read_to_string(pids_file) {
                for line in procs.lines() {
                    if let Ok(pid) = line.trim().parse::<i32>() {
                        if pid > 1 {
                            unsafe { libc::kill(pid, libc::SIGKILL) };
                        }
                    }
                }
            }
        }
        return StopOutcome::ForcedKill {
            waited_secs: (effective_grace_ms + 500) / 1000,
        };
    }
    StopOutcome::StoppedCleanly
}

/// Gracefully stops standalone PIDs with `SIGTERM`, waits up to `grace_ms`, and falls back to `SIGKILL`.
pub fn graceful_stop_pids(pids: &[i32], grace_ms: u64) -> StopOutcome {
    let mut live = Vec::new();
    for &pid in pids {
        if pid > 1 && unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            live.push(pid);
        }
    }
    if live.is_empty() {
        return StopOutcome::Failed;
    }

    let step_ms = 100u64;
    let steps = (grace_ms / step_ms).max(1);
    for _ in 0..steps {
        if !live.iter().any(|&p| is_pid_alive(p)) {
            return StopOutcome::StoppedCleanly;
        }
        if crate::service::is_stop_requested() {
            return StopOutcome::StoppedCleanly;
        }
        std::thread::sleep(std::time::Duration::from_millis(step_ms));
    }

    if live.iter().any(|&p| is_pid_alive(p)) {
        for &pid in &live {
            if is_pid_alive(pid) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        return StopOutcome::ForcedKill {
            waited_secs: (grace_ms + 500) / 1000,
        };
    }
    StopOutcome::StoppedCleanly
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

    let grace_ms = crate::autopilot::load_autopilot_config().stop_grace_secs * 1000;
    let dirs = find_app_cgroup_dirs(app_name);
    let mut killed_units = 0usize;
    let mut max_forced_secs: Option<u64> = None;
    let mut system_units_to_stop = Vec::new();

    for dir in &dirs {
        let unit_name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dir_str = dir.to_string_lossy();

        if dir_str.contains("/user.slice/") {
            match graceful_stop_user_cgroup(dir, &unit_name, grace_ms, true) {
                StopOutcome::StoppedCleanly => killed_units += 1,
                StopOutcome::ForcedKill { waited_secs } => {
                    killed_units += 1;
                    max_forced_secs = Some(max_forced_secs.unwrap_or(0).max(waited_secs));
                }
                StopOutcome::NotRunning | StopOutcome::Failed => {}
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
        if let Some(secs) = max_forced_secs {
            return Ok(format!("Stopped '{app_name}' (forced after {secs}s)"));
        }
        return Ok(format!("Stopped '{app_name}'"));
    }

    // Fallback: POSIX SIGTERM -> grace -> SIGKILL via `/proc` for non-systemd distros or standalone WMs
    let pids = find_matching_proc_pids(app_name);
    if !pids.is_empty() {
        match graceful_stop_pids(&pids, grace_ms) {
            StopOutcome::StoppedCleanly => {
                return Ok(format!(
                    "Stopped '{app_name}' ({} process(es))",
                    pids.len()
                ));
            }
            StopOutcome::ForcedKill { waited_secs } => {
                return Ok(format!(
                    "Stopped '{app_name}' ({} process(es), forced after {waited_secs}s)",
                    pids.len()
                ));
            }
            StopOutcome::NotRunning | StopOutcome::Failed => {}
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
    let grace_ms = crate::autopilot::load_autopilot_config().stop_grace_secs * 1000;

    let mut system_units_to_stop: Vec<String> = Vec::new();
    let mut system_targets_meta: Vec<&OptimizationTarget> = Vec::new();

    for t in targets.iter().filter(|t| t.selected) {
        match t.kind {
            TargetKind::UserUnit => {
                let mut ok = false;
                let mut forced_secs: Option<u64> = None;
                if t.cgroup_paths.is_empty() {
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
                } else {
                    for path in &t.cgroup_paths {
                        let uname = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        match graceful_stop_user_cgroup(path, &uname, grace_ms, false) {
                            StopOutcome::StoppedCleanly => ok = true,
                            StopOutcome::ForcedKill { waited_secs } => {
                                ok = true;
                                forced_secs = Some(forced_secs.unwrap_or(0).max(waited_secs));
                            }
                            StopOutcome::NotRunning | StopOutcome::Failed => {}
                        }
                    }
                }
                if ok {
                    freed_memory_mb += t.memory_mb;
                    stopped_pids += t.pids_count;
                    estimated_watts_saved += t.estimated_watts;
                    let forced_note = forced_secs
                        .map(|s| format!(" (forced after {s}s)"))
                        .unwrap_or_default();
                    actions_taken.push(format!(
                        "Stopped {} ({:.0} MB RAM, {} threads){forced_note}",
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

    #[test]
    fn parses_systemd_duration_strings() {
        assert_eq!(parse_systemd_duration_ms("1min 30s"), Some(90_000));
        assert_eq!(parse_systemd_duration_ms("15s"), Some(15_000));
        assert_eq!(parse_systemd_duration_ms("500ms"), Some(500));
        assert_eq!(parse_systemd_duration_ms("1h 2min"), Some(3_720_000));
        assert_eq!(parse_systemd_duration_ms("infinity"), None);
        assert_eq!(parse_systemd_duration_ms(""), None);
    }

    #[test]
    fn graceful_stop_allows_3s_sigterm_handler_to_exit_cleanly() {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "trap 'sleep 3; exit 0' TERM; while true; do sleep 0.1; done",
            ])
            .spawn()
            .expect("spawn sh");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(150));
        let outcome = graceful_stop_pids(&[pid], 6_000);
        let _ = child.wait();
        assert_eq!(outcome, StopOutcome::StoppedCleanly);
    }

    #[test]
    fn graceful_stop_forces_sigkill_when_sigterm_ignored() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; while true; do sleep 0.2; done"])
            .spawn()
            .expect("spawn sh");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(150));
        let outcome = graceful_stop_pids(&[pid], 500);
        let _ = child.wait();
        assert!(matches!(outcome, StopOutcome::ForcedKill { .. }));
    }

    #[test]
    fn failed_cgroup_thaw_keeps_unit_in_ownership_file() {
        use std::os::unix::fs::PermissionsExt;

        let uniq = format!(
            "rb_thaw_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let root = std::env::temp_dir().join(uniq);
        let writable = root.join("writable.scope");
        let readonly = root.join("readonly.scope");
        let vanished = root.join("vanished.scope");
        fs::create_dir_all(&writable).unwrap();
        fs::create_dir_all(&readonly).unwrap();
        fs::write(writable.join("cgroup.freeze"), "1").unwrap();
        let ro_file = readonly.join("cgroup.freeze");
        fs::write(&ro_file, "1").unwrap();
        fs::set_permissions(&ro_file, fs::Permissions::from_mode(0o444)).unwrap();

        let json_path = root.join("rb_frozen_units.json");
        save_owned_frozen_state_to(
            &json_path,
            &OwnedFrozenState {
                cgroups: vec![writable.clone(), readonly.clone(), vanished.clone()],
                pids: Vec::new(),
            },
        );

        let thawed = thaw_owned_state_at(&json_path);
        assert!(thawed.contains(&"writable.scope".to_string()));
        assert_eq!(fs::read_to_string(writable.join("cgroup.freeze")).unwrap(), "0");

        let remaining = load_owned_frozen_state_from(&json_path);
        assert!(!remaining.cgroups.contains(&writable));
        assert!(!remaining.cgroups.contains(&vanished));
        if unsafe { libc::geteuid() } != 0 {
            assert!(remaining.cgroups.contains(&readonly));
        }

        let _ = fs::set_permissions(&ro_file, fs::Permissions::from_mode(0o644));
        let _ = fs::remove_dir_all(root);
    }
}
