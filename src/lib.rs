pub mod apps;
pub mod autopilot;
pub mod battery;
pub mod db;
pub mod guards;
pub mod optimize;
pub mod paths;
pub mod power;
pub mod report;
pub mod service;
pub mod statusbar;
pub mod trace;
pub mod tui;

use crate::apps::{detect_focused_apps, AppEnergyTracker, Focus};
use crate::autopilot::{
    config_file_path, get_autopilot_log, get_last_autopilot_action, is_app_whitelisted,
    is_autopilot_enabled, last_config_error, load_autopilot_config, save_autopilot_config,
    set_autopilot_enabled, thaw_persisted_frozen_apps, try_parse_config_str, AutopilotConfig,
};
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
use crate::optimize::{
    disable_noisy_acpi_wakeups, execute_optimizations, inspect_s2idle_status,
    scan_disabled_noisy_acpi_wakeups, scan_optimization_targets, set_acpi_wakeups_state,
    set_app_frozen, stop_or_kill_app, thaw_all_user_cgroups, thaw_owned_user_cgroups,
};
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "rustybattery",
    version,
    about = "RustyBattery — Per-App Power Tracker & Battery Optimizer for Linux"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Print a non-interactive phone-style battery report to stdout and exit
    #[arg(short, long)]
    summary: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Launch the interactive phone-style Terminal UI (default)
    Tui,

    /// Print a phone-style summary of battery status and per-app battery usage
    Summary {
        /// Number of hours to look back (0 = since last full charge)
        #[arg(long, default_value_t = 0)]
        hours: u64,

        /// Maximum number of applications to display
        #[arg(short, long, default_value_t = 15)]
        top: usize,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Scan and turn off unused/underused background services, containers, and power hogs
    Optimize {
        /// Preview detected optimization targets without stopping them
        #[arg(long)]
        dry_run: bool,

        /// Skip interactive confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Freeze (0% CPU Deep Sleep) a running user application without closing its windows
    Freeze {
        /// Application name (e.g., "Waterfox", "Obsidian")
        app: String,
    },

    /// Unfreeze / resume a frozen application (`--all` thaws units frozen by RustyBattery; `--force-all` thaws all user cgroups)
    Resume {
        /// Application name (e.g., "Waterfox", "Obsidian")
        app: Option<String>,

        /// Thaw all cgroup units that were frozen by RustyBattery
        #[arg(long)]
        all: bool,

        /// Force-thaw every frozen cgroup under the current user's slice tree (including externally frozen units)
        #[arg(long)]
        force_all: bool,
    },

    /// Stop or kill all processes/units belonging to an application
    Stop {
        /// Application name (e.g., "Docker", "Obsidian", "GNOME Software")
        app: String,
    },

    /// Output status bar JSON/text for Waybar, Polybar, or Tmux (with hover tooltip of top draining apps)
    Statusbar {
        /// Output format: "waybar" (JSON), "polybar", "tmux", or "plain"
        #[arg(short, long, default_value = "waybar")]
        format: String,

        /// Number of top applications to include in the hover tooltip
        #[arg(short, long, default_value_t = 5)]
        top: usize,

        /// Print a ready-to-paste config snippet for "waybar", "polybar", or "tmux"
        #[arg(long)]
        print_config: Option<String>,
    },

    /// Send a desktop notification with current battery stats & top draining apps
    Notify,

    /// Run the background collector daemon (used by the systemd user service)
    Daemon {
        /// Sampling interval in seconds
        #[arg(short, long, default_value_t = 5)]
        interval: u64,
    },

    /// Manage the 24/7 systemd user background service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Configure Automatic "Smart Battery Saver" Rules & App Whitelist
    Autopilot {
        /// Action: "status" (default), "on", "off", "whitelist", "unwhitelist", or "config"
        #[arg(default_value = "status")]
        mode: String,
        /// App name for whitelist/unwhitelist actions
        target: Option<String>,
        /// Validate config.toml syntax and report errors (`battery autopilot config --check`)
        #[arg(long)]
        check: bool,
        /// Write a fresh default config.toml (`battery autopilot config --init`)
        #[arg(long)]
        init: bool,
    },

    /// Audit s2idle / lid-closed sleep drain and apply immediate system-level improvements
    Sleep {
        /// Immediately apply s2idle sleep optimizations (rfkill Bluetooth, ACPI wakeups, Smart Saver lid rule)
        #[arg(long)]
        fix: bool,
        /// Revert s2idle optimizations (re-enable ACPI wakeups & unblock Bluetooth)
        #[arg(long)]
        undo: bool,
    },

    /// Check system capabilities (cgroup v2, focus detection, RAPL, power profiles, logind, config)
    Doctor,

    /// Calibrate baseline idle draw and CPU Watts-per-core on battery (idle + 1-core + all-core)
    Calibrate {
        /// Idle phase duration in seconds (default: 60)
        #[arg(long, default_value_t = 60)]
        idle_secs: u64,

        /// Duration of each CPU load phase (1-core and all-core) in seconds (default: 45)
        #[arg(long, default_value_t = 45)]
        load_secs: u64,

        /// Reset calibration parameters back to factory defaults
        #[arg(long)]
        reset: bool,
    },

    /// Generate a daily or weekly battery health & app usage report (Markdown or JSON)
    Report {
        /// Number of days to summarize (default: 7)
        #[arg(short, long, default_value_t = 7)]
        days: u64,

        /// Output format: "markdown" or "json"
        #[arg(short, long, value_enum, default_value_t = report::ReportFormat::Markdown)]
        format: report::ReportFormat,

        /// Maximum number of top apps to include
        #[arg(short, long, default_value_t = 10)]
        top: usize,

        /// Write report atomically to a file instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },

    /// Export battery telemetry as CSV or Prometheus textfile metrics
    Export {
        /// Output format: "csv" or "prometheus"
        #[arg(short, long, value_enum, default_value_t = report::ExportFormat::Csv)]
        format: report::ExportFormat,

        /// Hours of history to export (0 = since last full charge)
        #[arg(long, default_value_t = 24)]
        hours: u64,

        /// Export per-application CSV breakdown instead of battery history
        #[arg(long)]
        per_app: bool,

        /// Maximum number of top apps to include (0 = all)
        #[arg(short, long, default_value_t = 20)]
        top: usize,

        /// Write export atomically to a file (0600) instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },

    /// Show kernel wakeup sources (/sys/class/wakeup) and 80% battery charge-limit helper
    Wakeups {
        /// Maximum number of wakeup sources to display
        #[arg(short, long, default_value_t = 15)]
        top: usize,
    },

    /// Inspect or apply battery charge-limit threshold (`--set 80`) and show targeted udev rule
    ChargeLimit {
        /// Set charge_control_end_threshold percentage (20..=100)
        #[arg(long)]
        set: Option<u8>,
    },

    /// Record live sysfs/RAPL/cgroup snapshots to a JSONL trace file for offline replay
    RecordTrace {
        /// Total capture duration in seconds
        #[arg(short, long, default_value_t = 30)]
        seconds: u64,

        /// Sampling interval in seconds
        #[arg(short, long, default_value_t = 2)]
        interval: u64,

        /// Destination JSONL trace file path
        #[arg(short, long)]
        output: std::path::PathBuf,
    },

    /// Generate shell completion scripts for bash, zsh, or fish
    Completions {
        /// Target shell: "bash", "zsh", or "fish"
        #[arg(value_enum)]
        shell: report::CompletionShell,
    },

    /// View the manual page or install it for `man battery` / `man rustybattery`
    Man {
        /// Install man pages into ~/.local/share/man/man1/
        #[arg(long)]
        install: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Install and enable the systemd user service (~/.config/systemd/user/rustybattery.service)
    Install {
        /// Sampling interval in seconds
        #[arg(short, long, default_value_t = 5)]
        interval: u64,
    },
    /// Stop and remove the systemd user service
    Uninstall,
    /// Check the status of the background collector service and database
    Status,
    /// Downsample old samples (>7d) and prune expired history (>90d)
    Prune,
    /// Reset accumulated per-app usage buckets (keeps battery curve)
    ResetStats,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.summary {
        return print_summary(0, 15, false);
    }

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => {
            let db = Database::open(&default_db_path())?;
            tui::run_tui(db)
        }
        Commands::Summary { hours, top, json } => print_summary(hours, top, json),
        Commands::Optimize { dry_run, yes } => run_cli_optimize(dry_run, yes),
        Commands::Freeze { app } => {
            let msg = set_app_frozen(&app, true)?;
            println!("{msg}");
            Ok(())
        }
        Commands::Resume {
            app,
            all,
            force_all,
        } => {
            if force_all {
                let db = Database::open(&default_db_path())?;
                let _ = thaw_owned_user_cgroups();
                let n = thaw_all_user_cgroups() + thaw_persisted_frozen_apps(&db);
                println!("▶ Force-resumed all frozen user cgroups ({n} unit(s) thawed).");
                Ok(())
            } else if all || app.is_none() {
                let db = Database::open(&default_db_path())?;
                let owned = thaw_owned_user_cgroups();
                let persisted = thaw_persisted_frozen_apps(&db);
                let total = owned.len() + persisted;
                if owned.is_empty() {
                    println!("▶ Resumed RustyBattery-frozen applications ({total} unit(s) thawed).");
                } else {
                    println!(
                        "▶ Resumed RustyBattery-frozen applications ({total} unit(s) thawed: {}).",
                        owned.join(", ")
                    );
                }
                Ok(())
            } else if let Some(name) = app {
                let msg = set_app_frozen(&name, false)?;
                println!("{msg}");
                Ok(())
            } else {
                Ok(())
            }
        }
        Commands::Stop { app } => {
            let msg = stop_or_kill_app(&app)?;
            println!("{msg}");
            Ok(())
        }
        Commands::Statusbar {
            format,
            top,
            print_config,
        } => statusbar::run_statusbar(&format, top, print_config.as_deref()),
        Commands::Notify => {
            let db = Database::open(&default_db_path())?;
            let bat = read_battery_snapshot()?;
            let _ = db.ensure_initial_seed(&bat);
            let mut tracker = AppEnergyTracker::new();
            thread::sleep(Duration::from_millis(200));
            let (live_deltas, _) =
                tracker.sample_delta(bat.power_w, bat.energy_full_wh, bat.status.is_discharging());
            let since_ts = db.get_last_full_charge_ts();
            let mut apps = db.get_app_breakdown(since_ts, &live_deltas).unwrap_or_default();
            apps.sort_by(|a, b| {
                b.battery_pct
                    .partial_cmp(&a.battery_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            statusbar::send_status_notification(&bat, &apps)?;
            println!("🔔 Sent desktop notification with battery & top apps summary.");
            Ok(())
        }
        Commands::Daemon { interval } => service::run_daemon(interval),
        Commands::Service { action } => match action {
            ServiceAction::Install { interval } => {
                let msg = service::install_systemd_service(interval)?;
                println!("{msg}");
                Ok(())
            }
            ServiceAction::Uninstall => {
                let msg = service::uninstall_systemd_service()?;
                println!("{msg}");
                Ok(())
            }
            ServiceAction::Status => {
                let db_path = default_db_path();
                let db = Database::open(&db_path)?;
                let unit_path = service::systemd_unit_path();
                let installed = service::is_service_file_installed();
                let active = service::is_daemon_active(&db);
                println!("RustyBattery Service & Database Status");
                println!("──────────────────────────────────────────────────");
                println!("Database Path:   {}", db_path.display());
                println!(
                    "Systemd Unit:    {} ({})",
                    unit_path.display(),
                    if installed { "installed" } else { "not installed" }
                );
                println!(
                    "Daemon Active:   {}",
                    if active { "YES (running)" } else { "NO (idle)" }
                );
                println!(
                    "Smart Saver:     {}",
                    if is_autopilot_enabled(&db) {
                        "ENABLED (Automatic Smart Battery Saver Rules)"
                    } else {
                        "DISABLED"
                    }
                );
                let st = db.stats();
                println!(
                    "Database Size:   {:.1} KB ({} battery rows, {} app buckets, {} sleep sessions)",
                    (st.file_size_bytes as f64) / 1024.0,
                    st.battery_rows,
                    st.app_rows,
                    st.sleep_sessions
                );
                Ok(())
            }
            ServiceAction::Prune => {
                let db = Database::open(&default_db_path())?;
                let rep = db.prune(chrono::Utc::now().timestamp())?;
                println!(
                    "✔ Pruned database: downsampled {} rows, deleted {} old battery rows & {} old app buckets{}",
                    rep.downsampled_rows,
                    rep.deleted_battery_rows,
                    rep.deleted_app_rows,
                    if rep.vacuumed { " (VACUUM completed)" } else { "" }
                );
                Ok(())
            }
            ServiceAction::ResetStats => {
                let db = Database::open(&default_db_path())?;
                let n = db.reset_app_usage()?;
                println!("✔ Cleared {n} per-app usage bucket(s). Fresh attribution started.");
                Ok(())
            }
        },
        Commands::Autopilot {
            mode,
            target,
            check,
            init,
        } => {
            let db = Database::open(&default_db_path())?;
            match mode.to_lowercase().as_str() {
                "on" | "enable" | "true" | "1" => {
                    let msg = set_autopilot_enabled(&db, true)?;
                    println!("✔ {msg}");
                }
                "off" | "disable" | "false" | "0" => {
                    let msg = set_autopilot_enabled(&db, false)?;
                    println!("✔ {msg}");
                }
                "whitelist" => {
                    let Some(app_name) = target else {
                        anyhow::bail!("Usage: battery autopilot whitelist <APP_NAME>");
                    };
                    let mut cfg = load_autopilot_config();
                    if !is_app_whitelisted(&cfg, &app_name) {
                        cfg.whitelist.push(app_name.clone());
                        save_autopilot_config(&cfg)?;
                        let _ = set_app_frozen(&app_name, false);
                        println!("🛡 Added `{app_name}` to Smart Saver whitelist.");
                    } else {
                        println!("🛡 `{app_name}` is already in the Smart Saver whitelist.");
                    }
                }
                "unwhitelist" => {
                    let Some(app_name) = target else {
                        anyhow::bail!("Usage: battery autopilot unwhitelist <APP_NAME>");
                    };
                    let mut cfg = load_autopilot_config();
                    let lower = app_name.trim().to_lowercase();
                    let before = cfg.whitelist.len();
                    cfg.whitelist.retain(|w| w.trim().to_lowercase() != lower);
                    if cfg.whitelist.len() < before {
                        save_autopilot_config(&cfg)?;
                        println!("✔ Removed `{app_name}` from Smart Saver whitelist.");
                    } else {
                        println!("`{app_name}` was not in the Smart Saver whitelist.");
                    }
                }
                "config" => {
                    let path = config_file_path();
                    if init {
                        let def = AutopilotConfig::default();
                        save_autopilot_config(&def)?;
                        println!("✔ Initialized default config at {}", path.display());
                        return Ok(());
                    }
                    if check {
                        let raw = std::fs::read_to_string(&path)?;
                        let cfg = try_parse_config_str(&raw)?;
                        println!(
                            "✔ Config at {} is valid (enabled={}, auto_freeze={}, whitelist={})",
                            path.display(),
                            cfg.enabled,
                            cfg.auto_freeze_enabled,
                            cfg.whitelist.len()
                        );
                        return Ok(());
                    }
                    println!("Config Path: {}", path.display());
                    if let Ok(raw) = std::fs::read_to_string(path) {
                        println!("\n{raw}");
                    } else {
                        println!("(not created yet — run `battery autopilot config --init` to create)");
                    }
                }
                _ => {
                    let enabled = is_autopilot_enabled(&db);
                    let cfg = load_autopilot_config();
                    let last_action = get_last_autopilot_action(&db);
                    let logs = get_autopilot_log(&db);
                    println!("🤖 RustyBattery — Smart Battery Saver Rules & Whitelist");
                    println!("──────────────────────────────────────────────────────────────────────");
                    println!(
                        "Autopilot Status:     {}",
                        if enabled && cfg.enabled { "● ENABLED" } else { "○ DISABLED" }
                    );
                    println!("Config File:          {}", config_file_path().display());
                    println!("Last Action:          {}", last_action);
                    println!();
                    println!("Configured Rules:");
                    println!(
                        "  • Auto-Switch Power Profile:   {}",
                        if cfg.auto_switch_power_profile { "ON" } else { "OFF" }
                    );
                    println!(
                        "  • Auto-Freeze Idle Apps:       {}",
                        if cfg.auto_freeze_enabled { "ON" } else { "OFF" }
                    );
                    println!("  • Idle Freeze Timeout:         {} min", cfg.idle_freeze_mins);
                    println!("  • Idle Power Threshold:        {:.1} W", cfg.idle_power_threshold_w);
                    println!("  • Low Battery Threshold:       {:.0}%", cfg.low_battery_pct);
                    println!("  • Critical Battery Threshold:  {:.0}%", cfg.critical_battery_pct);
                    println!(
                        "  • Dim Keyboard on Low Battery: {}",
                        if cfg.dim_kbd_on_low_battery { "ON" } else { "OFF" }
                    );
                    println!(
                        "  • s2idle Lid-Close Saver:      {}",
                        if cfg.s2idle_sleep_saver { "ON (auto-blocks BT & enforces power-saver)" } else { "OFF" }
                    );
                    println!(
                        "  • Alert Notifications:         {}",
                        if cfg.notifications_enabled { "ON" } else { "OFF" }
                    );
                    println!("  • Stop Grace Period:           {} s", cfg.stop_grace_secs);
                    println!();
                    println!(
                        "Protected Whitelist ({}): {}",
                        cfg.whitelist.len(),
                        if cfg.whitelist.is_empty() {
                            "(none)".to_string()
                        } else {
                            cfg.whitelist.join(", ")
                        }
                    );
                    if !logs.is_empty() {
                        println!();
                        println!("Recent Activity Log:");
                        for entry in logs.iter().take(5) {
                            println!("  • {entry}");
                        }
                    }
                    println!();
                    println!("Commands:");
                    println!("  battery autopilot on|off");
                    println!("  battery autopilot whitelist <app> | unwhitelist <app>");
                    println!("  battery autopilot config");
                    println!("  TUI: press `A` to edit rules live, `w` to whitelist selected app");
                }
            }
            Ok(())
        }
        Commands::Sleep { fix, undo } => {
            let db = Database::open(&default_db_path())?;
            let since_24h = chrono::Utc::now().timestamp() - 24 * 3600;
            let drain = db.get_period_drain_stats(since_24h).unwrap_or_default();
            let mut cfg = load_autopilot_config();

            if undo {
                println!("↩ Reverting s2idle Sleep Optimizations...");
                println!("──────────────────────────────────────────────────────────────────────");
                if crate::autopilot::set_bluetooth_blocked(false) {
                    println!("  ✔ Unblocked Bluetooth radio (`rfkill unblock bluetooth`)");
                }
                let disabled = scan_disabled_noisy_acpi_wakeups();
                if !disabled.is_empty() {
                    let devs: Vec<String> = disabled.into_iter().map(|(d, _)| d).collect();
                    match set_acpi_wakeups_state(&devs, true) {
                        Ok(n) => println!(
                            "  ✔ Re-enabled {n} ACPI wakeup trigger(s): {}",
                            devs.join(", ")
                        ),
                        Err(e) => println!("  ⚠ {e}"),
                    }
                } else {
                    println!("  ✔ All ACPI USB/PCI wakeup triggers are already enabled");
                }
                println!();
            } else if fix {
                println!("⚡ Applying Immediate s2idle System-Level Improvements...");
                println!("──────────────────────────────────────────────────────────────────────");
                if !cfg.s2idle_sleep_saver {
                    cfg.s2idle_sleep_saver = true;
                }
                save_autopilot_config(&cfg)?;
                println!("  ✔ Enabled `s2idle_sleep_saver = true` in {}", config_file_path().display());

                if crate::autopilot::is_bluetooth_unblocked() {
                    if crate::autopilot::set_bluetooth_blocked(true) {
                        println!("  ✔ Soft-blocked Bluetooth radio via `rfkill block bluetooth`");
                    }
                } else {
                    println!("  ✔ Bluetooth radio is already soft-blocked (`rfkill`)");
                }

                let st = inspect_s2idle_status();
                if !st.noisy_wakeups.is_empty() {
                    let devs: Vec<String> = st.noisy_wakeups.iter().map(|(d, _)| d.clone()).collect();
                    match disable_noisy_acpi_wakeups(&devs) {
                        Ok(n) => println!(
                            "  ✔ Disabled {n} noisy ACPI USB/PCI wakeup trigger(s): {}",
                            devs.join(", ")
                        ),
                        Err(e) => {
                            println!("  ⚠ {e}");
                        }
                    }
                } else {
                    println!("  ✔ No noisy ACPI USB/PCI wakeup triggers are enabled");
                }
                println!();
            }

            let st = inspect_s2idle_status();
            println!("🌙 RustyBattery — s2idle & Lid-Closed Sleep Drain Audit");
            println!("──────────────────────────────────────────────────────────────────────");
            println!("Kernel Sleep Mode:    {}", st.mem_sleep);
            println!(
                "Laptop Lid State:     {}",
                if st.lid_closed { "Closed" } else { "Open" }
            );
            println!(
                "Suspend Cycles:       {} succeeded, {} failed",
                st.suspend_success, st.suspend_fail
            );
            println!(
                "24h Sleep Drain:      {:.1}% lost over {}h {:02}m ({:.2}%/hr)",
                drain.sleep_drain_pct,
                drain.sleep_secs / 3600,
                (drain.sleep_secs % 3600) / 60,
                drain.sleep_drain_pct_per_hr
            );
            println!(
                "s2idle Lid Saver:     {}",
                if cfg.s2idle_sleep_saver {
                    "● ENABLED (auto-blocks BT & enforces power-saver on lid close)"
                } else {
                    "○ DISABLED"
                }
            );
            println!(
                "Bluetooth Radio:      {}",
                if st.bluetooth_unblocked {
                    "⚠ ACTIVE (can prevent deep S0ix residency; run `battery sleep --fix`)"
                } else {
                    "✔ Soft-blocked (0W in s2idle)"
                }
            );
            if st.noisy_wakeups.is_empty() {
                println!("Noisy ACPI Wakeups:   ✔ All noisy USB/PCI triggers disabled");
            } else {
                let list: Vec<String> = st
                    .noisy_wakeups
                    .iter()
                    .map(|(d, n)| format!("{d} ({n})"))
                    .collect();
                println!(
                    "Noisy ACPI Wakeups:   ⚠ {} enabled: {}",
                    st.noisy_wakeups.len(),
                    list.join(", ")
                );
                let devs: Vec<String> = st.noisy_wakeups.iter().map(|(d, _)| d.clone()).collect();
                let script = devs
                    .iter()
                    .map(|d| format!("echo {d} > /proc/acpi/wakeup"))
                    .collect::<Vec<_>>()
                    .join(" && ");
                println!("  → Fix command:      sudo sh -c '{script}'");
            }
            println!(
                "Hibernate Delay:      {}",
                if st.hibernate_delay_configured {
                    "✔ Configured in /etc/systemd/sleep.conf"
                } else {
                    "ℹ Not set (optional: set `HibernateDelaySec=2h` in /etc/systemd/sleep.conf)"
                }
            );
            let sessions = db.get_sleep_sessions(5);
            if !sessions.is_empty() {
                println!();
                println!("Recent Recorded Sleep Sessions:");
                for s in sessions {
                    let dur_m = s.duration_secs() / 60;
                    println!(
                        "  • {}h {:02}m — {:.1}% → {:.1}% (-{:.1}%, {:.2}%/hr)",
                        dur_m / 60,
                        dur_m % 60,
                        s.start_pct,
                        s.end_pct,
                        s.drain_pct(),
                        s.drain_pct_per_hr()
                    );
                }
            }
            println!();
            println!("Tip: Run `battery sleep --fix` (or `--undo` to revert), or press `o` in TUI (`battery`).");
            Ok(())
        }
        Commands::Doctor => run_doctor(),
        Commands::Calibrate {
            idle_secs,
            load_secs,
            reset,
        } => run_calibrate(idle_secs, load_secs, reset),
        Commands::Report {
            days,
            format,
            top,
            output,
        } => {
            let db = Database::open(&default_db_path())?;
            report::print_report(&db, days, format, top, output.as_deref())
        }
        Commands::Export {
            format,
            hours,
            per_app,
            top,
            output,
        } => {
            let db = Database::open(&default_db_path())?;
            report::print_export(&db, format, hours, per_app, top, output.as_deref())
        }
        Commands::Wakeups { top } => report::print_wakeup_report(top),
        Commands::ChargeLimit { set } => report::run_charge_limit(set),
        Commands::RecordTrace {
            seconds,
            interval,
            output,
        } => trace::record_trace(seconds, interval, &output),
        Commands::Completions { shell } => {
            let cmd = Cli::command();
            print!("{}", report::generate_shell_completions(shell, &cmd));
            Ok(())
        }
        Commands::Man { install } => {
            const MAN_PAGE: &str = include_str!("../man/rustybattery.1");
            if install {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                let man_root = std::path::PathBuf::from(&home).join(".local/share/man");
                let man_dir = man_root.join("man1");
                std::fs::create_dir_all(&man_dir)?;
                std::fs::write(man_dir.join("rustybattery.1"), MAN_PAGE)?;
                std::fs::write(man_dir.join("battery.1"), ".so man1/rustybattery.1\n")?;
                let manpath_cfg = std::path::PathBuf::from(&home).join(".manpath");
                let man_line = format!("MANDATORY_MANPATH\t{}\n", man_root.display());
                let existing = std::fs::read_to_string(&manpath_cfg).unwrap_or_default();
                if !existing.contains(&man_root.display().to_string()) {
                    let updated = format!("{existing}{man_line}");
                    let _ = std::fs::write(manpath_cfg, updated);
                }
                println!("✔ Installed manual pages to {}", man_dir.display());
                println!("  Try: `man battery` or `man rustybattery`");
                return Ok(());
            }

            use std::io::Write;
            use std::process::{Command, Stdio};
            if let Ok(mut child) = Command::new("man").arg("-l").arg("-").stdin(Stdio::piped()).spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(MAN_PAGE.as_bytes());
                }
                let _ = child.wait();
            } else {
                println!("{MAN_PAGE}");
            }
            Ok(())
        }
    }
}

fn print_summary(hours: u64, top: usize, json: bool) -> Result<()> {
    let db = Database::open(&default_db_path())?;
    let bat = read_battery_snapshot()?;
    let daemon_active = service::is_daemon_active(&db);
    if !daemon_active {
        db.ensure_initial_seed(&bat)?;
    }

    let mut tracker = AppEnergyTracker::new();
    thread::sleep(Duration::from_millis(250));
    let (live_deltas, _) =
        tracker.sample_delta(bat.power_w, bat.energy_full_wh, bat.status.is_discharging());

    let since_ts = if hours == 0 {
        db.get_last_full_charge_ts()
    } else {
        chrono::Utc::now().timestamp() - (hours as i64) * 3600
    };

    let drain_stats = db.get_period_drain_stats(since_ts).unwrap_or_default();
    let mut apps = db.get_app_breakdown(since_ts, &live_deltas)?;
    apps.sort_by(|a, b| {
        b.battery_pct
            .partial_cmp(&a.battery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if json {
        let out = serde_json::json!({
            "battery": bat,
            "since_timestamp": since_ts,
            "autopilot_enabled": is_autopilot_enabled(&db),
            "drain_stats": drain_stats,
            "apps": apps.into_iter().take(top).map(|a| serde_json::json!({
                "app_name": a.app_name,
                "category": a.category,
                "battery_pct": a.battery_pct,
                "focused_battery_pct": a.focused_battery_pct,
                "focused_secs": a.focused_secs,
                "is_focused": a.is_focused,
                "share_of_drain_pct": a.share_of_drain_pct,
                "energy_wh": a.energy_wh,
                "live_power_w": a.live_power_w,
                "live_cpu_pct": a.live_cpu_pct,
                "cpu_secs": a.cpu_secs,
                "memory_mb": a.memory_mb,
                "pids_count": a.pids_count,
                "confidence": a.confidence.as_str(),
            })).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let time_rem = match bat.time_remaining_mins {
        Some(m) => format!("~{}h {:02}m", m / 60, m % 60),
        None => "N/A".to_string(),
    };

    println!("╭────────────────────────────────────────────────────────────────────────────────────────╮");
    println!(
        "│ 🦀 {:<83} │",
        format!(
            "RustyBattery v{} — Per-App Power Tracker for Linux",
            env!("CARGO_PKG_VERSION")
        )
    );
    println!("├────────────────────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│ Battery: {:>5.1}% ({})   Power: {:>5.2} W   Remaining: {:<10}                 │",
        bat.capacity_pct,
        bat.status.as_str(),
        bat.power_w,
        time_rem
    );
    println!(
        "│ Energy:  {:>5.1} / {:>5.1} Wh (Design: {:>5.1} Wh)   Health: {:>5.1}% ({} cycles)           │",
        bat.energy_now_wh,
        bat.energy_full_wh,
        bat.energy_design_wh,
        bat.health_pct,
        bat.cycle_count
    );
    let sot_secs = if drain_stats.sot_secs > 0 {
        drain_stats.sot_secs
    } else {
        drain_stats.awake_total_secs
    };
    let sot_fmt = format!("{}h {:02}m", sot_secs / 3600, (sot_secs % 3600) / 60);
    let sleep_fmt = if drain_stats.sleep_secs >= 60 {
        format!(
            "{}h {:02}m ({:.1}% / {:.1}%/hr)",
            drain_stats.sleep_secs / 3600,
            (drain_stats.sleep_secs % 3600) / 60,
            drain_stats.sleep_drain_pct,
            drain_stats.sleep_drain_pct_per_hr
        )
    } else {
        "0h 00m (0.0%)".to_string()
    };
    println!(
        "│ Screen-On Time: {:<10}   Sleep/Suspend Drain: {:<34} │",
        sot_fmt, sleep_fmt
    );
    println!("╰────────────────────────────────────────────────────────────────────────────────────────╯");
    println!();
    println!(
        "  {:<3} {:<22} {:<13} {:<16} {:>7} {:>9} {:>8} {:>7} {:>8}",
        "#", "Application", "Category", "Share of Drain", "Batt %", "Focused %", "Energy", "Live W", "Memory"
    );
    println!("  ──────────────────────────────────────────────────────────────────────────────────────────────────");

    for (i, app) in apps.into_iter().take(top).enumerate() {
        let filled = ((app.share_of_drain_pct / 100.0) * 10.0).round() as usize;
        let bar = format!(
            "{}{}",
            "█".repeat(filled.min(10)),
            "░".repeat(10usize.saturating_sub(filled.min(10)))
        );
        let name_label = if app.is_focused {
            format!("🎯 {}", app.app_name)
        } else {
            app.app_name.clone()
        };
        println!(
            "  {:>2}. {:<22} {:<13} {} {:>4.0}% {:>6.2}% {:>8.2}% {:>5.2} Wh {:>5.2} W {:>5.0} MB",
            i + 1,
            name_label.chars().take(22).collect::<String>(),
            app.category.chars().take(13).collect::<String>(),
            bar,
            app.share_of_drain_pct,
            app.battery_pct,
            app.focused_battery_pct,
            app.energy_wh,
            app.live_power_w,
            app.memory_mb
        );
    }
    println!();
    println!("  Tip: Run `battery` for the interactive TUI, `battery autopilot` for Smart Saver rules, or `battery optimize`.");

    Ok(())
}

fn run_cli_optimize(dry_run: bool, yes: bool) -> Result<()> {
    let bat = read_battery_snapshot()?;
    let targets = scan_optimization_targets(bat.power_w);

    println!("╭──────────────────────────────────────────────────────────────────────────────╮");
    println!(
        "│ ⚡ RustyBattery Optimizer — Background Hogs & Power-Saving Scanner            │"
    );
    println!("├──────────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│ Current Battery: {:>5.1}% ({})   Live Power Draw: {:>5.2} W              │",
        bat.capacity_pct,
        bat.status.as_str(),
        bat.power_w
    );
    println!("╰──────────────────────────────────────────────────────────────────────────────╯");
    println!();

    if targets.is_empty() {
        println!("  ✨ Your system is already optimized! No unused background battery hogs found.");
        return Ok(());
    }

    let selected_targets: Vec<_> = targets.iter().filter(|t| t.selected).collect();
    let total_watts: f64 = selected_targets.iter().map(|t| t.estimated_watts).sum();
    let total_ram: f64 = selected_targets.iter().map(|t| t.memory_mb).sum();
    let total_pids: u32 = selected_targets.iter().map(|t| t.pids_count).sum();

    println!("  Detected {} candidate(s) ([x] = selected by default, [ ] = opt-in in TUI `o`):", targets.len());
    println!("  ──────────────────────────────────────────────────────────────────────────────");
    for (i, t) in targets.iter().enumerate() {
        let mark = if t.selected { "[x]" } else { "[ ]" };
        if t.memory_mb > 0.0 {
            println!(
                "  {:>2}. {} {:<34} ~{:>4.2} W est.  {:>5.0} MB RAM   ({:>3} threads)",
                i + 1,
                mark,
                t.display_name,
                t.estimated_watts,
                t.memory_mb,
                t.pids_count
            );
        } else {
            println!(
                "  {:>2}. {} {:<34} ~{:>4.2} W est.  (System Power Tunable)",
                i + 1,
                mark,
                t.display_name,
                t.estimated_watts
            );
        }
        println!("          └─ {}", t.reason);
    }
    println!();
    println!(
        "  Selected Savings (est.): ~{:.2} W reduction  •  {:.0} MB RAM  •  {} threads",
        total_watts, total_ram, total_pids
    );

    if dry_run || selected_targets.is_empty() {
        println!("\n  (Dry-run / no default targets selected. Run `battery` and press `o` to pick targets interactively.)");
        return Ok(());
    }

    if !yes && unsafe { libc::isatty(libc::STDIN_FILENO) == 1 } {
        use std::io::{self, Write};
        print!("\n  Proceed with applying {} selected optimization(s)? [y/N]: ", selected_targets.len());
        let _ = io::stdout().flush();
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    println!("\n  Applying optimizations...");
    let report = execute_optimizations(&targets);

    for action in &report.actions_taken {
        println!("   ✔ {action}");
    }
    for warn in &report.failed_actions {
        println!("   ℹ {warn}");
    }

    println!();
    if !report.actions_taken.is_empty() {
        let new_power = (bat.power_w - report.estimated_watts_saved).max(1.0);
        let old_mins = if bat.power_w > 0.5 {
            (bat.energy_now_wh / bat.power_w) * 60.0
        } else {
            0.0
        };
        let new_mins = if bat.power_w > 0.5 {
            (bat.energy_now_wh / new_power) * 60.0
        } else {
            0.0
        };
        let gained_mins = (new_mins - old_mins).max(0.0).round() as u32;

        println!(
            "  🎉 Done! Freed {:.0} MB RAM, stopped {} threads, saved ~{:.1} W est. (+{}m est. battery time).",
            report.freed_memory_mb,
            report.stopped_pids,
            report.estimated_watts_saved,
            gained_mins
        );
    }

    Ok(())
}

fn run_doctor() -> Result<()> {
    println!("🩺 RustyBattery v{} — System Capabilities & Health Doctor", env!("CARGO_PKG_VERSION"));
    println!("──────────────────────────────────────────────────────────────────────");

    match read_battery_snapshot() {
        Ok(bat) => {
            println!(
                "✔ Battery sysfs:        {} ({} {}, {:.1}% · {:.2} W, charge limit: {})",
                bat.device_name,
                bat.manufacturer,
                bat.model_name,
                bat.capacity_pct,
                bat.power_w,
                bat.charge_limit_pct
                    .map(|l| format!("{l}%"))
                    .unwrap_or_else(|| "none/100%".to_string())
            );
        }
        Err(e) => println!("✖ Battery sysfs:        ERROR ({e})"),
    }

    let uid = unsafe { libc::getuid() };
    let app_slice = std::path::PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ));
    if app_slice.exists() {
        println!("✔ cgroup v2 hierarchy:  {} (available, cgroup.freeze supported)", app_slice.display());
    } else {
        println!("⚠ cgroup v2 hierarchy:  user app.slice not found (using /proc + SIGSTOP fallback)");
    }

    match detect_focused_apps() {
        Focus::Known(set) => {
            let list: Vec<_> = set.into_iter().collect();
            println!("✔ Window Focus:         Known (currently focused: {})", list.join(", "));
        }
        Focus::Unknown => {
            println!("⚠ Window Focus:         Unknown (auto-freeze will fail-closed / stay disabled)");
        }
    }

    if crate::battery::read_rapl_package_energy_uj().is_some() {
        println!("✔ Hardware RAPL:        readable (multi-package energy_uj active)");
    } else if std::path::Path::new("/sys/class/powercap/intel-rapl:0/energy_uj").exists()
        || std::path::Path::new("/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj").exists()
    {
        println!(
            "ℹ Hardware RAPL:        present but root-only (kernel default due to CVE-2020-8694; on single-user laptops you may optionally restrict read access to a dedicated 'power' group via udev)"
        );
    } else {
        println!("ℹ Hardware RAPL:        not exposed on this platform");
    }

    match crate::power::get_power_profile() {
        Some((backend, prof)) => {
            println!("✔ Power Profile:        {backend} (active: {prof})");
        }
        None => {
            println!("⚠ Power Profile:        neither powerprofilesctl nor tuned-adm detected");
        }
    }

    let s2 = inspect_s2idle_status();
    println!(
        "✔ Sleep & Wakeups:      mode={}, lid={}, noisy ACPI wakeups enabled={}",
        s2.mem_sleep,
        if s2.lid_closed { "closed" } else { "open" },
        s2.noisy_wakeups.len()
    );

    let cfg_path = config_file_path();
    let _ = load_autopilot_config();
    if let Some(err) = last_config_error() {
        println!(
            "✖ Config validation:    {} (SAFE MODE — auto_freeze disabled: {err})",
            cfg_path.display()
        );
    } else if cfg_path.exists() {
        match std::fs::read_to_string(&cfg_path).and_then(|s| {
            try_parse_config_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(cfg) => println!(
                "✔ Config validation:    {} (valid, config_version={}, retention={}d, auto_freeze={})",
                cfg_path.display(),
                cfg.config_version,
                cfg.retention_days,
                cfg.auto_freeze_enabled
            ),
            Err(e) => println!("✖ Config validation:    {} ({e})", cfg_path.display()),
        }
    } else {
        println!("✔ Config validation:    using defaults ({} not created yet)", cfg_path.display());
    }

    let rules = crate::apps::load_user_app_rules();
    if let Some(err) = crate::apps::last_apps_rules_error() {
        println!("✖ Custom App Rules:     apps.toml error ({err})");
    } else {
        println!(
            "✔ Custom App Rules:     {} rule(s) active (apps.toml)",
            rules.len()
        );
    }

    let cfg = load_autopilot_config();
    match cfg.calibrated_at {
        None => println!(
            "ℹ Power Model:          not calibrated (defaults: baseline={:.2} W, cpu={:.2} W/core, rapl_idle={:.2} W; run `battery calibrate`)",
            cfg.calibrated_baseline_w, cfg.calibrated_cpu_w_per_core, cfg.calibrated_rapl_idle_w
        ),
        Some(ts) => {
            let hit_clamp = cfg.calibrated_baseline_w <= 0.31
                || cfg.calibrated_baseline_w >= 14.99
                || cfg.calibrated_cpu_w_per_core <= 1.01
                || cfg.calibrated_cpu_w_per_core >= 44.99;
            let date = chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| ts.to_string());
            if hit_clamp {
                println!(
                    "⚠ Power Model:          calibrated on {date}, value hit clamp (baseline={:.2} W, cpu={:.2} W/core, rapl_idle={:.2} W) — re-run `battery calibrate` or `battery calibrate --reset`",
                    cfg.calibrated_baseline_w, cfg.calibrated_cpu_w_per_core, cfg.calibrated_rapl_idle_w
                );
            } else {
                println!(
                    "✔ Power Model:          calibrated on {date} (baseline={:.2} W, cpu={:.2} W/core, rapl_idle={:.2} W)",
                    cfg.calibrated_baseline_w, cfg.calibrated_cpu_w_per_core, cfg.calibrated_rapl_idle_w
                );
            }
        }
    }

    let db = Database::open(&default_db_path())?;
    let st = db.stats();
    println!(
        "✔ SQLite Database:      {} ({:.1} KB, schema v{}, {} samples, {} app buckets, daemon active={})",
        db.path.display(),
        (st.file_size_bytes as f64) / 1024.0,
        db.schema_version(),
        st.battery_rows,
        st.app_rows,
        service::is_daemon_active(&db)
    );

    Ok(())
}

/// Fits `(baseline_w, w_per_core)` via ordinary least squares over `(cores_busy, total_power_w)` points (`N-2`).
pub fn fit_power_model(points: &[(f64, f64)], display_w: f64) -> Result<(f64, f64)> {
    if points.len() < 2 {
        anyhow::bail!("Calibration requires at least 2 measurement phases");
    }
    let n = points.len() as f64;
    let ys: Vec<f64> = points.iter().map(|(_, p)| p - display_w).collect();
    let mean_x = points.iter().map(|(c, _)| *c).sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let mut var_x = 0.0;
    let mut cov_xy = 0.0;
    for ((x, _), y) in points.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        let dy = y - mean_y;
        var_x += dx * dx;
        cov_xy += dx * dy;
    }
    if var_x < 1e-4 {
        anyhow::bail!("CPU load did not vary enough across calibration phases");
    }
    let slope = cov_xy / var_x;
    if slope <= 0.1 {
        anyhow::bail!("Measured power did not rise with CPU load; calibration aborted");
    }
    let intercept = mean_y - slope * mean_x;
    Ok((intercept.clamp(0.3, 15.0), slope.clamp(1.0, 45.0)))
}

struct CalibrationLoadGuard {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Drop for CalibrationLoadGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn measure_calibration_phase(
    label: &str,
    secs: u64,
    load_threads: usize,
    estimator: &mut service::RollingEnergyEstimator,
) -> Result<(f64, f64, Option<f64>)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    println!("  ▸ Phase `{label}` ({secs}s, {load_threads} worker thread(s))...");
    let stop = Arc::new(AtomicBool::new(false));
    let mut guard = CalibrationLoadGuard {
        stop: Arc::clone(&stop),
        handles: Vec::with_capacity(load_threads),
    };
    for _ in 0..load_threads {
        let s = Arc::clone(&stop);
        guard.handles.push(thread::spawn(move || {
            let mut acc = 1.0001_f64;
            while !s.load(Ordering::Relaxed) {
                for _ in 0..50_000 {
                    acc = std::hint::black_box(acc.mul_add(1.0000001, 0.0000003));
                    if acc > 1e9 {
                        acc = 1.0001;
                    }
                }
            }
        }));
    }

    let start_snap = read_battery_snapshot()?;
    if start_snap.ac_online || !start_snap.status.is_discharging() {
        anyhow::bail!("AC adapter connected or battery stopped discharging during calibration");
    }
    let start_rapl_uj = crate::battery::read_rapl_package_energy_uj();
    let mut tracker = AppEnergyTracker::with_interval(secs);
    let mut prev_wh = start_snap.energy_now_wh;
    let mut min_step_wh: Option<f64> = None;
    let mut power_now_samples = Vec::new();
    let mut rolling_samples = Vec::new();
    let mut last_snap = start_snap.clone();

    for _ in 0..secs {
        thread::sleep(Duration::from_secs(1));
        if service::is_stop_requested() {
            anyhow::bail!("Calibration interrupted by signal");
        }
        let snap = read_battery_snapshot()?;
        if snap.ac_online || !snap.status.is_discharging() {
            anyhow::bail!("AC adapter connected or battery stopped discharging during calibration");
        }
        let step = (prev_wh - snap.energy_now_wh).abs();
        if step > 1e-5 {
            min_step_wh = Some(match min_step_wh {
                Some(cur) => cur.min(step),
                None => step,
            });
        }
        prev_wh = snap.energy_now_wh;
        let rolled = estimator.observe(snap.timestamp, snap.energy_now_wh, snap.power_w, true);
        if rolled > 0.1 {
            rolling_samples.push(rolled);
        }
        if snap.power_w > 0.1 {
            power_now_samples.push(snap.power_w);
        }
        last_snap = snap;
    }

    // Stop worker threads immediately before sampling final deltas
    drop(guard);
    let end_rapl_uj = crate::battery::read_rapl_package_energy_uj();

    let dt = (last_snap.timestamp - start_snap.timestamp).max(1) as f64;
    let de = (start_snap.energy_now_wh - last_snap.energy_now_wh).max(0.0);
    let rapl_w = match (start_rapl_uj, end_rapl_uj) {
        (Some(start), Some(end)) if end >= start && dt >= 1.0 => {
            let w = ((end - start) as f64 / 1_000_000.0) / dt;
            (0.05..100.0).contains(&w).then_some(w)
        }
        _ => None,
    };
    let gauge_step = min_step_wh.unwrap_or(0.01);
    let mean_power_now = if power_now_samples.is_empty() {
        None
    } else {
        Some(power_now_samples.iter().sum::<f64>() / power_now_samples.len() as f64)
    };

    let phase_power_w = if de >= 3.0 * gauge_step {
        let p_energy = (de * 3600.0) / dt;
        match mean_power_now {
            Some(p_now) => 0.5 * p_now + 0.5 * p_energy,
            None => p_energy,
        }
    } else if let Some(p_now) = mean_power_now {
        p_now
    } else if let Some(&rolled_last) = rolling_samples.last() {
        rolled_last
    } else {
        anyhow::bail!(
            "Insufficient battery telemetry during '{label}' phase (no power_now readings and energy drop {de:.4} Wh < 3x gauge step {gauge_step:.4} Wh). Try longer --idle-secs / --load-secs."
        );
    };

    let (deltas, tracker_dt) = tracker.sample_delta(phase_power_w, last_snap.energy_full_wh, true);
    let total_cpu_secs: f64 = deltas.iter().map(|d| d.cpu_secs).sum();
    let cores_busy = (total_cpu_secs / tracker_dt.max(0.5)).max(0.01);
    println!(
        "    → {:.2} W @ {:.2} core(s) busy (ΔE={:.3} Wh, dt={:.0}s{})",
        phase_power_w,
        cores_busy,
        de,
        dt,
        rapl_w.map(|r| format!(", RAPL={r:.2} W")).unwrap_or_default()
    );
    Ok((cores_busy, phase_power_w, rapl_w))
}

fn run_calibrate(idle_secs: u64, load_secs: u64, reset: bool) -> Result<()> {
    if reset {
        let mut cfg = load_autopilot_config();
        cfg.calibrated_baseline_w = 1.0;
        cfg.calibrated_cpu_w_per_core = 12.0;
        cfg.calibrated_rapl_idle_w = 0.0;
        cfg.calibrated_at = None;
        save_autopilot_config(&cfg)?;
        println!(
            "✔ Reset calibration parameters to defaults (baseline=1.00 W, cpu=12.00 W/core, rapl_idle=0.00 W) in {}",
            config_file_path().display()
        );
        return Ok(());
    }

    service::install_signal_handlers();
    let bat = read_battery_snapshot()?;
    if bat.ac_online || !bat.status.is_discharging() {
        anyhow::bail!(
            "Calibration requires running on battery (discharging). Unplug AC power, keep screen brightness steady, and re-run `battery calibrate`."
        );
    }

    let idle_dur = idle_secs.clamp(10, 120);
    let load_dur = load_secs.clamp(10, 120);
    let all_cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16);
    let disp_w = crate::battery::estimate_display_power_w();

    println!("🔬 RustyBattery — 3-Phase Hardware Power Model Calibration");
    println!("──────────────────────────────────────────────────────────────────────");
    println!("Please keep your screen brightness steady and avoid heavy background apps.");
    println!(
        "Phases: Idle ({idle_dur}s) → 1-Core Load ({load_dur}s) → {all_cores}-Core Load ({load_dur}s)"
    );
    println!();

    let mut estimator = service::RollingEnergyEstimator::new();
    let (c1, w1, idle_rapl_w) = measure_calibration_phase("idle", idle_dur, 0, &mut estimator)?;
    let (c2, w2, _) = measure_calibration_phase("1-core", load_dur, 1, &mut estimator)?;
    let (c3, w3, _) = measure_calibration_phase("all-core", load_dur, all_cores, &mut estimator)?;

    let (baseline_w, w_per_core) = fit_power_model(&[(c1, w1), (c2, w2), (c3, w3)], disp_w)?;

    let mut cfg = load_autopilot_config();
    cfg.calibrated_baseline_w = (baseline_w * 100.0).round() / 100.0;
    cfg.calibrated_cpu_w_per_core = (w_per_core * 100.0).round() / 100.0;
    if let Some(rw) = idle_rapl_w {
        cfg.calibrated_rapl_idle_w = (rw.clamp(0.0, 15.0) * 100.0).round() / 100.0;
    }
    cfg.calibrated_at = Some(chrono::Utc::now().timestamp());
    save_autopilot_config(&cfg)?;

    println!();
    println!("  • Estimated Display Draw:    {:.2} W", disp_w);
    println!(
        "  ✔ Saved calibrated_baseline_w = {:.2} W, calibrated_cpu_w_per_core = {:.2} W/core, calibrated_rapl_idle_w = {:.2} W to {}",
        cfg.calibrated_baseline_w,
        cfg.calibrated_cpu_w_per_core,
        cfg.calibrated_rapl_idle_w,
        config_file_path().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_power_model_recovers_linear_baseline_and_watts_per_core() {
        // True model: display = 2.0 W, baseline = 1.5 W, slope = 7.5 W/core
        let points = vec![
            (0.1, 2.0 + 1.5 + 0.1 * 7.5),
            (1.0, 2.0 + 1.5 + 1.0 * 7.5),
            (4.0, 2.0 + 1.5 + 4.0 * 7.5),
        ];
        let (baseline, slope) = fit_power_model(&points, 2.0).unwrap();
        assert!((baseline - 1.5).abs() < 1e-6);
        assert!((slope - 7.5).abs() < 1e-6);
    }

    #[test]
    fn fit_power_model_rejects_flat_or_negative_slope() {
        assert!(fit_power_model(&[(0.2, 8.0), (0.2, 8.2)], 2.0).is_err());
        assert!(fit_power_model(&[(0.2, 10.0), (2.0, 9.0)], 2.0).is_err());
    }
}
