use crate::battery::{read_battery_snapshot, BatterySnapshot};
use crate::db::{BatteryHealthPoint, Database, PeriodDrainStats};
use anyhow::Result;
use chrono::{Local, TimeZone};
use serde::Serialize;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ReportFormat {
    Markdown,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    Csv,
    Prometheus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

/// Escapes label values according to the Prometheus text exposition format.
pub fn escape_prometheus_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Escapes cell text for Markdown tables.
pub fn escape_markdown_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// Escapes a CSV field according to RFC 4180.
pub fn escape_csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Writes `content` to `path` atomically via a temporary sibling file with `0700`/`0600` permissions.
pub fn write_atomic_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
            crate::paths::set_private_dir_mode(parent);
        }
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let tmp_name = format!(".{file_name}.tmp.{}", std::process::id());
    let tmp_path = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(tmp_name),
        _ => Path::new(&tmp_name).to_path_buf(),
    };
    fs::write(&tmp_path, content)?;
    crate::paths::set_private_file_mode(&tmp_path);
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

fn emit_output(output: Option<&Path>, text: &str) -> Result<()> {
    if let Some(path) = output {
        write_atomic_text(path, text)?;
        println!("✅ Wrote {}", path.display());
    } else {
        print!("{text}");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct WakeupSourceEntry {
    pub name: String,
    pub active_count: u64,
    pub event_count: u64,
    pub wakeup_count: u64,
    pub total_time_ms: u64,
}

/// Parses `/sys/kernel/debug/wakeup_sources` (where Linux prints `%-12s\t%lu\t\t%lu...` with
/// double tabs between numeric columns) and returns active sources sorted by `total_time_ms` desc (`N-3`).
pub fn parse_wakeup_sources_str(content: &str) -> Vec<WakeupSourceEntry> {
    let mut entries = Vec::new();
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line
            .split('\t')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if cols.len() >= 7 {
            let Ok(active_count) = cols[1].parse::<u64>() else {
                continue;
            };
            let name = cols[0].to_string();
            let event_count = cols[2].parse::<u64>().unwrap_or(0);
            let wakeup_count = cols[3].parse::<u64>().unwrap_or(0);
            let total_time_ms = cols[6].parse::<u64>().unwrap_or(0);
            if !name.is_empty() && (active_count > 0 || total_time_ms > 0) {
                entries.push(WakeupSourceEntry {
                    name,
                    active_count,
                    event_count,
                    wakeup_count,
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

fn read_wakeup_u64(dir: &Path, file: &str) -> u64 {
    fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Reads the world-readable `/sys/class/wakeup/wakeup*` sysfs hierarchy (`N-3`).
pub fn read_wakeup_sources_from_sysfs(base: &Path) -> Result<Vec<WakeupSourceEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(base)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(name_raw) = fs::read_to_string(path.join("name")) else {
            continue;
        };
        let name = name_raw.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let active_count = read_wakeup_u64(&path, "active_count");
        let event_count = read_wakeup_u64(&path, "event_count");
        let wakeup_count = read_wakeup_u64(&path, "wakeup_count");
        let total_time_ms = read_wakeup_u64(&path, "total_time_ms");
        if active_count > 0 || total_time_ms > 0 {
            entries.push(WakeupSourceEntry {
                name,
                active_count,
                event_count,
                wakeup_count,
                total_time_ms,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.total_time_ms
            .cmp(&a.total_time_ms)
            .then_with(|| b.active_count.cmp(&a.active_count))
    });
    Ok(entries)
}

pub fn read_wakeup_sources() -> Result<(String, Vec<WakeupSourceEntry>)> {
    let sysfs_base = Path::new("/sys/class/wakeup");
    if let Ok(entries) = read_wakeup_sources_from_sysfs(sysfs_base) {
        return Ok(("/sys/class/wakeup".to_string(), entries));
    }
    if let Ok(content) = fs::read_to_string("/sys/kernel/debug/wakeup_sources") {
        return Ok((
            "/sys/kernel/debug/wakeup_sources".to_string(),
            parse_wakeup_sources_str(&content),
        ));
    }
    anyhow::bail!(
        "Cannot read /sys/class/wakeup or /sys/kernel/debug/wakeup_sources (may require root on this kernel)"
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
    print_charge_limit_helper_for(80);
}

pub fn print_charge_limit_helper_for(target_pct: u8) {
    let pct = target_pct.clamp(20, 100);
    println!("🔋 Battery Charge-Limit Status ({pct}% Longevity Helper)");
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
            println!(
                "  • {name}: charge_control_end_threshold = {cur}% ({})",
                thresh.display()
            );
            println!("  • Set {pct}% now:  sudo sh -c 'echo {pct} > {}'", thresh.display());
            println!(
                "  • Persist via udev (/etc/udev/rules.d/99-battery-charge-limit.rules):\n    ACTION==\"add\", SUBSYSTEM==\"power_supply\", KERNEL==\"{name}\", ATTR{{charge_control_end_threshold}}==\"*\", ATTR{{charge_control_end_threshold}}=\"{pct}\""
            );
        }
    }
    if found_sysfs {
        println!(
            "  ℹ Note: Some laptop ECs reset charge_control_end_threshold after suspend/hibernate; add a /usr/lib/systemd/system-sleep/ post-resume hook to re-apply {pct}% if reset."
        );
    } else {
        let base = Path::new("/sys/class/power_supply");
        println!(
            "  ℹ No `charge_control_end_threshold` exposed under {} on this hardware/driver.",
            base.display()
        );
    }
}

pub fn run_charge_limit(set_pct: Option<u8>) -> Result<()> {
    let Some(pct) = set_pct else {
        print_charge_limit_helper_for(80);
        return Ok(());
    };
    if !(20..=100).contains(&pct) {
        anyhow::bail!("Charge limit must be between 20 and 100 (got {pct}%).");
    }

    let dirs = crate::battery::find_battery_dirs();
    let mut found = false;
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "BAT0".to_string());
        let thresh = dir.join("charge_control_end_threshold");
        if thresh.exists() {
            found = true;
            match fs::write(&thresh, format!("{pct}\n")) {
                Ok(()) => {
                    println!("✅ Set {name} charge_control_end_threshold to {pct}%");
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    print_charge_limit_helper_for(pct);
                    anyhow::bail!(
                        "Permission denied writing {} (run with sudo or install the udev rule printed above).",
                        thresh.display()
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    if !found {
        print_charge_limit_helper_for(pct);
        anyhow::bail!("No `charge_control_end_threshold` sysfs interface found on this hardware.");
    }
    Ok(())
}

pub fn print_report(
    db: &Database,
    days: u64,
    format: ReportFormat,
    top: usize,
    output: Option<&Path>,
) -> Result<()> {
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

    if format == ReportFormat::Json {
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
                "confidence": a.confidence.as_str(),
            })).collect::<Vec<_>>()
        });
        let text = format!("{}\n", serde_json::to_string_pretty(&payload)?);
        return emit_output(output, &text);
    }

    let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let sot_h = drain.sot_secs / 3600;
    let sot_m = (drain.sot_secs % 3600) / 60;
    let sleep_h = drain.sleep_secs / 3600;
    let sleep_m = (drain.sleep_secs % 3600) / 60;

    let mut out = String::new();
    writeln!(out, "# 🦀 RustyBattery {days_i}-Day Battery Report")?;
    writeln!(out)?;
    writeln!(
        out,
        "*Generated: {now_str} · Device: {} ({} {})*",
        escape_markdown_cell(&bat.device_name),
        escape_markdown_cell(&bat.manufacturer),
        escape_markdown_cell(&bat.model_name)
    )?;
    writeln!(out)?;
    writeln!(out, "## Summary")?;
    writeln!(
        out,
        "- **Current Battery:** {:.1}% ({}) — {:.2} W",
        bat.capacity_pct,
        bat.status.as_str(),
        bat.power_w
    )?;
    writeln!(
        out,
        "- **Battery Health:** {:.1}% ({:.1} / {:.1} Wh design, {} cycles, {days_i}d trend: {:+.2}%)",
        bat.health_pct, bat.energy_full_wh, bat.energy_design_wh, bat.cycle_count, health_delta
    )?;
    writeln!(
        out,
        "- **Screen-On Time (on battery):** {}h {:02}m ({:.1}% drained)",
        sot_h, sot_m, drain.sot_drain_pct
    )?;
    writeln!(
        out,
        "- **Sleep / Suspend:** {}h {:02}m across {} event(s) ({:.1}% lost · {:.2}%/hr)",
        sleep_h, sleep_m, drain.sleep_events, drain.sleep_drain_pct, drain.sleep_drain_pct_per_hr
    )?;
    writeln!(out)?;
    writeln!(out, "## Top {top} Battery Consumers ({days_i}d)")?;
    writeln!(out)?;
    writeln!(
        out,
        "| # | Application | Category | Share | Battery % | Energy (Wh) | Confidence |"
    )?;
    writeln!(out, "|---:|---|---|---:|---:|---:|---|")?;
    for (i, a) in apps.iter().take(top).enumerate() {
        writeln!(
            out,
            "| {} | {} | {} | {:.1}% | {:.2}% | {:.2} | {} |",
            i + 1,
            escape_markdown_cell(&a.app_name),
            escape_markdown_cell(&a.category),
            a.share_of_drain_pct,
            a.battery_pct,
            a.energy_wh,
            escape_markdown_cell(a.confidence.display_label())
        )?;
    }

    if !health.is_empty() {
        writeln!(out)?;
        writeln!(out, "## Battery Health History")?;
        writeln!(out)?;
        writeln!(out, "| Date | Full Capacity (Wh) | Health % | Cycles |")?;
        writeln!(out, "|---|---:|---:|---:|")?;
        for h in health.iter().rev().take(10).rev() {
            let dt = Local
                .timestamp_opt(h.day_ts, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| h.day_ts.to_string());
            writeln!(
                out,
                "| {} | {:.2} | {:.1}% | {} |",
                dt, h.energy_full_wh, h.health_pct, h.cycle_count
            )?;
        }
    }

    emit_output(output, &out)
}

pub fn print_export(
    db: &Database,
    format: ExportFormat,
    hours: u64,
    per_app: bool,
    top: usize,
    output: Option<&Path>,
) -> Result<()> {
    let since_ts = if hours == 0 {
        db.get_last_full_charge_ts()
    } else {
        chrono::Utc::now().timestamp() - (hours as i64) * 3600
    };

    let mut out = String::new();
    let limit = if top == 0 { usize::MAX } else { top };

    if format == ExportFormat::Prometheus {
        let bat: BatterySnapshot = read_battery_snapshot()?;
        let drain = db.get_period_drain_stats(since_ts).unwrap_or_default();
        let apps = db.get_app_breakdown(since_ts, &[]).unwrap_or_default();
        let cumulative = db.get_cumulative_app_energy(limit);
        let latest_ts = db.get_latest_sample_timestamp().unwrap_or(0);
        let dev = escape_prometheus_label(&bat.device_name);
        let status = escape_prometheus_label(bat.status.as_str());

        writeln!(
            out,
            "# HELP rustybattery_up 1 if RustyBattery metrics collection succeeded."
        )?;
        writeln!(out, "# TYPE rustybattery_up gauge")?;
        writeln!(out, "rustybattery_up 1")?;
        writeln!(
            out,
            "# HELP rustybattery_last_sample_timestamp_seconds Unix timestamp of the latest stored sample."
        )?;
        writeln!(out, "# TYPE rustybattery_last_sample_timestamp_seconds gauge")?;
        writeln!(out, "rustybattery_last_sample_timestamp_seconds {latest_ts}")?;
        writeln!(
            out,
            "# HELP rustybattery_capacity_percent Current battery charge percentage."
        )?;
        writeln!(out, "# TYPE rustybattery_capacity_percent gauge")?;
        writeln!(
            out,
            "rustybattery_capacity_percent{{device=\"{dev}\"}} {:.2}",
            bat.capacity_pct
        )?;
        writeln!(
            out,
            "# HELP rustybattery_power_watts Current battery power draw or charge rate in Watts."
        )?;
        writeln!(out, "# TYPE rustybattery_power_watts gauge")?;
        writeln!(
            out,
            "rustybattery_power_watts{{device=\"{dev}\",status=\"{status}\"}} {:.3}",
            bat.power_w
        )?;
        writeln!(
            out,
            "# HELP rustybattery_health_percent Battery health percentage relative to design capacity."
        )?;
        writeln!(out, "# TYPE rustybattery_health_percent gauge")?;
        writeln!(
            out,
            "rustybattery_health_percent{{device=\"{dev}\"}} {:.2}",
            bat.health_pct
        )?;
        writeln!(
            out,
            "# HELP rustybattery_energy_now_wh Current stored energy in Watt-hours."
        )?;
        writeln!(out, "# TYPE rustybattery_energy_now_wh gauge")?;
        writeln!(
            out,
            "rustybattery_energy_now_wh{{device=\"{dev}\"}} {:.3}",
            bat.energy_now_wh
        )?;
        writeln!(
            out,
            "# HELP rustybattery_sleep_drain_percent_per_hour Sleep drain rate in percent per hour."
        )?;
        writeln!(out, "# TYPE rustybattery_sleep_drain_percent_per_hour gauge")?;
        writeln!(
            out,
            "rustybattery_sleep_drain_percent_per_hour {:.3}",
            drain.sleep_drain_pct_per_hr
        )?;
        writeln!(
            out,
            "# HELP rustybattery_app_energy_wh Per-application attributed energy in Watt-hours over the requested window."
        )?;
        writeln!(out, "# TYPE rustybattery_app_energy_wh gauge")?;
        for a in apps.iter().take(limit) {
            let safe_app = escape_prometheus_label(&a.app_name);
            let safe_cat = escape_prometheus_label(&a.category);
            writeln!(
                out,
                "rustybattery_app_energy_wh{{app=\"{safe_app}\",category=\"{safe_cat}\"}} {:.4}",
                a.energy_wh
            )?;
        }
        writeln!(
            out,
            "# HELP rustybattery_app_energy_wh_total Cumulative attributed energy in Watt-hours stored per application."
        )?;
        writeln!(out, "# TYPE rustybattery_app_energy_wh_total counter")?;
        for (app_name, cat, wh) in cumulative {
            let safe_app = escape_prometheus_label(&app_name);
            let safe_cat = escape_prometheus_label(&cat);
            writeln!(
                out,
                "rustybattery_app_energy_wh_total{{app=\"{safe_app}\",category=\"{safe_cat}\"}} {:.4}",
                wh
            )?;
        }
        return emit_output(output, &out);
    }

    if per_app {
        let apps = db.get_app_breakdown(since_ts, &[]).unwrap_or_default();
        writeln!(
            out,
            "app_name,category,battery_pct,energy_wh,share_of_drain_pct,active_secs,focused_secs,confidence"
        )?;
        for a in apps.into_iter().take(limit) {
            writeln!(
                out,
                "{},{},{:.2},{:.4},{:.2},{:.0},{:.0},{}",
                escape_csv_field(&a.app_name),
                escape_csv_field(&a.category),
                a.battery_pct,
                a.energy_wh,
                a.share_of_drain_pct,
                a.active_secs,
                a.focused_secs,
                escape_csv_field(a.confidence.as_str())
            )?;
        }
        return emit_output(output, &out);
    }

    // Default: CSV export of battery history
    let history = db.get_battery_history(since_ts)?;
    writeln!(out, "timestamp,capacity_pct,status,power_w")?;
    for pt in history {
        writeln!(
            out,
            "{},{:.2},{},{:.3}",
            pt.timestamp,
            pt.capacity_pct,
            escape_csv_field(&pt.status),
            pt.power_w
        )?;
    }
    emit_output(output, &out)
}

/// Generates shell completion scripts dynamically from `clap::Command` (`N-14`).
pub fn generate_shell_completions(shell: CompletionShell, cmd: &clap::Command) -> String {
    let visible_subs: Vec<&clap::Command> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .collect();

    match shell {
        CompletionShell::Bash => {
            let top_names: Vec<&str> = visible_subs.iter().map(|s| s.get_name()).collect();
            let mut cases = String::new();
            for sub in &visible_subs {
                let mut opts: Vec<String> = sub
                    .get_subcommands()
                    .filter(|c| !c.is_hide_set())
                    .map(|c| c.get_name().to_string())
                    .collect();
                for arg in sub.get_arguments() {
                    if let Some(long) = arg.get_long() {
                        opts.push(format!("--{long}"));
                    }
                }
                if !opts.is_empty() {
                    let _ = writeln!(
                        cases,
                        "        {}) COMPREPLY=( $(compgen -W \"{}\" -- \"${{cur}}\") ) ;;",
                        sub.get_name(),
                        opts.join(" ")
                    );
                }
            }
            format!(
                r#"_rustybattery_completions() {{
    local cur prev cmds
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    cmds="{}"
    if [[ ${{COMP_CWORD}} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "${{cmds}}" -- "${{cur}}") )
        return 0
    fi
    case "${{prev}}" in
{cases}    esac
}}
complete -F _rustybattery_completions battery rustybattery
"#,
                top_names.join(" ")
            )
        }
        CompletionShell::Zsh => {
            let mut entries = String::new();
            for sub in &visible_subs {
                let desc = sub
                    .get_about()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
                    .replace('\'', "'\\''")
                    .replace(['\r', '\n'], " ");
                let _ = writeln!(entries, "        '{}:{}'", sub.get_name(), desc);
            }
            format!(
                r#"#compdef battery rustybattery
_rustybattery() {{
    local -a commands
    commands=(
{entries}    )
    _describe 'command' commands
}}
compdef _rustybattery battery rustybattery
"#
            )
        }
        CompletionShell::Fish => {
            let mut lines = String::from(
                "complete -c battery -f\ncomplete -c rustybattery -f\nfor cmd in battery rustybattery\n",
            );
            for sub in &visible_subs {
                let name = sub.get_name();
                let desc = sub
                    .get_about()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
                    .replace('\'', "\\'")
                    .replace(['\r', '\n'], " ");
                let _ = writeln!(
                    lines,
                    "    complete -c $cmd -n '__fish_use_subcommand' -a '{name}' -d '{desc}'"
                );
                for arg in sub.get_arguments() {
                    if let Some(long) = arg.get_long() {
                        let _ = writeln!(
                            lines,
                            "    complete -c $cmd -n '__fish_seen_subcommand_from {name}' -l '{long}'"
                        );
                    }
                }
            }
            lines.push_str("end\n");
            lines
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_double_tab_kernel_wakeup_sources_fixture() {
        let sample = include_str!("../tests/fixtures/wakeup_sources.txt");
        let entries = parse_wakeup_sources_str(sample);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "xhci_hcd");
        assert_eq!(entries[0].active_count, 50);
        assert_eq!(entries[0].event_count, 64);
        assert_eq!(entries[0].wakeup_count, 2);
        assert_eq!(entries[0].total_time_ms, 12_000);

        assert_eq!(entries[1].name, "iwlwifi");
        assert_eq!(entries[1].event_count, 135);
        assert_eq!(entries[1].total_time_ms, 4_500);

        assert_eq!(entries[2].name, "Power Button");
        assert_eq!(entries[2].total_time_ms, 42);
    }

    #[test]
    fn reads_wakeup_sources_from_sysfs_dir() {
        let uniq = format!(
            "rb_wakeup_sysfs_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let root = std::env::temp_dir().join(uniq);
        let w0 = root.join("wakeup0");
        let w1 = root.join("wakeup1");
        let w2 = root.join("wakeup2");
        fs::create_dir_all(&w0).unwrap();
        fs::create_dir_all(&w1).unwrap();
        fs::create_dir_all(&w2).unwrap();

        fs::write(w0.join("name"), "AC0\n").unwrap();
        fs::write(w0.join("active_count"), "12\n").unwrap();
        fs::write(w0.join("event_count"), "17\n").unwrap();
        fs::write(w0.join("wakeup_count"), "0\n").unwrap();
        fs::write(w0.join("total_time_ms"), "38\n").unwrap();

        fs::write(w1.join("name"), "0000:74:00.6\n").unwrap();
        fs::write(w1.join("active_count"), "5\n").unwrap();
        fs::write(w1.join("event_count"), "11\n").unwrap();
        fs::write(w1.join("wakeup_count"), "1\n").unwrap();
        fs::write(w1.join("total_time_ms"), "527\n").unwrap();

        fs::write(w2.join("name"), "idle_dev\n").unwrap();
        fs::write(w2.join("active_count"), "0\n").unwrap();
        fs::write(w2.join("total_time_ms"), "0\n").unwrap();

        let entries = read_wakeup_sources_from_sysfs(&root).unwrap();
        let _ = fs::remove_dir_all(root);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "0000:74:00.6");
        assert_eq!(entries[0].total_time_ms, 527);
        assert_eq!(entries[1].name, "AC0");
        assert_eq!(entries[1].event_count, 17);
    }

    #[test]
    fn escapes_prometheus_markdown_and_csv_safely() {
        assert_eq!(
            escape_prometheus_label("App\"One\\Two\nThree\r"),
            "App\\\"One\\\\Two\\nThree"
        );
        assert_eq!(
            escape_markdown_cell("App|Pipe\nLine2"),
            "App\\|Pipe Line2"
        );
        assert_eq!(escape_csv_field("PlainApp"), "PlainApp");
        assert_eq!(
            escape_csv_field("App, \"Quoted\""),
            "\"App, \"\"Quoted\"\"\""
        );
    }

    #[test]
    fn atomic_write_sets_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let uniq = format!(
            "rb_atomic_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let dir = std::env::temp_dir().join(uniq);
        let target = dir.join("nested").join("export.csv");
        write_atomic_text(&target, "a,b\n1,2\n").unwrap();
        let file_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(target.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        assert_eq!(fs::read_to_string(&target).unwrap(), "a,b\n1,2\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generates_dynamic_completions_for_bash_zsh_fish() {
        let cmd = clap::Command::new("battery")
            .subcommand(clap::Command::new("record-trace").about("Record trace"))
            .subcommand(clap::Command::new("charge-limit").about("Charge limit"));
        let bash = generate_shell_completions(CompletionShell::Bash, &cmd);
        let zsh = generate_shell_completions(CompletionShell::Zsh, &cmd);
        let fish = generate_shell_completions(CompletionShell::Fish, &cmd);
        assert!(bash.contains("complete -F") && bash.contains("record-trace"));
        assert!(zsh.contains("#compdef") && zsh.contains("charge-limit"));
        assert!(fish.contains("complete -c battery") && fish.contains("record-trace"));
    }
}
