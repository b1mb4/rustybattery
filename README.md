# 🦀 RustyBattery — Per-App Power Tracker for Linux

## 📋 Patch Notes

### **Version 0.4.0** *(Latest)*
- **🎯 Focused Window Battery Usage (`Focused %`) in Primary Stats Table:** Added a **`Focused %`** column right next to **`Batt %`** in both the interactive TUI primary table and `battery summary` CLI output, showing how much battery an application consumed while its window was actively focused in the foreground (`🎯` indicator on the active app, plus `Focused Drain` vs. `Bg Drain` in the App Inspector). Supports GNOME/Fedora `uresourced` cgroup focus boost, Hyprland (`hyprctl`), KDE Plasma (`kdotool`), and X11 (`xprop`).
- **🤖 Automatic "Smart Battery Saver" Rules (Daemon Mode Autopilot):** Added intelligent background autopilot rules (`battery autopilot [status|on|off]` or press **`a`** in the TUI):
  - Automatically switches Linux CPU power profile to `power-saver` when unplugged on battery and restores `balanced` when plugged into AC.
  - Automatically freezes (`cgroup.freeze = 1`) heavy background apps that have been unfocused for `>= 10 minutes` on battery while still drawing `>= 0.85 W`, and **instantly unfreezes/resumes** them the moment you focus their window again.
  - Low-battery threshold triggers at `<= 20%` (dims keyboard backlight & enforces powersave) and `<= 10%` (auto-stops background indexers & store updaters).
- **📝 Patch Notes Header:** Added a versioned patch notes changelog section at the top of `README.md` for every release.

<details>
<summary><b>Previous Releases (v0.1.0 – v0.3.0)</b></summary>

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
*(Installs both `rustybattery` and the `battery` command directly into `~/.cargo/bin`. Before publishing to crates.io, you can also run `cargo install --git https://github.com/b1mb4/rustybattery`.)*

### Option 2: One-Command Shell Installer (Pre-Built Binary)
```bash
curl -fsSL https://raw.githubusercontent.com/b1mb4/rustybattery/main/install.sh | bash
```

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
| `↑` / `↓` or `j` / `k` | Select application to inspect in the right-hand details panel |
| **`o`** | **⚡ Open Battery Optimizer** (scan & turn off unused background hogs, containers & services) |
| **`a`** | **🤖 Toggle Smart Battery Saver Autopilot** (automatic AC/Battery profile, 10m unfocused freeze & low-battery rules) |
| **`z`** | **❄️ Freeze / Resume App** (0% CPU Deep Sleep via cgroup v2 `cgroup.freeze` or `SIGSTOP`) |
| **`x`** | **Stop / Kill App** (terminates selected application or background service) |
| `Tab` or `1`–`4` | Switch time window (`1`: Since Full Charge, `2`: Last 6h, `3`: Last 24h, `4`: Last 7d) |
| `s` | Cycle sort column (`Battery %`, `Focused %`, `Energy Wh`, `Live Power W`, `Live CPU %`, `Memory MB`) |
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

## 🧠 How It Works Under the Hood

1. **Hardware Power Telemetry (`/sys/class/power_supply/`)**:
   - Reads real-time energy (`energy_now`/`charge_now`, `energy_full`, `energy_full_design`), instantaneous power draw (`power_now`/`current_now`), voltage, cycle count, and charge status across all system batteries while filtering out Bluetooth/HID peripheral batteries.
2. **Hybrid Cgroup v2 + `/proc` Attribution Engine + Window Focus Tracker**:
   - Reads microsecond-accurate CPU accounting (`cpu.stat`), resident memory (`memory.current`), and process counts (`pids.current`) across systemd cgroup v2 (`app.slice`, `session.slice`, `background.slice`, `system.slice`), and falls back to `/proc/[pid]/stat` + `/proc/[pid]/cmdline` for standalone window managers (Hyprland, Sway, i3, XFCE) and non-systemd Linux distributions.
   - Detects focused foreground applications via `uresourced` cgroup boost (`cpu.weight > 100` on GNOME/Fedora), Hyprland IPC (`hyprctl activewindow`), KDE (`kdotool`), and X11 (`xprop`) to separate **`Focused %`** from background drain.
3. **Instant First-Run Bootstrap + SQLite Persistence**:
   - Imports historical battery charge and discharge rate curves from UPower (`/var/lib/upower/history-*.dat`) and bootstraps current session app drain so charts and rankings are populated immediately on first launch.
   - Persists time-series snapshots and 5-minute app energy buckets to `~/.local/share/rustybattery/rustybattery.db`.

---

## 📄 License
MIT License — Created by [b1mb4](https://github.com/b1mb4).
