use std::fs;
use std::path::{Path, PathBuf};

/// System root paths used for hardware and cgroup telemetry (`E-4`).
/// Defaults to real `/sys`, `/proc`, `/sys/fs/cgroup`, or a fake root in integration tests.
#[derive(Debug, Clone)]
pub struct Paths {
    pub sysfs: PathBuf,
    pub procfs: PathBuf,
    pub cgroup: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self::real()
    }
}

impl Paths {
    pub fn real() -> Self {
        Self {
            sysfs: PathBuf::from("/sys"),
            procfs: PathBuf::from("/proc"),
            cgroup: PathBuf::from("/sys/fs/cgroup"),
        }
    }

    pub fn from_root(root: &Path) -> Self {
        Self {
            sysfs: root.join("sys"),
            procfs: root.join("proc"),
            cgroup: root.join("sys/fs/cgroup"),
        }
    }

    pub fn power_supply(&self) -> PathBuf {
        self.sysfs.join("class/power_supply")
    }

    pub fn powercap(&self) -> PathBuf {
        self.sysfs.join("class/powercap")
    }

    pub fn backlight(&self) -> PathBuf {
        self.sysfs.join("class/backlight")
    }

    pub fn wakeup(&self) -> PathBuf {
        self.sysfs.join("class/wakeup")
    }
}

/// Restricts a file containing per-app telemetry or config to owner-only read/write (`0600`).
pub fn set_private_file_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Restricts a RustyBattery state/config directory to owner-only access (`0700`).
pub fn set_private_dir_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Advisory exclusive file lock (`flock`) used to serialize cross-process read-modify-write
/// updates on RustyBattery state files (`rb_frozen_units.json`, `config.toml`).
pub struct AdvisoryLock {
    #[cfg(unix)]
    _file: fs::File,
}

impl AdvisoryLock {
    pub fn acquire(lock_path: &Path) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            if let Some(parent) = lock_path.parent() {
                let _ = fs::create_dir_all(parent);
                set_private_dir_mode(parent);
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(lock_path)
                .ok()?;
            set_private_file_mode(lock_path);
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return None;
            }
            Some(Self { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = lock_path;
            Some(Self {})
        }
    }
}

/// Generates a process- and timestamp-scoped temporary sibling path to avoid concurrent `.tmp` collisions.
pub fn unique_temp_sibling(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state");
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let tmp_name = format!(".{file_name}.tmp.{}.{nanos}", std::process::id());
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    }
}
