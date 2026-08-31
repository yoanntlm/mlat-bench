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
    // Accelerated runs: results.csv timestamps are in the oracle's FAKED
    // clock domain. One observed heartbeat (real r, faked h) anchors the
    // mapping; sim_t = (t_csv − h) + (r − wall_t0)·speed, folded into an
    // effective wall_t0 so the scorer below stays unchanged.
    let speed = run_json["speed"].as_f64().unwrap_or(1.0);
    let wall_t0 = if speed > 1.0 {
        let anchor = run_json["hb_anchor"]
            .as_array()
            .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
            .context("accelerated run but no heartbeat anchor in run.json")?;
        anchor.1 - (anchor.0 - wall_t0) * speed
    } else {
        wall_t0
    };
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
    // Aircraft that EXIST but carry no truth (ADS-B sync sources): results
    // for them are unscoreable, not ghosts. Mislabeling them inflated an
    // oracle 'ghost rate' 17x in one comparison — scoring bugs cut both ways.
    let known_untruthed: HashSet<Icao> = sc
        .aircraft
        .iter()
        .filter(|a| !matches!(a.kind, mb_sim::scenario::AircraftKind::ModesOnly))
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

    let m = mb_metrics::score(
        &truth,
        &rows,
        wall_t0,
        &audibility,
        &targets,
        &known_untruthed,
        &resources,
    );

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
    // Self-truth summary (the field metric: solved DF17s vs the aircraft's
    // own broadcast positions — works with zero external truth).
    let st_path = run_dir.join("oracle-work/selftruth.csv");
    if let Ok(text) = std::fs::read_to_string(&st_path) {
        let mut errs: Vec<f64> = text
            .lines()
            .filter_map(|l| l.split(',').nth(2)?.parse().ok())
            .collect();
        errs.sort_by(f64::total_cmp);
        if !errs.is_empty() {
            let n = errs.len();
            let q = |p: f64| errs[((n as f64 * p) as usize).min(n - 1)];
            println!(
                "score: self-truth (ADS-B as truth): n={} p50 {:.0} m / p90 {:.0} m",
                n,
                q(0.5),
                q(0.9)
            );
            let mut report = std::fs::read_to_string(run_dir.join("report.md")).unwrap_or_default();
            report.push_str(&format!(
                "\n## Self-truth (ADS-B aircraft as their own truth)\n\nn={} · p50 {:.0} m · p90 {:.0} m · p99 {:.0} m\n",
                n, q(0.5), q(0.9), q(0.99)
            ));
            let _ = std::fs::write(run_dir.join("report.md"), report);
        }
    }
    println!("score: report at {}", run_dir.join("report.md").display());
    Ok(())
}

/// `diff`: two scored runs side by side. Accepts run dirs or metrics.json
/// paths.
pub fn diff(a: &Path, b: &Path) -> Result<()> {
    let load = |p: &Path| -> Result<(String, serde_json::Value)> {
        let f = if p.is_dir() {
            p.join("metrics.json")
        } else {
            p.to_path_buf()
        };
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok((name, serde_json::from_str(&std::fs::read_to_string(&f)?)?))
    };
    let (na, ma) = load(a)?;
    let (nb, mb) = load(b)?;
    let g = |m: &serde_json::Value, path: &[&str]| -> Option<f64> {
        let mut v = m;
        for k in path {
            v = v.get(k)?;
        }
        v.as_f64()
    };
    let rows: &[(&str, &[&str])] = &[
        ("results", &["results_total"]),
        ("matched", &["results_matched"]),
        ("p50 err (m)", &["horizontal_error_m", "p50"]),
        ("p90 err (m)", &["horizontal_error_m", "p90"]),
        ("p99 err (m)", &["horizontal_error_m", "p99"]),
        ("mean err (m)", &["horizontal_error_m", "mean"]),
        ("ghosts unknown", &["ghosts_unknown_icao"]),
        ("ghosts gross", &["ghosts_gross_error"]),
        ("coverage", &["coverage", "ratio"]),
        ("cpu mean %", &["resources", "cpu_mean_pct"]),
        ("rss max MB", &["resources", "mem_max_mb"]),
    ];
    println!("{:<16} {:>14} {:>14}", "", na, nb);
    for (label, path) in rows {
        let fa = g(&ma, path);
        let fb = g(&mb, path);
        let fmt = |v: Option<f64>| v.map_or("—".to_string(), |x| format!("{x:.2}"));
        println!("{:<16} {:>14} {:>14}", label, fmt(fa), fmt(fb));
    }
    Ok(())
}
