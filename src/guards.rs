use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Walks `PPid` from the current process up to PID 1 and returns `true` if any ancestor
/// process normalizes to `target_app_name`. Prevents freezing the terminal hosting `battery`.
pub fn is_terminal_ancestor_app(target_app_name: &str) -> bool {
    let mut pid = unsafe { libc::getpid() } as u32;
    for _ in 0..16 {
        if pid <= 1 {
            break;
        }
        let pid_dir = PathBuf::from(format!("/proc/{pid}"));
        if let Ok(comm) = fs::read_to_string(pid_dir.join("comm")) {
            let comm = comm.trim();
            if let Some((app_name, _)) = crate::apps::normalize_cgroup_unit(comm, "app") {
                if app_name.eq_ignore_ascii_case(target_app_name) {
                    return true;
                }
            }
        }
        let Ok(status) = fs::read_to_string(pid_dir.join("status")) else {
            break;
        };
        let mut ppid = 0u32;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                ppid = rest.trim().parse::<u32>().unwrap_or(0);
                break;
            }
        }
        if ppid == 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    false
}

fn collect_pids(cgroup_dirs: &[PathBuf]) -> HashSet<u32> {
    let mut pids = HashSet::new();
    for dir in cgroup_dirs {
        if let Ok(s) = fs::read_to_string(dir.join("cgroup.procs")) {
            for line in s.lines() {
                if let Ok(p) = line.trim().parse::<u32>() {
                    pids.insert(p);
                }
            }
        }
    }
    pids
}

fn has_open_webcam(pids: &HashSet<u32>) -> bool {
    for &pid in pids {
        let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
        let Ok(entries) = fs::read_dir(fd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(target) = fs::read_link(entry.path()) {
                let s = target.to_string_lossy();
                if s.starts_with("/dev/video") {
                    return true;
                }
            }
        }
    }
    false
}

fn has_active_alsa_or_pulse_audio(app_name: &str, pids: &HashSet<u32>) -> bool {
    // 1. Fast sysfs/procfs check: `/proc/asound/card*/pcm*/sub*/status` lists `state: RUNNING` and `owner_pid`
    if let Ok(cards) = fs::read_dir("/proc/asound") {
        for card in cards.flatten() {
            if !card.file_name().to_string_lossy().starts_with("card") {
                continue;
            }
            let Ok(pcms) = fs::read_dir(card.path()) else {
                continue;
            };
            for pcm in pcms.flatten() {
                if !pcm.file_name().to_string_lossy().starts_with("pcm") {
                    continue;
                }
                let Ok(subs) = fs::read_dir(pcm.path()) else {
                    continue;
                };
                for sub in subs.flatten() {
                    if let Ok(st) = fs::read_to_string(sub.path().join("status")) {
                        if st.contains("state: RUNNING") {
                            for line in st.lines() {
                                if let Some(rest) = line.trim().strip_prefix("owner_pid") {
                                    let pid_str = rest.trim_start_matches([':', ' ']).trim();
                                    if let Ok(p) = pid_str.parse::<u32>() {
                                        if pids.contains(&p) {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Check PipeWire / PulseAudio sink-inputs & source-outputs via `pactl`
    let lower = app_name.to_lowercase();
    for kind in ["sink-inputs", "source-outputs"] {
        if let Ok(out) = Command::new("pactl").args(["list", kind]).output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
                if text.contains(&lower) && !text.contains("corked: yes") {
                    return true;
                }
            }
        }
    }

    false
}

fn has_systemd_inhibitor(app_name: &str) -> bool {
    let lower = app_name.to_lowercase();
    if let Ok(out) = Command::new("systemd-inhibit").arg("--list").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if text.contains(&lower) {
                return true;
            }
        }
    }
    false
}

/// Checks if `app_name` has any active safety guard (hosts current terminal, active audio/mic stream,
/// open webcam `/dev/video*`, or active systemd sleep/idle inhibitor).
/// Returns `Some(reason)` if the app must NOT be frozen.
pub fn check_freeze_guards(app_name: &str, cgroup_dirs: &[PathBuf]) -> Option<&'static str> {
    if is_terminal_ancestor_app(app_name) {
        return Some("hosts current terminal session");
    }
    let pids = collect_pids(cgroup_dirs);
    if has_open_webcam(&pids) {
        return Some("active camera (/dev/video*) in use");
    }
    if has_active_alsa_or_pulse_audio(app_name, &pids) {
        return Some("active audio playback/recording stream");
    }
    if has_systemd_inhibitor(app_name) {
        return Some("holds active systemd inhibitor lock");
    }
    None
}
