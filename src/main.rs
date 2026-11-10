pub mod apps;
pub mod battery;
pub mod db;
pub mod optimize;
pub mod service;
pub mod tui;

use crate::apps::AppEnergyTracker;
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
    version = "0.2.0",
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
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Install and enable the systemd user service (~/.config/systemd/user/battstat.service)
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

fn main() -> Result<()> {
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
                println!("Systemd Unit:    {} ({})", unit_path.display(), if installed { "installed" } else { "not installed" });
                println!("Daemon Active:   {}", if active { "YES (running)" } else { "NO (idle)" });
                Ok(())
            }
        },
    }
}

fn print_summary(hours: u64, top: usize, json: bool) -> Result<()> {
    let db = Database::open(&default_db_path())?;
    let bat = read_battery_snapshot()?;
    db.ensure_initial_seed(&bat)?;
    db.record_battery_snapshot(&bat)?;

    // Take a quick 250ms live delta sample so live_power_w and live_cpu_pct are populated
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
            "apps": apps.into_iter().take(top).map(|a| serde_json::json!({
                "app_name": a.app_name,
                "category": a.category,
                "battery_pct": a.battery_pct,
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

    println!("╭──────────────────────────────────────────────────────────────────────────────╮");
    println!(
        "│ 🦀 RustyBattery — Per-App Power Tracker for Linux                            │"
    );
    println!("├──────────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│ Battery: {:>5.1}% ({})   Power: {:>5.2} W   Remaining: {:<10}       │",
        bat.capacity_pct,
        bat.status.as_str(),
        bat.power_w,
        time_rem
    );
    println!(
        "│ Energy:  {:>5.1} / {:>5.1} Wh (Design: {:>5.1} Wh)   Health: {:>5.1}% ({} cycles) │",
        bat.energy_now_wh,
        bat.energy_full_wh,
        bat.energy_design_wh,
        bat.health_pct,
        bat.cycle_count
    );
    println!("╰──────────────────────────────────────────────────────────────────────────────╯");
    println!();
    println!(
        "  {:<3} {:<24} {:<13} {:<16} {:>7} {:>8} {:>7} {:>8}",
        "#", "Application", "Category", "Share of Drain", "Batt %", "Energy", "Live W", "Memory"
    );
    println!("  ──────────────────────────────────────────────────────────────────────────────────────────");

    for (i, app) in apps.into_iter().take(top).enumerate() {
        let filled = ((app.share_of_drain_pct / 100.0) * 10.0).round() as usize;
        let bar = format!(
            "{}{}",
            "█".repeat(filled.min(10)),
            "░".repeat(10usize.saturating_sub(filled.min(10)))
        );
        println!(
            "  {:>2}. {:<24} {:<13} {} {:>4.0}% {:>6.2}% {:>5.2} Wh {:>5.2} W {:>6.0} MB",
            i + 1,
            app.app_name.chars().take(24).collect::<String>(),
            app.category.chars().take(13).collect::<String>(),
            bar,
            app.share_of_drain_pct,
            app.battery_pct,
            app.energy_wh,
            app.live_power_w,
            app.memory_mb
        );
    }
    println!();
    println!("  Tip: Run `battery` for the interactive TUI, or `battery optimize` to stop unused background hogs.");

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
