# 🦀 RustyBattery — Per-App Power Tracker for Linux

**RustyBattery** (`rustybattery` / `battery`) is a fast, lightweight terminal UI (`ratatui`) and background energy analyzer for Linux laptops. It brings the **phone-style "Battery Usage by App"** experience from Android and iOS to Linux—showing you exactly which applications, containers, and background services drained your battery (`%` and `Wh`), along with a built-in **Battery Optimizer** and **0% CPU App Freezer**.

---

## ✨ Features

- **📱 Phone-Style Per-App Battery Breakdown**
  - Attributes battery drain (`%` of capacity and `Wh`) across running applications, browsers, IDEs, containers, and system services.
  - Automatically groups multi-process apps (Firefox, Waterfox, Chrome, Chromium, Brave, VS Code, Cursor, Antigravity, Obsidian, Discord, Spotify, Docker/Podman containers) into clean application entries instead of raw subprocess names.
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

### System Requirements
- Linux with `/sys/class/power_supply/`
- Rust toolchain (`cargo`) and `sqlite3` (`libsqlite3-dev` on Ubuntu/Debian, `sqlite-devel` on Fedora, `sqlite` on Arch)

### Build & Install from Source
```bash
git clone https://github.com/b1mb4/rustybattery.git
cd rustybattery
cargo build --release

# Install binary, `battery` shortcut, and optional 24/7 systemd user service:
./target/release/rustybattery service install
```

Make sure `~/.local/bin` is in your `PATH`, then launch with:
```bash
battery
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
| `↑` / `↓` or `j` / `k` | Select application to inspect in the right-hand details panel |
| **`o`** | **⚡ Open Battery Optimizer** (scan & turn off unused background hogs, containers & services) |
| **`z`** | **❄️ Freeze / Resume App** (0% CPU Deep Sleep via cgroup v2 `cgroup.freeze` or `SIGSTOP`) |
| **`x`** | **Stop / Kill App** (terminates selected application or background service) |
| `Tab` or `1`–`4` | Switch time window (`1`: Since Full Charge, `2`: Last 6h, `3`: Last 24h, `4`: Last 7d) |
| `s` | Cycle sort column (`Battery %`, `Energy Wh`, `Live Power W`, `Live CPU %`, `Memory MB`) |
| `f` | Cycle category filter (`All Apps & Services`, `User Apps Only`, `Desktop & System`) |
| `c` | Toggle Battery `%` & Power (`W`) charts |
| `i` | Install & enable the 24/7 systemd user background service |
| `r` | Force immediate live sample refresh |
| `q` / `Esc` | Quit |

---

### 2. Battery Optimizer & App Controls (CLI)
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

### 3. Terminal & JSON Summary Reports
```bash
# Quick phone-style summary in the terminal
battery summary

# Look back 24 hours and show top 20 apps
battery summary --hours 24 --top 20

# Export structured JSON for scripts or status bars (Waybar / Polybar)
battery summary --json
```

---

### 4. 24/7 Background Tracking (Systemd User Service)
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

## 🧠 How It Works Under the Hood

1. **Hardware Power Telemetry (`/sys/class/power_supply/`)**:
   - Reads real-time energy (`energy_now`/`charge_now`, `energy_full`, `energy_full_design`), instantaneous power draw (`power_now`/`current_now`), voltage, cycle count, and charge status across all system batteries while filtering out Bluetooth/HID peripheral batteries.
2. **Hybrid Cgroup v2 + `/proc` Attribution Engine**:
   - Reads microsecond-accurate CPU accounting (`cpu.stat`), resident memory (`memory.current`), and process counts (`pids.current`) across systemd cgroup v2 (`app.slice`, `session.slice`, `background.slice`, `system.slice`), and falls back to `/proc/[pid]/stat` + `/proc/[pid]/cmdline` for standalone window managers (Hyprland, Sway, i3, XFCE) and non-systemd Linux distributions.
3. **Instant First-Run Bootstrap + SQLite Persistence**:
   - Imports historical battery charge and discharge rate curves from UPower (`/var/lib/upower/history-*.dat`) and bootstraps current session app drain so charts and rankings are populated immediately on first launch.
   - Persists time-series snapshots and 5-minute app energy buckets to `~/.local/share/rustybattery/rustybattery.db`.

---

## 📄 License
MIT License — Created by [b1mb4](https://github.com/b1mb4).
