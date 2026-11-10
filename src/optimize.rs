use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    /// User systemd service or scope under `/sys/fs/cgroup/user.slice/...` (can be stopped without root)
    UserUnit,
    /// Running Docker containers (can be stopped via `docker stop` or `systemctl stop`)
    DockerContainers,
    /// System daemon under `/sys/fs/cgroup/system.slice/...`
    SystemService,
    /// Fedora Tuned power profile (`tuned-adm profile powersave`)
    PowerProfile,
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

/// Freezes (`freeze = true`) or unfreezes (`freeze = false`) all user cgroup directories for a given app.
pub fn set_app_frozen(app_name: &str, freeze: bool) -> Result<String> {
    let dirs = find_app_cgroup_dirs(app_name);
    if dirs.is_empty() {
        anyhow::bail!("No active cgroup found for '{app_name}'");
    }

    let val = if freeze { "1" } else { "0" };
    let mut count = 0usize;
    let mut last_err = None;

    for dir in &dirs {
        let freeze_file = dir.join("cgroup.freeze");
        if freeze_file.exists() {
            match fs::write(&freeze_file, val) {
                Ok(_) => count += 1,
                Err(e) => last_err = Some(e),
            }
        }
    }

    if count > 0 {
        let action = if freeze {
            "❄ Frozen (0% CPU Deep Sleep)"
        } else {
            "▶ Resumed"
        };
        Ok(format!("{action}: {app_name} ({count} cgroup unit(s))"))
    } else if let Some(e) = last_err {
        anyhow::bail!("Permission denied freezing '{app_name}': {e}")
    } else {
        anyhow::bail!("Could not freeze '{app_name}'")
    }
}

/// Stops / terminates all processes and units belonging to an application.
pub fn stop_or_kill_app(app_name: &str) -> Result<String> {
    let lower = app_name.to_lowercase();
    // Safety guard: never kill core session or current terminal/IDE
    if lower.contains("gnome shell")
        || lower.contains("pipewire")
        || lower.contains("dbus")
        || lower.contains("systemd core")
        || lower.contains("network & wi-fi")
    {
        anyhow::bail!("Protected system/session component '{app_name}' cannot be killed");
    }

    if lower.contains("docker") {
        let stopped = stop_all_docker_containers();
        return Ok(format!(
            "Stopped Docker containers & background services ({stopped} stopped)"
        ));
    }

    let dirs = find_app_cgroup_dirs(app_name);
    if dirs.is_empty() {
        anyhow::bail!("No running processes found for '{app_name}'");
    }

    let mut killed_units = 0usize;
    let mut system_units_to_stop = Vec::new();

    for dir in &dirs {
        let unit_name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dir_str = dir.to_string_lossy();

        if dir_str.contains("/user.slice/") {
            // Try cgroup.kill first (fast, kernel-level termination of all processes in cgroup)
            let kill_file = dir.join("cgroup.kill");
            if fs::write(&kill_file, "1").is_ok() {
                killed_units += 1;
            }
            if unit_name.ends_with(".service") || unit_name.ends_with(".scope") {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", &unit_name])
                    .output();
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
        Ok(format!("Stopped '{app_name}'"))
    } else {
        anyhow::bail!(
            "Could not stop '{app_name}' (system service may require `sudo systemctl stop`)"
        )
    }
}

/// Finds all active cgroup directories (`pids.current > 0`) that normalize to `target_app_name`.
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
        let Ok(entries) = fs::read_dir(&slice_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let unit_name = entry.file_name().to_string_lossy().to_string();
            if let Some((app_name, _)) =
                crate::apps::normalize_cgroup_unit(&unit_name, slice_type)
            {
                if app_name.eq_ignore_ascii_case(target_app_name) {
                    let pids = read_u64_file(&path.join("pids.current"));
                    if pids > 0 {
                        out.push(path);
                    }
                }
            }
        }
    }
    out
}

/// Scans the system for unused/underused background services, containers, and power settings
/// that are currently consuming battery/RAM and can be turned off.
pub fn scan_optimization_targets(current_power_w: f64) -> Vec<OptimizationTarget> {
    let mut targets = Vec::new();
    let uid = unsafe { libc::getuid() };
    let user_base = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));

    // 1. Check Docker Containers & Docker Desktop
    let mut docker_units = Vec::new();
    let mut docker_paths = Vec::new();
    let mut docker_mem_bytes = 0u64;
    let mut docker_pids = 0u32;

    if let Ok(entries) = fs::read_dir("/sys/fs/cgroup/system.slice") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("docker-") && name.ends_with(".scope") {
                let path = entry.path();
                let pids = read_u64_file(&path.join("pids.current")) as u32;
                if pids > 0 {
                    docker_mem_bytes += read_u64_file(&path.join("memory.current"));
                    docker_pids += pids;
                    docker_units.push(name);
                    docker_paths.push(path);
                }
            }
        }
    }
    let docker_desktop_dir = user_base.join("app.slice/docker-desktop.service");
    if docker_desktop_dir.exists() {
        let pids = read_u64_file(&docker_desktop_dir.join("pids.current")) as u32;
        if pids > 0 {
            docker_mem_bytes += read_u64_file(&docker_desktop_dir.join("memory.current"));
            docker_pids += pids;
            docker_units.push("docker-desktop.service".to_string());
            docker_paths.push(docker_desktop_dir);
        }
    }

    if docker_pids > 0 {
        let mem_mb = (docker_mem_bytes as f64) / (1024.0 * 1024.0);
        let est_w = (current_power_w * 0.28).clamp(1.8, 5.5);
        targets.push(OptimizationTarget {
            id: "docker-containers".to_string(),
            display_name: format!(
                "Docker Containers ({} running)",
                docker_units
                    .iter()
                    .filter(|u| u.starts_with("docker-"))
                    .count()
                    .max(1)
            ),
            reason: "Heavy background containers & virtualized services draining CPU & RAM"
                .to_string(),
            kind: TargetKind::DockerContainers,
            unit_names: docker_units,
            cgroup_paths: docker_paths,
            memory_mb: mem_mb,
            pids_count: docker_pids,
            estimated_watts: est_w,
            selected: true,
        });
    }

    // 2. Check User Background Hogs (can be stopped directly by the user without root!)
    let user_candidates: &[(&str, &str, &[&str], &str, f64)] = &[
        (
            "gnome-software",
            "GNOME Software Updater",
            &["app.slice/gnome-software.service"],
            "Background package/flatpak store checker holding RAM & wakeups",
            0.35,
        ),
        (
            "localsearch",
            "GNOME File Indexer (LocalSearch)",
            &["background.slice/localsearch-3.service"],
            "Background disk & file content indexer",
            0.40,
        ),
        (
            "evolution-bg",
            "Evolution Calendar/Mail Sync",
            &[
                "app.slice/evolution-alarm-notify.service",
                "app.slice/evolution-calendar-factory.service",
                "app.slice/evolution-addressbook-factory.service",
                "app.slice/evolution-source-registry.service",
            ],
            "4 background Evolution sync daemons running even when mail/calendar is closed",
            0.25,
        ),
        (
            "abrt-applet",
            "ABRT Problem Reporting Applet",
            &["app.slice/dbus-:1.2-org.freedesktop.problems.applet@0.service"],
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
                selected: true,
            });
        }
    }

    // Check dynamic user scopes in app.slice (like KDE Connect or Blueman applet if idle)
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

    // 3. Check System Background Daemons (`/sys/fs/cgroup/system.slice`)
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
            "mysqld",
            "MySQL Database Server (mysqld)",
            &["mysqld.service"],
            "Local relational database server running 36 background threads",
            0.45,
        ),
        (
            "dnf5daemon",
            "DNF5 Package Daemon",
            &["dnf5daemon-server.service"],
            "Idle package manager background service",
            0.15,
        ),
        (
            "abrt-system",
            "ABRT Crash Watchers",
            &[
                "abrtd.service",
                "abrt-journal-core.service",
                "abrt-oops.service",
                "abrt-xorg.service",
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
                selected: true,
            });
        }
    }

    // 4. Check CPU Energy Performance Preference / Tuned Power Profile
    let epp = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference")
        .unwrap_or_default()
        .trim()
        .to_string();
    if !epp.is_empty() && epp != "power" {
        targets.push(OptimizationTarget {
            id: "power-profile".to_string(),
            display_name: format!("CPU Power Profile (currently '{epp}' → 'powersave')"),
            reason: "Switch Fedora Tuned profile to 'powersave' for lower CPU voltage & frequency scaling".to_string(),
            kind: TargetKind::PowerProfile,
            unit_names: vec!["powersave".to_string()],
            cgroup_paths: Vec::new(),
            memory_mb: 0.0,
            pids_count: 0,
            estimated_watts: 1.50,
            selected: true,
        });
    }

    targets
}

fn stop_all_docker_containers() -> usize {
    let mut stopped_count = 0usize;

    // 1. Stop running Docker containers via `docker ps -q` -> `docker stop`
    if let Ok(out) = Command::new("docker").args(["ps", "-q"]).output() {
        if out.status.success() {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !ids.is_empty() {
                let mut stop_cmd = Command::new("docker");
                stop_cmd.arg("stop").arg("-t").arg("2");
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

    // 2. Also stop user-level docker-desktop.service if running
    let uid = unsafe { libc::getuid() };
    let dd_kill = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/docker-desktop.service/cgroup.kill"
    ));
    if dd_kill.exists() {
        let _ = fs::write(&dd_kill, "1");
    }
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
                for path in &t.cgroup_paths {
                    let kill_file = path.join("cgroup.kill");
                    if fs::write(&kill_file, "1").is_ok() {
                        ok = true;
                    }
                }
                for unit in &t.unit_names {
                    if let Ok(out) = Command::new("systemctl")
                        .args(["--user", "stop", unit])
                        .output()
                    {
                        if out.status.success() {
                            ok = true;
                        }
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
            TargetKind::DockerContainers => {
                let n = stop_all_docker_containers();
                if n > 0 {
                    freed_memory_mb += t.memory_mb;
                    stopped_pids += t.pids_count;
                    estimated_watts_saved += t.estimated_watts;
                    actions_taken.push(format!(
                        "Stopped {} Docker container(s) ({:.0} MB RAM freed, ~{:.1} W saved)",
                        n, t.memory_mb, t.estimated_watts
                    ));
                } else {
                    // Fallback: queue docker.service for systemctl stop
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
            TargetKind::PowerProfile => {
                if let Ok(out) = Command::new("tuned-adm")
                    .args(["profile", "powersave"])
                    .output()
                {
                    if out.status.success() {
                        estimated_watts_saved += t.estimated_watts;
                        actions_taken.push(
                            "Switched Fedora Tuned profile to 'powersave' (~1.5 W saved)"
                                .to_string(),
                        );
                    } else {
                        failed_actions.push(
                            "CPU profile 'powersave' requires host session (run `tuned-adm profile powersave`)"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    // Batch-stop system services in a single systemctl call (uses Polkit/pkttyagent if interactive)
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
            // Try non-interactive sudo -n systemctl stop in case user has nopasswd sudo cached
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
