use crate::battery::{read_battery_snapshot, BatterySnapshot};
use crate::db::{BatteryHealthPoint, Database, PeriodDrainStats};
use anyhow::Result;
use chrono::{Local, TimeZone};
use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct WakeupSourceEntry {
    pub name: String,
    pub active_count: u64,
    pub event_count: u64,
    pub wakeup_count: u64,
    pub active_time_ms: u64,
    pub total_time_ms: u64,
}

/// Parses `/sys/power/wakeup_sources` (or `/sys/kernel/debug/wakeup_sources` when readable)
/// and returns top active wake sources sorted by `total_time_ms` descending.
pub fn parse_wakeup_sources_str(content: &str) -> Vec<WakeupSourceEntry> {
    let mut entries = Vec::new();
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').map(|s| s.trim()).collect();
        if cols.len() >= 7 {
            let name = cols[0].to_string();
            let active_count = cols[1].parse::<u64>().unwrap_or(0);
            let event_count = cols[2].parse::<u64>().unwrap_or(0);
            let wakeup_count = cols[3].parse::<u64>().unwrap_or(0);
            let active_time_ms = cols[5].parse::<u64>().unwrap_or(0);
            let total_time_ms = cols[6].parse::<u64>().unwrap_or(0);
            if !name.is_empty() && (active_count > 0 || total_time_ms > 0) {
                entries.push(WakeupSourceEntry {
                    name,
                    active_count,
                    event_count,
                    wakeup_count,
                    active_time_ms,
                    total_time_ms,
                });
            }
        }
    }
    entries.sort_by(|a, b| {
        b.total_time_ms
            .cmp(&a.total_time_ms)
            .then_with(|| b.active_count.cmp(&a.active_count))
    });
    entries
}

pub fn read_wakeup_sources() -> Result<(String, Vec<WakeupSourceEntry>)> {
    for path in [
        "/sys/power/wakeup_sources",
        "/sys/kernel/debug/wakeup_sources",
    ] {
        if let Ok(content) = fs::read_to_string(path) {
            return Ok((path.to_string(), parse_wakeup_sources_str(&content)));
        }
    }
    anyhow::bail!(
        "Cannot read /sys/power/wakeup_sources or /sys/kernel/debug/wakeup_sources (may require root on this kernel)"
    )
}

pub fn print_wakeup_report(top: usize) -> Result<()> {
    println!("⏰ RustyBattery — Kernel Wakeup Sources & Charge-Limit Helper");
    println!("──────────────────────────────────────────────────────────────────────");
    match read_wakeup_sources() {
        Ok((src, entries)) => {
            println!("Source: {src} ({} active wakeup source(s))", entries.len());
            println!();
            println!(
                "  {:<3} {:<30} {:>10} {:>10} {:>12}",
                "#", "Wakeup Source", "Activations", "Events", "Total Time"
            );
            println!("  ────────────────────────────────────────────────────────────────────");
            for (i, e) in entries.iter().take(top.max(1)).enumerate() {
                let secs = e.total_time_ms as f64 / 1000.0;
                println!(
                    "  {:>2}. {:<30} {:>10} {:>10} {:>10.1} s",
                    i + 1,
                    e.name.chars().take(30).collect::<String>(),
                    e.active_count,
                    e.event_count,
                    secs
                );
            }
        }
        Err(e) => {
            println!("⚠ {e}");
            println!("  Tip: Run `sudo cat /sys/kernel/debug/wakeup_sources | head -n 25` to inspect manually.");
        }
    }

    println!();
    print_charge_limit_helper();
    Ok(())
}

pub fn print_charge_limit_helper() {
    println!("🔋 Battery Charge-Limit Status (80% Longevity Helper)");
    println!("──────────────────────────────────────────────────────────────────────");
    let dirs = crate::battery::find_battery_dirs();
    let mut found_sysfs = false;
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "BAT0".to_string());
        let thresh = dir.join("charge_control_end_threshold");
        if thresh.exists() {
            found_sysfs = true;
            let cur = fs::read_to_string(&thresh)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            println!("  • {name}: charge_control_end_threshold = {cur}% ({})", thresh.display());
            println!("  • Set 80% now:  sudo sh -c 'echo 80 > {}'", thresh.display());
            println!(
                "  • Persist via udev (/etc/udev/rules.d/99-battery-charge-limit.rules):\n    SUBSYSTEM==\"power_supply\", ATTR{{charge_control_end_threshold}}==\"*\", ATTR{{charge_control_end_threshold}}=\"80\""
            );
        }
    }
    if !found_sysfs {
        let base = Path::new("/sys/class/power_supply");
        println!(
            "  ℹ No `charge_control_end_threshold` exposed under {} on this hardware/driver.",
            base.display()
        );
    }
}

pub fn print_report(db: &Database, days: u64, format: &str, top: usize) -> Result<()> {
    let days_i = days.clamp(1, 90) as i64;
    let since_ts = chrono::Utc::now().timestamp() - days_i * 86_400;
    let bat = read_battery_snapshot()?;
    let drain: PeriodDrainStats = db.get_period_drain_stats(since_ts).unwrap_or_default();
    let mut apps = db.get_app_breakdown(since_ts, &[]).unwrap_or_default();
    apps.sort_by(|a, b| {
        b.battery_pct
            .partial_cmp(&a.battery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let health: Vec<BatteryHealthPoint> = db.get_health_history(days_i);
    let health_delta = match (health.first(), health.last()) {
        (Some(first), Some(last)) => last.health_pct - first.health_pct,
        _ => 0.0,
    };

    if format.eq_ignore_ascii_case("json") {
        let payload = serde_json::json!({
            "period_days": days_i,
            "generated_at": chrono::Utc::now().timestamp(),
            "battery": bat,
            "drain_stats": drain,
            "health_trend": {
                "samples": health,
                "delta_pct": health_delta,
            },
            "top_apps": apps.into_iter().take(top).map(|a| serde_json::json!({
                "app_name": a.app_name,
                "category": a.category,
                "battery_pct": a.battery_pct,
                "energy_wh": a.energy_wh,
                "share_of_drain_pct": a.share_of_drain_pct,
                "confidence": a.confidence,
            })).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let sot_h = drain.sot_secs / 3600;
    let sot_m = (drain.sot_secs % 3600) / 60;
    let sleep_h = drain.sleep_secs / 3600;
    let sleep_m = (drain.sleep_secs % 3600) / 60;

    println!("# 🦀 RustyBattery {days_i}-Day Battery Report");
    println!();
    println!("*Generated: {now_str} · Device: {} ({} {})*", bat.device_name, bat.manufacturer, bat.model_name);
    println!();
    println!("## Summary");
    println!("- **Current Battery:** {:.1}% ({}) — {:.2} W", bat.capacity_pct, bat.status.as_str(), bat.power_w);
    println!(
        "- **Battery Health:** {:.1}% ({:.1} / {:.1} Wh design, {} cycles, {days_i}d trend: {:+.2}%)",
        bat.health_pct, bat.energy_full_wh, bat.energy_design_wh, bat.cycle_count, health_delta
    );
    println!(
        "- **Screen-On Time (on battery):** {}h {:02}m ({:.1}% drained)",
        sot_h, sot_m, drain.sot_drain_pct
    );
    println!(
        "- **Sleep / Suspend:** {}h {:02}m across {} event(s) ({:.1}% lost · {:.2}%/hr)",
        sleep_h, sleep_m, drain.sleep_events, drain.sleep_drain_pct, drain.sleep_drain_pct_per_hr
    );
    println!();
    println!("## Top {top} Battery Consumers ({days_i}d)");
    println!();
    println!("| # | Application | Category | Share | Battery % | Energy (Wh) | Confidence |");
    println!("|---:|---|---|---:|---:|---:|---|");
    for (i, a) in apps.iter().take(top).enumerate() {
        println!(
            "| {} | {} | {} | {:.1}% | {:.2}% | {:.2} | {} |",
            i + 1,
            a.app_name,
            a.category,
            a.share_of_drain_pct,
            a.battery_pct,
            a.energy_wh,
            a.confidence
        );
    }

    if !health.is_empty() {
        println!();
        println!("## Battery Health History");
        println!();
        println!("| Date | Full Capacity (Wh) | Health % | Cycles |");
        println!("|---|---:|---:|---:|");
        for h in health.iter().rev().take(10).rev() {
            let dt = Local
                .timestamp_opt(h.day_ts, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| h.day_ts.to_string());
            println!(
                "| {} | {:.2} | {:.1}% | {} |",
                dt, h.energy_full_wh, h.health_pct, h.cycle_count
            );
        }
    }

    Ok(())
}

pub fn print_export(db: &Database, format: &str, hours: u64) -> Result<()> {
    let since_ts = if hours == 0 {
        db.get_last_full_charge_ts()
    } else {
        chrono::Utc::now().timestamp() - (hours as i64) * 3600
    };

    if format.eq_ignore_ascii_case("prometheus") || format.eq_ignore_ascii_case("prom") {
        let bat: BatterySnapshot = read_battery_snapshot()?;
        let drain = db.get_period_drain_stats(since_ts).unwrap_or_default();
        let apps = db.get_app_breakdown(since_ts, &[]).unwrap_or_default();
        println!("# HELP rustybattery_capacity_percent Current battery charge percentage.");
        println!("# TYPE rustybattery_capacity_percent gauge");
        println!("rustybattery_capacity_percent{{device=\"{}\"}} {:.2}", bat.device_name, bat.capacity_pct);
        println!("# HELP rustybattery_power_watts Current battery power draw or charge rate in Watts.");
        println!("# TYPE rustybattery_power_watts gauge");
        println!("rustybattery_power_watts{{device=\"{}\",status=\"{}\"}} {:.3}", bat.device_name, bat.status.as_str(), bat.power_w);
        println!("# HELP rustybattery_health_percent Battery health percentage relative to design capacity.");
        println!("# TYPE rustybattery_health_percent gauge");
        println!("rustybattery_health_percent{{device=\"{}\"}} {:.2}", bat.device_name, bat.health_pct);
        println!("# HELP rustybattery_energy_now_wh Current stored energy in Watt-hours.");
        println!("# TYPE rustybattery_energy_now_wh gauge");
        println!("rustybattery_energy_now_wh{{device=\"{}\"}} {:.3}", bat.device_name, bat.energy_now_wh);
        println!("# HELP rustybattery_sleep_drain_percent_per_hour Sleep drain rate in percent per hour.");
        println!("# TYPE rustybattery_sleep_drain_percent_per_hour gauge");
        println!("rustybattery_sleep_drain_percent_per_hour {:.3}", drain.sleep_drain_pct_per_hour());
        println!("# HELP rustybattery_app_energy_wh Per-application attributed energy in Watt-hours.");
        println!("# TYPE rustybattery_app_energy_wh gauge");
        for a in apps.iter().take(20) {
            let safe_app = a.app_name.replace('"', "'");
            let safe_cat = a.category.replace('"', "'");
            println!(
                "rustybattery_app_energy_wh{{app=\"{safe_app}\",category=\"{safe_cat}\"}} {:.4}",
                a.energy_wh
            );
        }
        return Ok(());
    }

    // Default: CSV export of battery history
    let history = db.get_battery_history(since_ts)?;
    println!("timestamp,capacity_pct,status,power_w");
    for pt in history {
        println!(
            "{},{:.2},{},{:.3}",
            pt.timestamp,
            pt.capacity_pct,
            pt.status.replace(',', ";"),
            pt.power_w
        );
    }
    Ok(())
}

trait DrainRateExt {
    fn sleep_drain_pct_per_hour(&self) -> f64;
}

impl DrainRateExt for PeriodDrainStats {
    fn sleep_drain_pct_per_hour(&self) -> f64 {
        self.sleep_drain_pct_per_hr
    }
}

pub fn generate_shell_completions(shell: &str) -> Result<String> {
    let s = shell.to_lowercase();
    let script = match s.as_str() {
        "bash" => r#"_rustybattery_completions() {
    local cur prev cmds
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    cmds="tui summary optimize freeze resume stop statusbar notify daemon service autopilot sleep doctor calibrate report export wakeups completions man"
    if [[ ${COMP_CWORD} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "${cmds}" -- "${cur}") )
        return 0
    fi
    case "${prev}" in
        service) COMPREPLY=( $(compgen -W "install uninstall status prune reset-stats" -- "${cur}") ) ;;
        autopilot) COMPREPLY=( $(compgen -W "status on off whitelist unwhitelist config" -- "${cur}") ) ;;
        completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") ) ;;
        export) COMPREPLY=( $(compgen -W "--format --hours" -- "${cur}") ) ;;
        report) COMPREPLY=( $(compgen -W "--days --format --top" -- "${cur}") ) ;;
    esac
}
complete -F _rustybattery_completions battery rustybattery
"#,
        "zsh" => r#"#compdef battery rustybattery
_rustybattery() {
    local -a commands
    commands=(
        'tui:Launch the interactive Terminal UI'
        'summary:Print phone-style battery summary'
        'optimize:Scan and stop unused background hogs'
        'freeze:Freeze an app to 0% CPU'
        'resume:Resume a frozen app (--all / --force-all)'
        'stop:Stop or kill an application'
        'statusbar:Output Waybar/Polybar/Tmux JSON or text'
        'notify:Send desktop battery notification'
        'daemon:Run the background collector daemon'
        'service:Manage the systemd user service'
        'autopilot:Configure Smart Battery Saver rules'
        'sleep:Audit or fix s2idle sleep drain'
        'doctor:Check system capabilities and health'
        'calibrate:Calibrate baseline and CPU W/core model'
        'report:Generate daily/weekly Markdown or JSON report'
        'export:Export CSV or Prometheus metrics'
        'wakeups:Inspect kernel wakeup sources and charge limit'
        'completions:Generate shell completions'
        'man:View or install manual pages'
    )
    _describe 'command' commands
}
compdef _rustybattery battery rustybattery
"#,
        "fish" => r#"complete -c battery -f
complete -c rustybattery -f
for cmd in battery rustybattery
    complete -c $cmd -n '__fish_use_subcommand' -a 'tui' -d 'Launch interactive TUI'
    complete -c $cmd -n '__fish_use_subcommand' -a 'summary' -d 'Print battery summary'
    complete -c $cmd -n '__fish_use_subcommand' -a 'optimize' -d 'Optimize background power hogs'
    complete -c $cmd -n '__fish_use_subcommand' -a 'freeze' -d 'Freeze an app to 0% CPU'
    complete -c $cmd -n '__fish_use_subcommand' -a 'resume' -d 'Resume frozen app(s)'
    complete -c $cmd -n '__fish_use_subcommand' -a 'stop' -d 'Stop an application'
    complete -c $cmd -n '__fish_use_subcommand' -a 'statusbar' -d 'Status bar output'
    complete -c $cmd -n '__fish_use_subcommand' -a 'notify' -d 'Send desktop notification'
    complete -c $cmd -n '__fish_use_subcommand' -a 'service' -d 'Manage systemd user service'
    complete -c $cmd -n '__fish_use_subcommand' -a 'autopilot' -d 'Smart Battery Saver rules'
    complete -c $cmd -n '__fish_use_subcommand' -a 'sleep' -d 'Audit or fix s2idle sleep drain'
    complete -c $cmd -n '__fish_use_subcommand' -a 'doctor' -d 'Check system capabilities'
    complete -c $cmd -n '__fish_use_subcommand' -a 'calibrate' -d 'Calibrate power model'
    complete -c $cmd -n '__fish_use_subcommand' -a 'report' -d 'Generate Markdown/JSON report'
    complete -c $cmd -n '__fish_use_subcommand' -a 'export' -d 'Export CSV or Prometheus metrics'
    complete -c $cmd -n '__fish_use_subcommand' -a 'wakeups' -d 'Kernel wakeup sources & charge limit'
    complete -c $cmd -n '__fish_use_subcommand' -a 'completions' -d 'Generate shell completions'
    complete -c $cmd -n '__fish_use_subcommand' -a 'man' -d 'View or install man page'
end
"#,
        _ => anyhow::bail!("Unsupported shell `{shell}`. Supported shells: bash, zsh, fish"),
    };
    Ok(script.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wakeup_sources_and_sorts_by_total_time() {
        let sample = "name\tactive_count\tevent_count\twakeup_count\texpire_count\tactive_since\ttotal_time\tmax_time\tlast_change\tprevent_suspend_time\n\
                      iwlwifi\t120\t120\t5\t0\t0\t4500\t120\t1000\t0\n\
                      xhci_hcd\t50\t50\t2\t0\t0\t12000\t300\t1000\t0\n\
                      idle_dev\t0\t0\t0\t0\t0\t0\t0\t0\t0\n";
        let entries = parse_wakeup_sources_str(sample);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "xhci_hcd");
        assert_eq!(entries[0].total_time_ms, 12000);
        assert_eq!(entries[1].name, "iwlwifi");
    }

    #[test]
    fn generates_completions_for_bash_zsh_fish() {
        assert!(generate_shell_completions("bash").unwrap().contains("complete -F"));
        assert!(generate_shell_completions("zsh").unwrap().contains("#compdef"));
        assert!(generate_shell_completions("fish").unwrap().contains("complete -c battery"));
        assert!(generate_shell_completions("powershell").is_err());
    }
}
