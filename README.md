# 🦀 RustyBattery — Per-App Power Tracker for Linux

## 📋 Patch Notes

### **Version 0.14.1** *(Latest)*
- **🔒 Security, Safe-Mode & Concurrency Hardening:**
  - Exported files (`report`, `export`, `record-trace`) now stay `0600` without changing permissions on existing parent folders like `/tmp` or `~/Downloads`.
  - Validated `--hibernate-delay` in `battery sleep --setup-script`, prevented hibernate swapfile creation on removable USB drives, and required `--yes` for non-interactive `battery optimize`.
  - Broken `config.toml` files now strictly fall back to safe mode (`enabled = false`, `auto_freeze_enabled = false`), and state/config writes now use file locking (`flock`) with unique temp files.
  - Added core desktop/terminal protections to `battery stop`, honored `stop_grace_secs` during batch optimization, and fixed `battery resume --force-all` thaw counts.
- **⚡ Power Profiles, Keyboard Backlight & Calibration:**
  - Keyboard backlight dimming/restoring now uses unprivileged UPower D-Bus (`gdbus`) with sysfs fallback, and saved profile/backlight state is only cleared after restoration succeeds.
  - Fixed laptops getting stuck in `power-saver` after plugging into AC, deduplicated lid-close and sleep hooks, and properly restored profiles on wake.
  - Improved `battery calibrate` to handle TDP-limited multi-core laptops without under-attributing everyday apps, and smoothed the RAPL low-CPU transition.
- **🗄️ Database & TUI Reliability:**
  - Fixed SQLite `incremental_vacuum` and added `WAL` truncation on prune, cut redundant 5-second metadata writes, prevented idle sleep-monitor connections from holding WAL files open, and preserved worst-case confidence on live overlays.
  - Added safe terminal cleanup on startup errors, moved TUI `[x] Stop` off the UI thread to prevent freezes, excluded charging power from `Avg:` Watts, fixed config error banners not resurfacing, and fixed tab mouse click targets.

<details>
<summary><b>Previous Releases (v0.1.0 – v0.14.0)</b></summary>

- **Version 0.14.0:**
  - Clearer confidence labels showing whether app power is hardware-backed, modelled, or estimated, along with multi-package CPU energy support, idle power floor calibration, tighter idle bounds, and live telemetry trace recording/replay.
  - Restricted all local history, configuration, state, and exported report files to owner-only permissions, improved freeze guards (fail-safe audio, MPRIS, desktop idle-inhibitor), and added per-app hourly budgets, typo warnings, custom app matching rules, CSV/Prometheus exports, `charge-limit`, and shell completions.

- **Version 0.13.1:**
  - Restored full graceful shutdown timeouts when stopping apps, moved critical-battery cleanup off the main background tick loop, and unified tracking of frozen apps so every paused app resumes reliably.
  - Upgraded power calibration to a three-phase on-battery measurement, improved unprivileged kernel wakeup reporting, enforced release checksum verification in the installer, and added stricter automated quality checks.

- **Version 0.13.0:**
  - Added `battery report [--days 7]`, `battery export --format csv|prometheus`, `battery wakeups` + 80% charge-limit helper, `battery completions bash|zsh|fish`, `Shift+A` Alert Notifications toggle, per-app hourly budget alerts (`app_hourly_budget_pct`), daily `battery_health_daily` tracking, and mode-based history status aggregation.

- **Version 0.12.0:**
  - Added `battery calibrate`, hardware RAPL (`measured`) and confidence tags (`measured`/`modelled`/`estimated`), `~/.config/rustybattery/apps.toml` custom app rules, unbiased rolling-window power estimator, strict `config.toml` validation with safe-mode fallback & atomic saves, ownership-tracked `battery resume --all` (`--force-all`), per-PID audio guards, non-blocking stops (`--no-block`), SQLite incremental `auto_vacuum`, hardened systemd unit, and `SHA256SUMS` + `aarch64` releases.

- **Version 0.11.0:**
  - Safer defaults and confirmations (`[y/N]` prompt, graceful `SIGTERM` before `SIGKILL`, `battery resume --all`), power profile & keyboard backlight restore on wake/AC, audio/webcam/terminal freeze guards, `battery doctor`, per-unit CPU tracking, 80% charge-limit awareness, **Unattributed / Hardware & Idle** bucket, and `<1 ms` cached statusbar updates.

- **Version 0.10.0:**
  - Monotonic & suspend-aware clocks, logind `PrepareForSleep` D-Bus hook, statically bundled `rusqlite` with daily DB retention, `toml` config caching, in-memory TUI search/sort/filter, and unit test suite.

- **Version 0.9.0:**
  - Added `battery sleep [--fix]` audit & `s2idle_sleep_saver` lid-close rule, Unix manual pages (`man battery` / `man rustybattery`), and TUI layout clipping fixes.

- **Version 0.8.0:**
  - Screen-On Time (SoT) tracking via sysfs backlight/DPMS and Sleep/Suspend drain tracking (`%/hr`, `Wh`, `%`) across TUI, `battery summary`, and `--json`.

- **Version 0.7.0:**
  - Configurable Smart Saver Rules (`~/.config/rustybattery/config.toml`) with persistent thresholds and protected app `whitelist`.
  - Live TUI Rules Editor (`Shift+A`) & App Whitelist toggle (`w` with `🛡` badge), plus `battery autopilot whitelist|unwhitelist|config` CLI commands.

- **Version 0.6.0:**
  - Live TUI Search (`/`): filter apps in real time by name or category while preserving accurate system-wide `Battery Share` percentages.
  - Full Mouse Support in TUI: left-click rows, scroll wheel, click Time Range tabs & filter box, and right-click to open/close `🔍 App Battery Details`.
  - Updated packaging versions (`packaging/PKGBUILD` and `packaging/workflows/release.yml`) to `0.6.0`.

- **Version 0.5.0:**
  - 4-Tier Adaptive Responsive TUI Scaling: auto-hides `App Battery Details` & `Battery Level (%) History` on half-screen/tiled windows (`< 120 cols` or `< 28 rows`), adds on-demand `🔍 App Battery Details` modal (`Enter`/`d`), and progressively adapts columns (9 → 7 → 6), headers, and tabs.
- **Version 0.4.0:**
  - Added **`Focused %`** column to the primary stats table in TUI & `battery summary` (tracks battery drain when an app's window is focused in the foreground, with `🎯` active indicator).
  - Added **Automatic "Smart Battery Saver" Rules** (`battery autopilot [status|on|off]` / `a` in TUI): auto-switches CPU profile on AC/battery, auto-freezes heavy background apps unfocused `>= 10m` (`>= 0.85W`) and instantly resumes on focus, plus `<= 20%` and `<= 10%` low-battery triggers.
  - Added Patch Notes changelog header at the top of `README.md`.
- **Version 0.3.0:**
  - Added desktop status bar integration (`battery statusbar` for Waybar, Polybar, and Tmux with live hover tooltips).
  - Added native desktop notifications (`battery notify` & automatic low-battery / high-drain alerts in daemon mode).
  - Added dual binary targets (`rustybattery` + `battery` via `cargo install rustybattery`) and 1-command `install.sh` installer.
- **Version 0.2.0:**
  - Added interactive Battery Optimizer (`o` in TUI / `battery optimize`) to detect and turn off unused background hogs, containers, and indexers.
  - Added 0% CPU App Deep Sleep / Freeze (`z` in TUI / `battery freeze <app>`) using cgroup v2 `cgroup.freeze` and POSIX `SIGSTOP` fallback.
  - Expanded cross-distro & cross-desktop support (Flatpak, Snap, AppImage, `/proc` fallback for non-systemd & standalone WMs, multi-battery & Apple Silicon support).
- **Version 0.1.0:**
  - Initial release with phone-style per-app battery attribution, UPower history import, SQLite WAL storage, Braille battery/wattage charts, and 24/7 systemd user service.
</details>

---

**RustyBattery** (`rustybattery` / `battery`) is a fast, lightweight terminal UI (`ratatui`) and background energy analyzer for Linux laptops. It brings the **phone-style "Battery Usage by App"** experience from Android and iOS to Linux—showing you exactly which applications, containers, and background services drained your battery (`%`, `Focused %`, and `Wh`), along with a built-in **Battery Optimizer**, **Automatic Smart Battery Saver Rules**, and **0% CPU App Freezer**.

---

## ✨ Features

- **📱 Phone-Style Per-App Battery Breakdown (+ Focused Window Drain)**
  - Attributes battery drain (`Batt %`, **`Focused %`**, and `Wh`) across running applications, browsers, IDEs, containers, and system services.
  - Tracks **Focused Window Battery Usage (`Focused %`)** vs. background drain so you can immediately spot apps draining battery while unfocused in the background.
  - Automatically groups multi-process apps (Firefox, Waterfox, Chrome, Chromium, Brave, VS Code, Cursor, Antigravity, Obsidian, Discord, Spotify, Docker/Podman containers) into clean application entries instead of raw subprocess names.
- **🤖 Automatic "Smart Battery Saver" Rules (`a` in TUI / `battery autopilot`)**
  - **AC ↔ Battery Power Profile:** Switches to `power-saver` when unplugged on battery and restores `balanced` on AC.
  - **Idle Unfocused App Auto-Freeze:** Freezes heavy windowed apps (`cgroup.freeze = 1`) if unfocused for `>= 10 minutes` on battery while drawing `>= 0.85 W`, and **instantly resumes** them when focused again.
  - **Low-Battery Triggers:** Dims keyboard backlight & enforces powersave at `<= 20%`, and stops safe background hogs at `<= 10%`.
- **📈 High-Resolution Battery `%` & Power (`W`) Charts**
  - Smooth Braille (`Marker::Braille`) curves of battery level (`0–100%`) and instantaneous discharge wattage (`W`) across **Since Full Charge**, **Last 6 Hours**, **Last 24 Hours**, and **Last 7 Days**.
- **⚡ One-Click Battery Optimizer (`o` in TUI / `battery optimize`)**
  - Automatically detects and turns off unused background battery hogs (running Docker/Podman containers, desktop file indexers like Tracker/LocalSearch/Baloo, background store updaters like GNOME Software/Discover/PackageKit, idle local databases/AI servers like Ollama/Prometheus/MySQL/PostgreSQL, and crash watchers).
  - Switches your Linux CPU power profile to `powersave` automatically via `powerprofilesctl`, `tuned-adm`, `tlp`, or `cpufreq` sysfs.
- **❄️ 0% CPU App Deep Sleep / Freeze (`z` in TUI / `battery freeze <app>`)**
  - Suspends heavy background apps (browsers with dozens of open tabs, Electron apps, IDEs) to **0.00 W / 0% CPU** without closing their windows using Linux cgroup v2 (`cgroup.freeze`) or POSIX `SIGSTOP`/`SIGCONT` fallback.
- **🐧 Works Across All Linux Distributions & Desktops**
  - **Distributions:** Ubuntu, Debian, Fedora, Arch Linux, Manjaro, openSUSE, NixOS, Linux Mint, Pop!_OS, Void, Alpine, Gentoo.
  - **Desktops & Window Managers:** GNOME, KDE Plasma, COSMIC, Hyprland, Sway, i3, XFCE, Cinnamon, MATE.
  - **Packaging Formats:** Native binaries, Flatpaks, Snaps, AppImages, and Nix packages.
  - **Hardware:** Single- and dual-battery laptops (`BAT0`+`BAT1`), x86_64 ACPI, Apple Silicon (`macsmc-battery` on Asahi Linux), and ARM/Chromebook `sbs-*` batteries.

---

## 📦 Installation

### Option 1: Install via Cargo (Recommended)
```bash
cargo install rustybattery
```
*(Installs both `rustybattery` and the `battery` command directly into `~/.cargo/bin`.)*

### Option 2: One-Command Shell Installer (Pre-Built Binary, SHA256 Verified)
```bash
curl -fsSL https://raw.githubusercontent.com/b1mb4/rustybattery/main/install.sh | bash
```
*(Verifies `SHA256SUMS` automatically for both `x86_64` and `aarch64` releases. To verify manually or check GitHub build provenance: `sha256sum -c SHA256SUMS` or `gh attestation verify rustybattery-linux-x86_64.tar.gz -R b1mb4/rustybattery`.)*

### Option 3: Build from Source
```bash
git clone https://github.com/b1mb4/rustybattery.git
cd rustybattery
cargo install --path .
```

### Option 4: Arch Linux (`PKGBUILD`)
```bash
cd packaging && makepkg -si
```

---

## 🚀 Usage

### 1. Interactive Terminal UI (Default)
```bash
battery
# or:
rustybattery
```

#### TUI Keybindings
| Key | Action |
| --- | --- |
| `↑` / `↓` or `j` / `k` | Select application to inspect |
| **`Enter` / `d`** | **🔍 App Battery Details** (opens centered popup modal when tiled `< 120 cols`, or toggles side panel on wide screens) |
| **`o`** | **⚡ Open Battery Optimizer** (scan & turn off unused background hogs, containers & services) |
| **`a`** | **🤖 Toggle Smart Battery Saver Autopilot** (automatic AC/Battery profile, 10m unfocused freeze & low-battery rules) |
| **`z`** | **❄️ Freeze / Resume App** (0% CPU Deep Sleep via cgroup v2 `cgroup.freeze` or `SIGSTOP`) |
| **`x`** | **Stop / Kill App** (terminates selected application or background service) |
| `Tab` or `1`–`4` | Switch time window (`1`: Since Full Charge, `2`: Last 6h, `3`: Last 24h, `4`: Last 7d) |
| `s` | Cycle sort column (`Battery %`, `Focused %`, `Energy Wh`, `Live Power W`, `Live CPU %`, `Memory MB`) |
| **`/`** | **🔍 Live Search** (filter apps & categories in real time; `Enter` to apply, `Esc` to clear) |
| **`Mouse`** | **🖱️ Click rows/tabs/filter** (`Left-click` select/switch, `Scroll` list, `Right-click` app details popup) |
| `f` | Cycle category filter (`All Apps & Services`, `User Apps Only`, `Desktop & System`) |
| `c` | Toggle Battery `%` & Power (`W`) charts |
| `i` | Install & enable the 24/7 systemd user background service |
| `n` | Send a desktop notification with current battery stats & top draining apps |
| `r` | Force immediate live sample refresh |
| `q` / `Esc` | Quit |

---

### 2. Automatic "Smart Battery Saver" Rules (`battery autopilot`)
```bash
# View active Smart Battery Saver rules & last autopilot action
battery autopilot

# Enable or disable Smart Battery Saver Autopilot
battery autopilot on
battery autopilot off
```

---

### 3. Desktop Status Bar (Waybar / Polybar / Tmux) & Notifications
```bash
# Output Waybar JSON (with hover tooltip showing live top 5 battery-draining apps)
battery statusbar

# Output compact one-liner for Polybar or Tmux
battery statusbar --format polybar
battery statusbar --format tmux

# Print ready-to-paste config snippets for Waybar, Polybar, or Tmux
battery statusbar --print-config waybar
battery statusbar --print-config polybar

# Trigger a desktop notification with current battery status & top consumers
battery notify
```
When the background daemon (`rustybattery daemon`) is running, it also automatically sends native desktop notifications (`notify-send`) on **low battery (`<= 20%` and `<= 10%`)** and **sustained high battery drain (`>= 22 W`)** with the name of the top draining application.

---

### 4. Battery Optimizer & App Controls (CLI)
```bash
# Preview unused background services, containers, and estimated Watt/RAM savings
battery optimize --dry-run

# Automatically stop unused background hogs & switch CPU profile to powersave
battery optimize

# Freeze an app to 0% CPU (Deep Sleep) without losing open windows/tabs
battery freeze Waterfox
battery resume Waterfox

# Stop/kill a specific application or service
battery stop Obsidian
```

---

### 5. Terminal & JSON Summary Reports
```bash
# Quick phone-style summary in the terminal (includes Batt % and Focused %)
battery summary

# Look back 24 hours and show top 20 apps
battery summary --hours 24 --top 20

# Export structured JSON for scripts or status bars (Waybar / Polybar)
battery summary --json
```

---

### 6. 24/7 Background Tracking (Systemd User Service)
```bash
# Install and enable ~/.config/systemd/user/rustybattery.service
battery service install
systemctl --user daemon-reload && systemctl --user enable --now rustybattery.service

# Check service and SQLite database status
battery service status

# Uninstall background service
battery service uninstall
```

---

## 🧠 How It Works & Safety

1. **Hardware Power Telemetry (`/sys/class/power_supply/`)**:
   - Reads real-time energy (`energy_now`/`charge_now`, `energy_full`, `energy_full_design`), instantaneous power draw (`power_now`/`current_now`), charge limits (`charge_control_end_threshold`), voltage, cycle count, and charge status across all system batteries while filtering out Bluetooth/HID peripheral batteries.
2. **Per-Unit Cgroup v2 + `/proc` Attribution Engine + Window Focus Tracker**:
   - Computes per-unit CPU deltas (`cpu.stat` per systemd unit or `/proc/[pid]/stat` keyed by `(pid, starttime)`) so exiting containers or tabs never distort attribution, and scales CPU-attributed wattage by actual CPU utilization (placing non-CPU hardware draw into **Display & Backlight (est.)** and **Unattributed / Hardware & Idle**).
   - Detects focused foreground applications via `uresourced` cgroup boost (`cpu.weight > 100` on GNOME/Fedora), Hyprland IPC (`hyprctl activewindow`), Sway IPC (`swaymsg -t get_tree`), KDE (`kdotool`), and X11 (`xprop`). If focus is unknown, auto-freeze fails closed and never freezes apps.
3. **Safety Guards & Instant Recovery (`battery resume --all` / `battery doctor`)**:
   - Auto-freeze (`auto_freeze_enabled`) is **opt-in (`false` by default)**, skips apps playing/recording audio, using a webcam (`/dev/video*`), holding a systemd inhibitor, or hosting your current terminal, and automatically thaws frozen apps on daemon/TUI exit (`ExecStopPost=rustybattery resume --all`).
   - Run `battery doctor` at any time to inspect cgroup v2 support, window focus method, RAPL permissions, power-profile backend, and database health.

---

## 📊 Accuracy, Confidence & Privacy

- **Confidence Labels (`MeasuredTotalModelledSplit` / `Modelled` / `Estimated`):**
  - When multi-package Linux `intel-rapl` (`package-*`) energy counters are readable, package CPU energy is measured in hardware (after subtracting `calibrated_rapl_idle_w`) and split proportionally across active apps by per-unit CPU delta (`measured total, modelled split`).
  - When RAPL is restricted (kernel default since **`CVE-2020-8694`** mitigates unprivileged RAPL power side-channels) or unavailable, active CPU draw is estimated from `calibrated_cpu_w_per_core` (`modelled`), while **Display & Backlight (est.)** and **Unattributed / Hardware & Idle** carry `estimated`.
  - If you want hardware-measured package totals on a trusted single-user laptop, create a dedicated `power` group and grant group-only read access to `energy_uj` via a targeted udev rule (rather than world-readable `0444`). Check status anytime with `battery doctor`.
- **Trace Capture & Offline Replay (`battery record-trace`):**
  - Capture real sysfs/RAPL/cgroup snapshots with `battery record-trace --seconds 30 --interval 2 --output trace.jsonl` and replay them deterministically via `rustybattery::trace::replay_trace_str`.
- **Owner-Only State & Config Privacy (`0600` / `0700`):**
  - RustyBattery enforces `0700` directories and `0600` files for `rustybattery.db`, `config.toml`, `rb_frozen_units.json`, `status.json`, trace recordings, and `--output` report/export files so other local users cannot read per-app activity history.

---

## 📄 License
MIT License — Created by [b1mb4](https://github.com/b1mb4).
