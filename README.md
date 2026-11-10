# 🦀 RustyBattery — Phone-Style Battery & Per-App Power Tracker for Fedora Linux

`rustybattery` tracks laptop battery drain (`%` and `Wh`), discharge power (`W`), battery health, and **per-application battery usage** on Fedora Linux—giving you the same "Battery Usage by App" view found on Android and iOS.

## ✨ How It Works on Fedora

1. **Hardware Power Telemetry (`/sys/class/power_supply/BAT*`)**:
   - Reads real-time energy (`energy_now`, `energy_full`, `energy_full_design`), instantaneous power draw (`power_now`), voltage, cycle count, and charge status.
2. **Per-App Attribution via Systemd Cgroup v2 (`/sys/fs/cgroup`)**:
   - On Fedora (GNOME / KDE / systemd), desktop apps (including Flatpaks, browsers, terminals, IDEs, and background services) run in isolated systemd scopes and services (`app.slice`, `session.slice`, `background.slice`, `system.slice`).
   - `rustybattery` reads microsecond-accurate CPU accounting (`cpu.stat`), memory (`memory.current`), and process counts (`pids.current`) across every app cgroup—meaning even short-lived child processes (`rustc`, browser tab workers, etc.) are accurately rolled up into their parent application (`Waterfox`, `Antigravity`, `Obsidian`, `Ghostty`, `GNOME Shell`, `PipeWire Audio`, etc.).
3. **Instant First-Run Bootstrap + SQLite Persistence**:
   - Imports historical battery level and discharge rate curves from UPower (`/var/lib/upower/history-*.dat`) and bootstraps current session app drain from cgroup v2 counters so you get full charts and app rankings immediately on first launch.
   - Saves continuous time-series snapshots and 5-minute app energy buckets to `~/.local/share/rustybattery/rustybattery.db`.

---

## 🚀 Usage

### 1. Interactive Terminal UI (Default)
```bash
cargo run --release
# or if installed to ~/.local/bin:
rustybattery
```

#### TUI Keybindings
| Key | Action |
| --- | --- |
| `↑` / `↓` or `j` / `k` | Select application to inspect in the right-hand details panel |
| `Tab` or `1`–`4` | Switch time window (`1`: Since Full Charge, `2`: Last 6h, `3`: Last 24h, `4`: Last 7d) |
| `s` | Cycle sort column (`Battery %`, `Energy Wh`, `Live Power W`, `Live CPU %`, `Memory MB`) |
| `f` | Cycle category filter (`All Apps & Services`, `User Apps Only`, `Desktop & System`) |
| `c` | Toggle Battery `%` & Power (`W`) charts |
| `i` | Install & enable the 24/7 systemd user background service |
| `r` | Force immediate live sample refresh |
| `q` / `Esc` | Quit |

### 2. CLI Summary Report
```bash
rustybattery summary
rustybattery summary --hours 24 --top 20
rustybattery summary --json
```

### 3. 24/7 Background Tracking (Systemd User Service)
```bash
# Install and start ~/.config/systemd/user/rustybattery.service
rustybattery service install

# Check service and database status
rustybattery service status

# Uninstall service
rustybattery service uninstall
```
