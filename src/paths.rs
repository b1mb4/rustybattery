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
