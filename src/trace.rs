use crate::apps::{
    read_all_app_counters, AppEnergyTracker, Confidence, RawAppCounter, SampleInput,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFrame {
    pub timestamp: i64,
    pub monotonic_s: f64,
    pub suspended_s: f64,
    pub power_w: f64,
    pub energy_now_wh: f64,
    pub energy_full_wh: f64,
    pub discharging: bool,
    pub display_w: f64,
    pub rapl_uj: Option<u64>,
    pub counters: HashMap<String, RawAppCounter>,
}

#[derive(Debug, Clone)]
pub struct ReplaySummary {
    pub frames: usize,
    pub total_battery_wh: f64,
    pub total_attributed_wh: f64,
    pub display_wh: f64,
    pub unattributed_wh: f64,
    pub per_app_wh: HashMap<String, f64>,
    pub per_app_confidence: HashMap<String, Confidence>,
}

fn capture_frame() -> Result<TraceFrame> {
    let bat = crate::battery::read_battery_snapshot()?;
    Ok(TraceFrame {
        timestamp: chrono::Utc::now().timestamp(),
        monotonic_s: crate::battery::monotonic_secs(),
        suspended_s: crate::battery::suspended_total_secs(),
        power_w: bat.power_w,
        energy_now_wh: bat.energy_now_wh,
        energy_full_wh: bat.energy_full_wh,
        discharging: bat.status.is_discharging(),
        display_w: crate::battery::estimate_display_power_w(),
        rapl_uj: crate::battery::read_rapl_package_energy_uj(),
        counters: read_all_app_counters(),
    })
}

/// Records real hardware/cgroup snapshots as JSONL trace frames and writes atomically (`0600`).
pub fn record_trace(seconds: u64, interval_secs: u64, output: &Path) -> Result<()> {
    let interval = interval_secs.max(1);
    let total_secs = seconds.max(interval);
    let start = Instant::now();
    let mut lines = String::new();
    let mut count = 0usize;

    loop {
        let frame = capture_frame()?;
        lines.push_str(&serde_json::to_string(&frame)?);
        lines.push('\n');
        count += 1;

        if start.elapsed() >= Duration::from_secs(total_secs) {
            break;
        }
        let remaining = Duration::from_secs(total_secs).saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(Duration::from_secs(interval)));
    }

    crate::report::write_atomic_text(output, &lines)
        .with_context(|| format!("failed to write trace to {}", output.display()))?;
    println!("✅ Recorded {count} trace frames to {}", output.display());
    Ok(())
}

/// Replays a JSONL trace offline through `AppEnergyTracker::compute_delta_from_snapshots`.
pub fn replay_trace_str(
    jsonl: &str,
    cpu_w_per_core: f64,
    baseline_max_w: f64,
    rapl_idle_floor_w: f64,
) -> Result<ReplaySummary> {
    let mut frames = Vec::new();
    for (idx, line) in jsonl.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame: TraceFrame = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid trace JSONL on line {}", idx + 1))?;
        frames.push(frame);
    }

    let mut summary = ReplaySummary {
        frames: frames.len(),
        total_battery_wh: 0.0,
        total_attributed_wh: 0.0,
        display_wh: 0.0,
        unattributed_wh: 0.0,
        per_app_wh: HashMap::new(),
        per_app_confidence: HashMap::new(),
    };

    if frames.len() < 2 {
        return Ok(summary);
    }

    let mut tracker = AppEnergyTracker::from_initial(frames[0].counters.clone(), 300.0);
    let mut prev_mono = frames[0].monotonic_s;
    let mut prev_susp = frames[0].suspended_s;
    let mut prev_rapl = frames[0].rapl_uj;

    for frame in &frames[1..] {
        let raw_dt = (frame.monotonic_s - prev_mono).max(0.1);
        let slept_delta = (frame.suspended_s - prev_susp).max(0.0);
        prev_mono = frame.monotonic_s;
        prev_susp = frame.suspended_s;

        let rapl_power_w = match (prev_rapl, frame.rapl_uj) {
            (Some(prev), Some(curr)) if curr >= prev && raw_dt >= 0.5 => {
                let w = ((curr - prev) as f64 / 1_000_000.0) / raw_dt;
                (w > 0.05 && w < 200.0).then_some(w)
            }
            _ => None,
        };
        prev_rapl = frame.rapl_uj;

        let (deltas, dt) = tracker.compute_delta_from_snapshots(
            frame.counters.clone(),
            SampleInput {
                raw_dt_secs: raw_dt,
                slept_delta_secs: slept_delta,
                power_w: frame.power_w,
                energy_full_wh: frame.energy_full_wh,
                is_discharging: frame.discharging,
                raw_display_w: frame.display_w,
                rapl_power_w,
                cpu_w_per_core,
                baseline_max_w,
                rapl_idle_floor_w,
            },
        );

        if frame.discharging && slept_delta < 5.0 {
            summary.total_battery_wh += frame.power_w.max(0.0) * (dt / 3600.0);
            for d in deltas {
                summary.total_attributed_wh += d.energy_wh;
                *summary.per_app_wh.entry(d.app_name.clone()).or_insert(0.0) += d.energy_wh;
                summary
                    .per_app_confidence
                    .entry(d.app_name.clone())
                    .and_modify(|c| *c = c.worst(d.confidence))
                    .or_insert(d.confidence);

                if d.app_name == "Display & Backlight (est.)" {
                    summary.display_wh += d.energy_wh;
                } else if d.app_name == "Unattributed / Hardware & Idle" {
                    summary.unattributed_wh += d.energy_wh;
                }
            }
        }
    }

    Ok(summary)
}
