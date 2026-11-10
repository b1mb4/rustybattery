pub mod apps;
pub mod battery;
pub mod db;
pub mod service;
pub mod tui;

use crate::apps::AppEnergyTracker;
use crate::battery::read_battery_snapshot;
use crate::db::{default_db_path, Database};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "rustybattery",
    version = "0.1.0",
    about = "Phone-style laptop battery usage tracker & per-app energy analyzer for Fedora Linux"
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
        "│ 🦀 RustyBattery — Fedora Laptop Battery & Per-App Power Report               │"
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
    println!("  Tip: Run `rustybattery` to launch the interactive TUI, or `rustybattery service install` for 24/7 tracking.");

    Ok(())
}
