use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static GUARD_CACHE: Mutex<Option<HashMap<String, (Instant, Option<&'static str>)>>> =
    Mutex::new(None);
const GUARD_CACHE_TTL: Duration = Duration::from_secs(30);

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
            let (app_name, _) = crate::apps::resolve_proc_app_name(&pid_dir, comm);
            if app_name.eq_ignore_ascii_case(target_app_name) {
                return true;
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

/// Parses `pactl list sink-inputs` or `source-outputs` block-by-block and returns `true`
/// if any uncorked stream block belongs to a PID in `pids` (or matches `app_name`).
pub fn pactl_output_has_active_stream_for_pids(
    text: &str,
    pids: &HashSet<u32>,
    app_name: &str,
) -> bool {
    let lower_app = app_name.trim().to_lowercase();
    for block in text.split("\n\n") {
        let lower_block = block.to_lowercase();
        if lower_block.contains("corked: yes") {
            continue;
        }
        let mut pid_matched = false;
        for line in block.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("application.process.id =") {
                let p_str = rest.trim().trim_matches('"');
                if let Ok(p) = p_str.parse::<u32>() {
                    if pids.contains(&p) {
                        pid_matched = true;
                        break;
                    }
                }
            }
        }
        if pid_matched
            || (!lower_app.is_empty()
                && lower_block.contains(&format!("application.name = \"{lower_app}")))
        {
            return true;
        }
    }
    false
}

fn has_active_alsa_or_pulse_audio(app_name: &str, pids: &HashSet<u32>) -> bool {
    // 1. Fast procfs check: `/proc/asound/card*/pcm*/sub*/status` lists `state: RUNNING` and `owner_pid`
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

    // 2. Check PipeWire / PulseAudio sink-inputs & source-outputs per stream block by PID
    for kind in ["sink-inputs", "source-outputs"] {
        if let Ok(out) = Command::new("pactl").args(["list", kind]).output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if pactl_output_has_active_stream_for_pids(&text, pids, app_name) {
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
/// open webcam `/dev/video*`, or active systemd sleep/idle inhibitor), caching results for 30s.
pub fn check_freeze_guards(app_name: &str, cgroup_dirs: &[PathBuf]) -> Option<&'static str> {
    if let Ok(guard) = GUARD_CACHE.lock() {
        if let Some(map) = &*guard {
            if let Some((ts, res)) = map.get(app_name) {
                if ts.elapsed() < GUARD_CACHE_TTL {
                    return *res;
                }
            }
        }
    }

    let result = if is_terminal_ancestor_app(app_name) {
        Some("hosts current terminal session")
    } else {
        let pids = collect_pids(cgroup_dirs);
        if has_open_webcam(&pids) {
            Some("active camera (/dev/video*) in use")
        } else if has_active_alsa_or_pulse_audio(app_name, &pids) {
            Some("active audio playback/recording stream")
        } else if has_systemd_inhibitor(app_name) {
            Some("holds active systemd inhibitor lock")
        } else {
            None
        }
    };

    if let Ok(mut guard) = GUARD_CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(app_name.to_string(), (Instant::now(), result));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pactl_block_parser_ignores_corked_stream_from_other_app() {
        // F-5 verification: a paused stream from App A must not hide an active stream from App B (PID 4242)
        let sample = "\
Sink Input #11
\tCorked: yes
\tProperties:
\t\tapplication.name = \"Firefox\"
\t\tapplication.process.id = \"1001\"

Sink Input #12
\tCorked: no
\tProperties:
\t\tapplication.name = \"Zen\"
\t\tapplication.process.id = \"4242\"
";
        let mut pids = HashSet::new();
        pids.insert(4242);
        assert!(pactl_output_has_active_stream_for_pids(
            sample,
            &pids,
            "Zen Browser"
        ));

        let mut other_pids = HashSet::new();
        other_pids.insert(1001);
        assert!(!pactl_output_has_active_stream_for_pids(
            sample,
            &other_pids,
            "Firefox"
        ));
    }
}
