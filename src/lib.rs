pub mod apps;
pub mod autopilot;
pub mod battery;
pub mod db;
pub mod optimize;
pub mod service;
pub mod statusbar;
pub mod tui;

use crate::apps::AppEnergyTracker;
use crate::autopilot::{
    config_file_path, get_autopilot_log, get_last_autopilot_action, is_app_whitelisted,
    is_autopilot_enabled, load_autopilot_config, save_autopilot_config, set_autopilot_enabled,
};
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, stop_or_kill_app,
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "rustybattery",
    version = "0.8.0",
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
    },

    /// Freeze (0% CPU Deep Sleep) a running user application without closing its windows
    Freeze {
        /// Application name (e.g., "Waterfox", "Obsidian")
        app: String,
    },

    /// Unfreeze / resume a frozen application
    Resume {
        /// Application name (e.g., "Waterfox", "Obsidian")
        app: String,
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
        Commands::Optimize { dry_run } => run_cli_optimize(dry_run),
        Commands::Freeze { app } => {
            let msg = set_app_frozen(&app, true)?;
            println!("{msg}");
            Ok(())
        }
        Commands::Resume { app } => {
            let msg = set_app_frozen(&app, false)?;
            println!("{msg}");
            Ok(())
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
                Ok(())
            }
        },
        Commands::Autopilot { mode, target } => {
            let db = Database::open(&default_db_path())?;
            match mode.to_lowercase().as_str() {
                "on" | "enable" | "true" | "1" => {
                    let msg = set_autopilot_enabled(&db, true)?;
                    let mut cfg = load_autopilot_config();
                    cfg.enabled = true;
                    let _ = save_autopilot_config(&cfg);
                    println!("✔ {msg}");
                }
                "off" | "disable" | "false" | "0" => {
                    let msg = set_autopilot_enabled(&db, false)?;
                    let mut cfg = load_autopilot_config();
                    cfg.enabled = false;
                    let _ = save_autopilot_config(&cfg);
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
                    let cfg = load_autopilot_config();
                    save_autopilot_config(&cfg)?;
                    println!("Config Path: {}", path.display());
                    if let Ok(raw) = std::fs::read_to_string(&path) {
                        println!("\n{raw}");
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
    }
}

fn print_summary(hours: u64, top: usize, json: bool) -> Result<()> {
    let db = Database::open(&default_db_path())?;
    let bat = read_battery_snapshot()?;
    db.ensure_initial_seed(&bat)?;
    db.record_battery_snapshot(&bat)?;

    let mut tracker = AppEnergyTracker::new();
    thread::sleep(Duration::from_millis(250));
    let (live_deltas, _) =
        tracker.sample_delta(bat.power_w, bat.energy_full_wh, bat.status.is_discharging());
    if !service::is_daemon_active(&db) {
        let _ = db.record_app_deltas(&live_deltas);
    }

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
        "│ 🦀 RustyBattery v0.8.0 — Per-App Power Tracker for Linux                               │"
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

fn run_cli_optimize(dry_run: bool) -> Result<()> {
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

    let total_watts: f64 = targets.iter().map(|t| t.estimated_watts).sum();
    let total_ram: f64 = targets.iter().map(|t| t.memory_mb).sum();
    let total_pids: u32 = targets.iter().map(|t| t.pids_count).sum();

    println!("  Detected {} background battery hog(s):", targets.len());
    println!("  ──────────────────────────────────────────────────────────────────────────────");
    for (i, t) in targets.iter().enumerate() {
        if t.memory_mb > 0.0 {
            println!(
                "  {:>2}. {:<36} ~{:>4.2} W   {:>5.0} MB RAM   ({:>3} threads)",
                i + 1,
                t.display_name,
                t.estimated_watts,
                t.memory_mb,
                t.pids_count
            );
        } else {
            println!(
                "  {:>2}. {:<36} ~{:>4.2} W   (System Power Profile)",
                i + 1,
                t.display_name,
                t.estimated_watts
            );
        }
        println!("      └─ {}", t.reason);
    }
    println!();
    println!(
        "  Potential Savings: ~{:.2} W power reduction  •  {:.0} MB RAM  •  {} background threads",
        total_watts, total_ram, total_pids
    );

    if dry_run {
        println!("\n  (Dry-run mode: no services were stopped. Run `battery optimize` to apply.)");
        return Ok(());
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
        let new_power = (bat.power_w - report.estimated_watts_saved).max(4.5);
        let old_mins = if bat.power_w > 0.5 {
            (bat.energy_now_wh / bat.power_w) * 60.0
        } else {
            0.0
        };
        let new_mins = (bat.energy_now_wh / new_power) * 60.0;
        let gained_mins = (new_mins - old_mins).max(0.0).round() as u32;

        println!(
            "  🎉 Done! Freed {:.0} MB RAM, stopped {} threads, saved ~{:.1} W (est. +{}m extra battery time).",
            report.freed_memory_mb,
            report.stopped_pids,
            report.estimated_watts_saved,
            gained_mins
        );
    }

    Ok(())
}
