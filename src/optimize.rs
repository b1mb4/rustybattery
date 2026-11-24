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
    pub hibernate_delay_value: Option<String>,
    pub lid_switch_hibernate: bool,
    pub disk_swap_gb: f64,
    pub ram_gb: f64,
    pub resume_param_configured: bool,
    pub persistent_port_fix_installed: bool,
    pub external_usb_devices: Vec<String>,
    pub pcie_pm_on_count: usize,
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

pub fn parse_systemd_key_value(content: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let val = v.trim();
            if k.trim() == key && !val.is_empty() {
                found = Some(val.to_string());
            }
        }
    }
    found
}

fn read_systemd_config_key(main_file: &str, dropin_dir: &str, key: &str) -> Option<String> {
    let mut val = fs::read_to_string(main_file)
        .ok()
        .and_then(|s| parse_systemd_key_value(&s, key));
    if let Ok(entries) = fs::read_dir(dropin_dir) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("conf"))
            .collect();
        paths.sort();
        for p in paths {
            if let Some(v) = fs::read_to_string(&p)
                .ok()
                .and_then(|s| parse_systemd_key_value(&s, key))
            {
                val = Some(v);
            }
        }
    }
    val
}

pub fn parse_disk_swap_kb(proc_swaps: &str) -> u64 {
    let mut total_kb = 0u64;
    for line in proc_swaps.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        if parts[0].starts_with("/dev/zram") {
            continue;
        }
        if let Ok(kb) = parts[2].parse::<u64>() {
            total_kb = total_kb.saturating_add(kb);
        }
    }
    total_kb
}

fn read_ram_total_gb() -> f64 {
    let Ok(meminfo) = fs::read_to_string("/proc/meminfo") else {
        return 0.0;
    };
    for line in meminfo.lines() {
        if let Some(kb) = line
            .strip_prefix("MemTotal:")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok())
        {
            return kb as f64 / (1024.0 * 1024.0);
        }
    }
    0.0
}

fn scan_external_usb_devices() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("usb") || name.contains(':') {
            continue;
        }
        let dev_class = fs::read_to_string(path.join("bDeviceClass"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if dev_class == "09" {
            continue;
        }
        let product = fs::read_to_string(path.join("product"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let lower = product.to_lowercase();
        if lower.contains("webcam")
            || lower.contains("camera")
            || lower.contains("wireless_device")
            || lower.contains("bluetooth")
            || lower.contains("fingerprint")
        {
            continue;
        }
        let vid = fs::read_to_string(path.join("idVendor"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let pid = fs::read_to_string(path.join("idProduct"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if vid.is_empty() && product.is_empty() {
            continue;
        }
        let wakeup = fs::read_to_string(path.join("power/wakeup"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = if product.is_empty() {
            format!("{name} ({vid}:{pid})")
        } else {
            format!("{name}: {product} ({vid}:{pid})")
        };
        if wakeup == "enabled" {
            out.push(format!("{label} [wakeup=enabled]"));
        } else {
            out.push(label);
        }
    }
    out.sort();
    out
}

fn count_pcie_endpoints_pm_on() -> usize {
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let p = entry.path();
        let class_hex = fs::read_to_string(p.join("class"))
            .unwrap_or_default()
            .trim()
            .to_string();
        // Check NVMe (0x0108), Wi-Fi/Network (0x02), Display/GPU (0x03), Audio (0x0403), USB (0x0c03/0x0c0a)
        let relevant = class_hex.starts_with("0x0108")
            || class_hex.starts_with("0x02")
            || class_hex.starts_with("0x03")
            || class_hex.starts_with("0x0403")
            || class_hex.starts_with("0x0c03")
            || class_hex.starts_with("0x0c0a");
        if !relevant {
            continue;
        }
        let ctrl = fs::read_to_string(p.join("power/control"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if ctrl == "on" {
            count += 1;
        }
    }
    count
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
    let hibernate_delay_value = read_systemd_config_key(
        "/etc/systemd/sleep.conf",
        "/etc/systemd/sleep.conf.d",
        "HibernateDelaySec",
    );
    let hibernate_delay_configured = hibernate_delay_value.is_some();
    let lid_switch_val = read_systemd_config_key(
        "/etc/systemd/logind.conf",
        "/etc/systemd/logind.conf.d",
        "HandleLidSwitch",
    );
    let lid_switch_hibernate = lid_switch_val
        .as_deref()
        .is_some_and(|v| v == "suspend-then-hibernate" || v == "hibernate");
    let disk_swap_gb = fs::read_to_string("/proc/swaps")
        .map(|s| parse_disk_swap_kb(&s) as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);
    let ram_gb = read_ram_total_gb();
    let resume_param_configured = fs::read_to_string("/proc/cmdline")
        .map(|s| s.split_whitespace().any(|tok| tok.starts_with("resume=")))
        .unwrap_or(false);
    let persistent_port_fix_installed =
        Path::new("/etc/udev/rules.d/99-rustybattery-port-power.rules").exists()
            && Path::new("/etc/systemd/system/rustybattery-acpi-wakeup.service").exists();
    let external_usb_devices = scan_external_usb_devices();
    let pcie_pm_on_count = count_pcie_endpoints_pm_on();

    S2IdleStatus {
        mem_sleep,
        lid_closed,
        suspend_success,
        suspend_fail,
        bluetooth_unblocked,
        noisy_wakeups,
        hibernate_delay_configured,
        hibernate_delay_value,
        lid_switch_hibernate,
        disk_swap_gb,
        ram_gb,
        resume_param_configured,
        persistent_port_fix_installed,
        external_usb_devices,
        pcie_pm_on_count,
    }
}

/// Validates a systemd `HibernateDelaySec=` timespan string against a strict allowlist so it can
/// never inject shell commands or extra directives into the generated root setup script.
pub fn validate_hibernate_delay(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok("2h".to_string());
    }
    if trimmed.len() > 32
        || trimmed.contains(['\n', '\r', '\0', '\'', '"', '`', '$', ';', '&', '|', '\\'])
    {
        anyhow::bail!("Invalid hibernate delay '{trimmed}': unsafe characters or too long");
    }
    let normalized = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(ms) = parse_systemd_duration_ms(&normalized) else {
        anyhow::bail!(
            "Invalid hibernate delay '{trimmed}': expected a systemd timespan like '2h', '90min', or '1h 30m'"
        );
    };
    if !(60_000..=7 * 86_400_000).contains(&ms) {
        anyhow::bail!("Hibernate delay '{normalized}' must be between 1min and 7d");
    }
    Ok(normalized)
}

/// Generates an auditable, idempotent root shell script that:
/// 1. Persistently stops USB/USB4/PCIe ports from draining or waking during lid-closed sleep
///    (udev runtime PM + wakeup disable rules, boot-time ACPI wakeup service, and pre-suspend hook).
/// 2. Configures `HandleLidSwitch=suspend-then-hibernate` + `HibernateDelaySec=<delay>` so the
///    laptop automatically transitions from s2idle into 0 W hibernation after the configured delay.
/// 3. Automatically provisions a real disk swapfile (with `resume=` + `resume_offset=` + dracut
///    `resume` module) if the system only has RAM-based `zram` swap.
pub fn generate_system_sleep_fix_script(hibernate_delay: &str) -> Result<String> {
    let delay = validate_hibernate_delay(hibernate_delay)?;
    Ok(format!(
        r##"#!/usr/bin/env bash
set -euo pipefail

# RustyBattery Persistent Lid-Closed Port-Power & Suspend-Then-Hibernate Setup
# Usage:
#   sudo bash "$0"            # Install persistent port-power fix + {delay} suspend-then-hibernate
#   sudo bash "$0" --uninstall # Revert all system changes made by this script

UDEV_RULE="/etc/udev/rules.d/99-rustybattery-port-power.rules"
WAKEUP_SVC="/etc/systemd/system/rustybattery-acpi-wakeup.service"
SLEEP_HOOK="/usr/lib/systemd/system-sleep/99-rustybattery-port-suspend"
SLEEP_DROPIN="/etc/systemd/sleep.conf.d/99-rustybattery.conf"
LOGIND_DROPIN="/etc/systemd/logind.conf.d/99-rustybattery.conf"
DRACUT_CONF="/etc/dracut.conf.d/99-rustybattery-resume.conf"
SWAP_MARKER="# rustybattery-hibernate-swap"

if [[ "${{EUID:-$(id -u)}}" -ne 0 ]]; then
  echo "Please run as root: sudo bash $0 $*" >&2
  exit 1
fi

if [[ "${{1:-}}" == "--uninstall" ]]; then
  echo "Reverting RustyBattery system sleep & port-power configuration..."
  systemctl disable --now rustybattery-acpi-wakeup.service 2>/dev/null || true
  rm -f "$UDEV_RULE" "$WAKEUP_SVC" "$SLEEP_HOOK" "$SLEEP_DROPIN" "$LOGIND_DROPIN" "$DRACUT_CONF"
  udevadm control --reload-rules 2>/dev/null || true
  systemctl daemon-reload 2>/dev/null || true
  systemctl kill -s HUP systemd-logind 2>/dev/null || true

  if grep -q '\.hibernate-swapfile' /etc/fstab 2>/dev/null; then
    SWAPFILE=$(awk '$1 ~ /\.hibernate-swapfile$/ {{print $1; exit}}' /etc/fstab)
    if [[ -n "${{SWAPFILE:-}}" ]]; then
      swapoff "$SWAPFILE" 2>/dev/null || true
      rm -f "$SWAPFILE"
    fi
    sed -i "\|$SWAP_MARKER|d; \|\.hibernate-swapfile|d" /etc/fstab
    if command -v grubby >/dev/null 2>&1; then
      grubby --update-kernel=ALL --remove-args="resume resume_offset" 2>/dev/null || true
    fi
    if command -v dracut >/dev/null 2>&1; then
      dracut --force --kver "$(uname -r)" 2>/dev/null || true
    fi
  fi
  echo "Done. Reboot to restore default kernel wakeup/resume state."
  exit 0
fi

echo "[1/5] Installing persistent USB/USB4/PCIe port runtime-PM & wakeup-disable udev rules..."
cat > "$UDEV_RULE" << 'EOF'
# Disable wakeup and enable runtime auto-suspend on USB/USB4/XHCI controllers
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0c03*", TEST=="power/control", ATTR{{power/control}}="auto"
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0c03*", TEST=="power/wakeup", ATTR{{power/wakeup}}="disabled"
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0c0a*", TEST=="power/control", ATTR{{power/control}}="auto"
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0c0a*", TEST=="power/wakeup", ATTR{{power/wakeup}}="disabled"

# Enable runtime auto-suspend on NVMe, Wi-Fi, and HDA Audio PCIe endpoints
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0108*", TEST=="power/control", ATTR{{power/control}}="auto"
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0280*", TEST=="power/control", ATTR{{power/control}}="auto"
ACTION=="add|change", SUBSYSTEM=="pci", ATTR{{class}}=="0x0403*", TEST=="power/control", ATTR{{power/control}}="auto"

# Prevent external USB-C/USB-A peripherals (audio dongles, hubs, etc.) from holding ports awake
ACTION=="add|change", SUBSYSTEM=="usb", ENV{{DEVTYPE}}=="usb_device", TEST=="power/control", ATTR{{power/control}}="auto"
ACTION=="add|change", SUBSYSTEM=="usb", ENV{{DEVTYPE}}=="usb_device", TEST=="power/wakeup", ATTR{{power/wakeup}}="disabled"
EOF
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --subsystem-match=pci || true

echo "[2/5] Installing pre-suspend & boot-time port power-gating hook..."
mkdir -p /usr/lib/systemd/system-sleep
cat > "$SLEEP_HOOK" << 'EOF'
#!/usr/bin/env bash
if [[ "${{1:-}}" == "pre" ]]; then
  for dev in $(awk 'NR>1 && /\*enabled/ && /^(XHC|EHC|NHI|GPP|GP1|PTXH)/ {{print $1}}' /proc/acpi/wakeup); do
    echo "$dev" > /proc/acpi/wakeup 2>/dev/null || true
  done
  for f in /sys/bus/usb/devices/*/power/wakeup; do
    [[ -w "$f" ]] && echo disabled > "$f" 2>/dev/null || true
  done
  for f in /sys/bus/usb/devices/*/power/control; do
    [[ -w "$f" ]] && echo auto > "$f" 2>/dev/null || true
  done
  for d in /sys/bus/pci/devices/*; do
    cls=$(cat "$d/class" 2>/dev/null || true)
    if [[ "$cls" == 0x0c03* || "$cls" == 0x0c0a* ]]; then
      [[ -w "$d/power/wakeup" ]] && echo disabled > "$d/power/wakeup" 2>/dev/null || true
      [[ -w "$d/power/control" ]] && echo auto > "$d/power/control" 2>/dev/null || true
    fi
  done
fi
EOF
chmod 0755 "$SLEEP_HOOK"

echo "[3/5] Installing boot-time ACPI noisy-wakeup hygiene service..."
cat > "$WAKEUP_SVC" << 'EOF'
[Unit]
Description=RustyBattery - Disable noisy USB/PCIe ACPI wakeup triggers for s2idle
After=local-fs.target

[Service]
Type=oneshot
ExecStart=/usr/lib/systemd/system-sleep/99-rustybattery-port-suspend pre
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now rustybattery-acpi-wakeup.service

echo "[4/5] Configuring systemd suspend-then-hibernate after {delay} of lid-closed sleep..."
mkdir -p /etc/systemd/sleep.conf.d /etc/systemd/logind.conf.d
cat > "$SLEEP_DROPIN" << 'EOF'
[Sleep]
AllowSuspend=yes
AllowHibernation=yes
AllowSuspendThenHibernate=yes
HibernateDelaySec={delay}
EOF

cat > "$LOGIND_DROPIN" << 'EOF'
[Login]
HandleLidSwitch=suspend-then-hibernate
HandleLidSwitchExternalPower=suspend
EOF
systemctl kill -s HUP systemd-logind 2>/dev/null || true

echo "[5/5] Checking real disk swap & kernel resume parameters for hibernation..."
RAM_KB=$(awk '/^MemTotal:/ {{print $2}}' /proc/meminfo)
DISK_SWAP_KB=$(awk 'NR>1 && $1 !~ /^\/dev\/zram/ {{sum += $3}} END {{print sum+0}}' /proc/swaps)
NEED_REBOOT=0

if (( DISK_SWAP_KB < RAM_KB )); then
  SWAP_GB=$(( (RAM_KB + 1048575) / 1048576 + 2 ))
  MIN_FREE_KB=$(( (SWAP_GB + 10) * 1048576 ))
  TARGET_MOUNT=""
  BEST_AVAIL=0
  # Prefer a non-removable internal ext4/xfs partition with plenty of headroom (never fill a tight root FS or USB drive)
  while IFS=$'\t' read -r fs_src fs_type fs_avail fs_mnt; do
    if [[ "$fs_src" == /dev/* && ( "$fs_type" == "ext4" || "$fs_type" == "xfs" ) ]]; then
      base_dev=$(lsblk -no PKNAME "$fs_src" 2>/dev/null | head -n1 || true)
      if [[ -n "$base_dev" && -r "/sys/block/$base_dev/removable" ]]; then
        if [[ "$(cat "/sys/block/$base_dev/removable" 2>/dev/null || echo 0)" == "1" ]]; then
          continue
        fi
      fi
      if (( fs_avail >= MIN_FREE_KB && fs_avail > BEST_AVAIL )); then
        BEST_AVAIL="$fs_avail"
        TARGET_MOUNT="$fs_mnt"
      fi
    fi
  done < <(findmnt -rn -b -o SOURCE,FSTYPE,AVAIL,TARGET -t ext4,xfs 2>/dev/null | awk 'BEGIN{{OFS="\t"}} {{avail_kb=int($3/1024); $3=avail_kb; tgt=$4; for(i=5;i<=NF;i++) tgt=tgt" "$i; print $1,$2,$3,tgt}}')

  if [[ -z "$TARGET_MOUNT" ]]; then
    echo "WARNING: No ext4/xfs partition with >= $((SWAP_GB + 10)) GiB free found for a ${{SWAP_GB}}G hibernate swapfile." >&2
    echo "         Port power-saving and sleep.conf/logind.conf ARE installed, but hibernation requires >= ${{SWAP_GB}}G disk swap." >&2
    exit 0
  fi

  SWAPFILE="${{TARGET_MOUNT%/}}/.hibernate-swapfile"
  echo "  -> Creating ${{SWAP_GB}} GiB hibernate swapfile at $SWAPFILE (zram stays primary at pri=100; disk swap at pri=10)..."
  if [[ ! -f "$SWAPFILE" ]]; then
    dd if=/dev/zero of="$SWAPFILE" bs=1M count=$((SWAP_GB * 1024)) status=progress
    chmod 0600 "$SWAPFILE"
    mkswap "$SWAPFILE"
  fi
  if command -v chcon >/dev/null 2>&1; then
    chcon -t swapfile_t "$SWAPFILE" 2>/dev/null || true
  fi
  if command -v semanage >/dev/null 2>&1; then
    semanage fcontext -a -t swapfile_t "$SWAPFILE" 2>/dev/null || true
    restorecon -v "$SWAPFILE" 2>/dev/null || true
  fi

  if ! grep -q '\.hibernate-swapfile' /etc/fstab; then
    printf '%s\n%s none swap defaults,pri=10,nofail,x-systemd.requires-mounts-for=%s 0 0\n' \
      "$SWAP_MARKER" "$SWAPFILE" "$TARGET_MOUNT" >> /etc/fstab
  fi
  swapon "$SWAPFILE" 2>/dev/null || true

  SWAP_UUID=$(findmnt -no UUID --target "$TARGET_MOUNT")
  RESUME_OFFSET=$(filefrag -v "$SWAPFILE" | awk '$1=="0:" {{gsub(/\.\./,"",$4); print $4; exit}}')
  if [[ -n "$SWAP_UUID" && "$RESUME_OFFSET" =~ ^[0-9]+$ && "$RESUME_OFFSET" -gt 0 ]]; then
    mkdir -p /etc/dracut.conf.d
    echo 'add_dracutmodules+=" resume "' > "$DRACUT_CONF"
    if command -v grubby >/dev/null 2>&1; then
      grubby --update-kernel=ALL --remove-args="resume resume_offset" 2>/dev/null || true
      grubby --update-kernel=ALL --args="resume=UUID=$SWAP_UUID resume_offset=$RESUME_OFFSET"
    fi
    if command -v dracut >/dev/null 2>&1; then
      echo "  -> Rebuilding initramfs with resume support for kernel $(uname -r)..."
      dracut --force --kver "$(uname -r)"
    fi
    NEED_REBOOT=1
    echo "  -> Configured kernel resume=UUID=$SWAP_UUID resume_offset=$RESUME_OFFSET"
  else
    echo "WARNING: Could not compute swapfile resume_offset automatically; check 'filefrag -v $SWAPFILE'." >&2
  fi
else
  echo "  -> Disk swap already >= RAM ($((DISK_SWAP_KB / 1048576)) GiB)."
fi

echo ""
echo "RustyBattery persistent port-power & {delay} suspend-then-hibernate setup complete!"
if (( NEED_REBOOT == 1 )); then
  echo "IMPORTANT: Reboot once so the new kernel resume= parameters take effect before testing hibernation."
fi
"##
    ))
}

pub fn write_system_sleep_fix_script(hibernate_delay: &str) -> Result<PathBuf> {
    let script = generate_system_sleep_fix_script(hibernate_delay)?;
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rustybattery");
    fs::create_dir_all(&base)?;
    crate::paths::set_private_dir_mode(&base);
    let path = base.join("setup-lid-sleep.sh");
    fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
    }
    Ok(path)
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
        let tmp = crate::paths::unique_temp_sibling(path);
        if fs::write(&tmp, json).is_ok() {
            crate::paths::set_private_file_mode(&tmp);
            if fs::rename(&tmp, path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
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
    let _lock = crate::paths::AdvisoryLock::acquire(&state_path.with_extension("lock"));
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
    let _lock = state_path
        .as_deref()
        .and_then(|p| crate::paths::AdvisoryLock::acquire(&p.with_extension("lock")));
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
                        if !state.cgroups.contains(dir) {
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
    Interrupted,
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
            return if read_u64_file(&dir.join("pids.current")) == 0 {
                StopOutcome::StoppedCleanly
            } else {
                StopOutcome::Interrupted
            };
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
            return if !live.iter().any(|&p| is_pid_alive(p)) {
                StopOutcome::StoppedCleanly
            } else {
                StopOutcome::Interrupted
            };
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
    if crate::guards::is_terminal_ancestor_app(app_name) {
        anyhow::bail!(
            "Refusing to stop '{app_name}' because it hosts the current terminal session"
        );
    }
    let lower = app_name.trim().to_lowercase();
    // Safety guard: never kill core display servers, audio servers, session daemons, or RustyBattery itself
    if lower.contains("gnome shell")
        || lower.contains("gnome settings")
        || lower.contains("kde plasma")
        || lower.contains("hyprland")
        || lower.contains("sway")
        || lower.contains("cosmic")
        || lower.contains("window manager")
        || lower.contains("pipewire")
        || lower.contains("pulseaudio")
        || lower.contains("dbus")
        || lower.contains("d-bus")
        || lower.contains("xdg desktop portal")
        || lower.contains("security, auth & keyring")
        || lower.contains("rustybattery")
        || lower.contains("system core")
        || lower.contains("network & wi-fi")
    {
        anyhow::bail!("Protected system/desktop component '{app_name}' cannot be killed");
    }

    if matches!(lower.as_str(), "docker / containers" | "docker" | "podman") {
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
                StopOutcome::NotRunning | StopOutcome::Interrupted | StopOutcome::Failed => {}
            }
        } else if dir_str.contains("/system.slice/") {
            system_units_to_stop.push(unit_name);
        }
    }

    if !system_units_to_stop.is_empty() {
        let mut cmd = Command::new("systemctl");
        cmd.args(["stop", "--no-block", "--no-ask-password"]);
        for u in &system_units_to_stop {
            cmd.arg(u);
        }
        if cmd.output().is_ok_and(|out| out.status.success()) {
            killed_units += system_units_to_stop.len();
        } else {
            let mut sudo_cmd = Command::new("sudo");
            sudo_cmd.args(["-n", "systemctl", "stop", "--no-block"]);
            for u in &system_units_to_stop {
                sudo_cmd.arg(u);
            }
            if sudo_cmd.output().is_ok_and(|out| out.status.success()) {
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
            StopOutcome::NotRunning | StopOutcome::Interrupted | StopOutcome::Failed => {}
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
    user_rules: &[crate::apps::UserAppRule],
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
            collect_matching_cgroup_dirs(&path, slice_type, target_app_name, user_rules, out);
            continue;
        }
        if let Some((app_name, _)) =
            crate::apps::normalize_cgroup_unit_with_rules(&unit_name, slice_type, user_rules)
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

/// Finds all active cgroup directories (`pids.current > 0`) that normalize to `target_app_name`,
/// recursing into nested `.slice` subdirectories.
pub fn find_app_cgroup_dirs(target_app_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let user_rules = crate::apps::load_user_app_rules();
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
        collect_matching_cgroup_dirs(&slice_dir, slice_type, target_app_name, &user_rules, &mut out);
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

fn stop_containers_for(runtime: &str, docker_host: Option<&str>) -> usize {
    let mut ps = Command::new(runtime);
    ps.args(["ps", "-q"]);
    if let Some(host) = docker_host {
        ps.env("DOCKER_HOST", host);
    }
    let Ok(out) = ps.output() else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        return 0;
    }
    let mut stop_cmd = Command::new(runtime);
    stop_cmd.args(["stop", "-t", "2"]);
    if let Some(host) = docker_host {
        stop_cmd.env("DOCKER_HOST", host);
    }
    for id in &ids {
        stop_cmd.arg(id);
    }
    if stop_cmd.output().is_ok_and(|res| res.status.success()) {
        ids.len()
    } else {
        0
    }
}

fn stop_all_containers() -> usize {
    let mut stopped_count = 0usize;

    // 1. Try Docker (default context first, then system unix:///var/run/docker.sock fallback
    //    in case a stale Docker Desktop context points at a missing desktop socket) and Podman.
    let docker_default = stop_containers_for("docker", None);
    stopped_count += docker_default;
    if docker_default == 0 && Path::new("/var/run/docker.sock").exists() {
        stopped_count += stop_containers_for("docker", Some("unix:///var/run/docker.sock"));
    }
    stopped_count += stop_containers_for("podman", None);

    // 2. Also stop user-level docker-desktop.service gracefully if running
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "--no-block", "docker-desktop.service"])
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
    let grace_ms = (crate::autopilot::load_autopilot_config().stop_grace_secs * 1000)
        .clamp(2_000, 30_000);

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
                            StopOutcome::NotRunning | StopOutcome::Interrupted | StopOutcome::Failed => {}
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
        cmd.args(["stop", "--no-block", "--no-ask-password"]);
        for u in &system_units_to_stop {
            cmd.arg(u);
        }
        let stopped_ok = cmd.output().is_ok_and(|o| o.status.success());

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
            sudo_cmd.args(["-n", "systemctl", "stop", "--no-block"]);
            for u in &system_units_to_stop {
                sudo_cmd.arg(u);
            }
            if sudo_cmd.output().is_ok_and(|o| o.status.success()) {
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

    #[test]
    fn parse_systemd_key_value_respects_comments_and_last_override() {
        let cfg = "\
# HibernateDelaySec=1h
[Sleep]
HibernateDelaySec=30min
; HibernateDelaySec=4h
HibernateDelaySec = 2h
";
        assert_eq!(
            parse_systemd_key_value(cfg, "HibernateDelaySec").as_deref(),
            Some("2h")
        );
        assert_eq!(parse_systemd_key_value(cfg, "HandleLidSwitch"), None);
    }

    #[test]
    fn parse_disk_swap_kb_excludes_zram() {
        let swaps = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/zram0                              partition\t8388604\t\t1400000\t\t100
/home/user/D/.hibernate-swapfile        file\t\t16777212\t0\t\t10
";
        assert_eq!(parse_disk_swap_kb(swaps), 16_777_212);

        let zram_only = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/zram0                              partition\t8388604\t\t1400000\t\t100
";
        assert_eq!(parse_disk_swap_kb(zram_only), 0);
    }

    #[test]
    fn generated_sleep_fix_script_contains_delay_and_uninstall() {
        let script = generate_system_sleep_fix_script("2h").unwrap();
        assert!(script.contains("HibernateDelaySec=2h"));
        assert!(script.contains("HandleLidSwitch=suspend-then-hibernate"));
        assert!(script.contains("99-rustybattery-port-power.rules"));
        assert!(script.contains("99-rustybattery-port-suspend"));
        assert!(script.contains("--uninstall"));
        assert!(script.contains("/sys/block/$base_dev/removable"));
    }

    #[test]
    fn hibernate_delay_validator_rejects_shell_injection() {
        assert_eq!(validate_hibernate_delay("").unwrap(), "2h");
        assert_eq!(validate_hibernate_delay("90min").unwrap(), "90min");
        assert_eq!(validate_hibernate_delay("1h 30m").unwrap(), "1h 30m");
        assert!(validate_hibernate_delay("2h; id").is_err());
        assert!(validate_hibernate_delay("2h\nAllowSuspend=no").is_err());
        assert!(validate_hibernate_delay("$(whoami)").is_err());
        assert!(validate_hibernate_delay("10s").is_err());
        assert!(generate_system_sleep_fix_script("2h\"; id #").is_err());
    }

    #[test]
    fn stop_or_kill_protects_core_session_components() {
        for protected in [
            "RustyBattery",
            "GNOME Settings & Session",
            "XDG Desktop Portals",
            "Security, Auth & Keyring",
        ] {
            assert!(stop_or_kill_app(protected).is_err());
        }
    }
}
