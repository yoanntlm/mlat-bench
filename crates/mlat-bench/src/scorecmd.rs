//! `score`: run directory → metrics.json + report.md + stdout summary.

use anyhow::{Context, Result};
use mb_capture::CaptureReader;
use mb_core::Icao;
use mb_metrics::{AudibilitySecond, ResourceSample};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn score(run_dir: &Path) -> Result<()> {
    let run_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_dir.join("run.json"))
            .context("run.json — did the run finish?")?,
    )?;
    let wall_t0 = run_json["wall_t0"]
        .as_f64()
        .context("run.json lacks wall_t0 (pre-M4 run?)")?;
    let capture_path = run_json["capture"]
        .as_str()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| run_dir.join("capture"));

    let cap = CaptureReader::open(&capture_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let truth_pts = cap.truth().map_err(|e| anyhow::anyhow!("{e}"))?;
    let truth = mb_metrics::TruthIndex::build(&truth_pts);

    let audibility: Vec<AudibilitySecond> = cap
        .audibility_raw()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_iter()
        .filter_map(|v| {
            Some(AudibilitySecond {
                t_s: v.get("t_s")?.as_u64()?,
                icao: Icao(v.get("icao")?.as_u64()? as u32),
                n_receivers: v.get("receivers")?.as_array()?.len(),
            })
        })
        .collect();

    // MLAT targets = the scenario's modes_only aircraft.
    let sc = mb_sim::Scenario::from_toml(&cap.scenario_toml().map_err(|e| anyhow::anyhow!("{e}"))?)
        .context("parse capture scenario")?;
    let targets: HashSet<Icao> = sc
        .aircraft
        .iter()
        .filter(|a| matches!(a.kind, mb_sim::scenario::AircraftKind::ModesOnly))
        .filter_map(|a| Icao::from_hex(&a.icao))
        .collect();

    let csv = std::fs::read_to_string(run_dir.join("oracle-work/results.csv")).unwrap_or_default();
    let rows = mb_metrics::parse_results_csv(&csv);

    let resources: Vec<ResourceSample> = std::fs::read_to_string(run_dir.join("resources.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            Some(ResourceSample {
                t_wall: v.get("t")?.as_f64()?,
                cpu_usec: v.get("cpu_usec").and_then(|x| x.as_u64()),
                mem_bytes: v.get("mem_bytes").and_then(|x| x.as_u64()),
            })
        })
        .collect();

    let m = mb_metrics::score(&truth, &rows, wall_t0, &audibility, &targets, &resources);

    std::fs::write(run_dir.join("metrics.json"), serde_json::to_vec_pretty(&m)?)?;
    let name = run_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let report = mb_metrics::render_report(&m, &name);
    std::fs::write(run_dir.join("report.md"), &report)?;

    // stdout summary: the numbers someone quotes in a bug report.
    let h = &m.horizontal_error_m;
    println!(
        "score: {} results, {} matched",
        m.results_total, m.results_matched
    );
    println!(
        "score: horizontal error p50 {:.0} m / p90 {:.0} m / p99 {:.0} m",
        h.p50, h.p90, h.p99
    );
    if let Some(c) = m.coverage.ratio {
        println!("score: coverage {:.1}%", c * 100.0);
    }
    println!(
        "score: ghosts {} unknown + {} gross",
        m.ghosts_unknown_icao, m.ghosts_gross_error
    );
    println!("score: report at {}", run_dir.join("report.md").display());
    Ok(())
}
