//! Scoring: oracle output vs ground truth → metrics.json + report.md.
//!
//! Pure computation — the binary loads files and hands us plain data, so this
//! crate scores captures from any source (synthetic today, imported real
//! datasets later) without knowing where they came from.

use mb_core::{Geodetic, Icao, TruthPoint};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Ghost threshold: a matched result farther than this from truth is counted
/// as a ghost, not folded into the error percentiles.
pub const GHOST_M: f64 = 10_000.0;

/// Minimum receivers for "theoretically trackable" (a 3D TDOA solve).
pub const MIN_RX: usize = 4;

// ------------------------------------------------------------------ inputs

/// One row of the oracle's --write-csv output.
/// Columns (mlat/output.py): t, address, callsign, squawk, lat, lon, alt,
/// err, n, d, receivers, dof, vrate (+ Kalman extension we ignore for now).
#[derive(Debug, Clone)]
pub struct CsvRow {
    pub t_wall: f64,
    pub icao: Icao,
    pub lat: f64,
    pub lon: f64,
    pub alt_ft: Option<f64>,
    pub err_est_m: Option<f64>,
    pub n_receivers: Option<u32>,
    pub distinct_receivers: Option<u32>,
}

pub fn parse_results_csv(text: &str) -> Vec<CsvRow> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 10 {
            continue;
        }
        let (Ok(t), Some(icao)) = (f[0].parse::<f64>(), Icao::from_hex(f[1])) else {
            continue;
        };
        let (Ok(lat), Ok(lon)) = (f[4].parse::<f64>(), f[5].parse::<f64>()) else {
            continue;
        };
        out.push(CsvRow {
            t_wall: t,
            icao,
            lat,
            lon,
            alt_ft: f[6].trim().parse().ok(),
            err_est_m: f[7].trim().parse().ok(),
            n_receivers: f[8].trim().parse().ok(),
            distinct_receivers: f[9].trim().parse().ok(),
        });
    }
    out
}

/// Interpolating index over the 1 Hz truth log.
pub struct TruthIndex {
    by_icao: HashMap<Icao, Vec<(f64, Geodetic)>>,
}

impl TruthIndex {
    pub fn build(points: &[TruthPoint]) -> Self {
        let mut by_icao: HashMap<Icao, Vec<(f64, Geodetic)>> = HashMap::new();
        for p in points {
            by_icao
                .entry(p.icao)
                .or_default()
                .push((p.t.as_secs_f64(), p.pos));
        }
        for v in by_icao.values_mut() {
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        TruthIndex { by_icao }
    }

    pub fn known(&self, icao: Icao) -> bool {
        self.by_icao.contains_key(&icao)
    }

    pub fn pos_at(&self, icao: Icao, t_s: f64) -> Option<Geodetic> {
        let pts = self.by_icao.get(&icao)?;
        if pts.is_empty() || t_s < pts[0].0 - 1.0 || t_s > pts.last()?.0 + 1.0 {
            return None;
        }
        let i = pts.partition_point(|(t, _)| *t <= t_s).saturating_sub(1);
        let (t0, p0) = pts[i];
        let (t1, p1) = *pts.get(i + 1).unwrap_or(&pts[i]);
        if (t1 - t0).abs() < 1e-9 {
            return Some(p0);
        }
        let f = ((t_s - t0) / (t1 - t0)).clamp(0.0, 1.0);
        Some(Geodetic {
            lat_deg: p0.lat_deg + f * (p1.lat_deg - p0.lat_deg),
            lon_deg: p0.lon_deg + f * (p1.lon_deg - p0.lon_deg),
            alt_m: p0.alt_m + f * (p1.alt_m - p0.alt_m),
        })
    }
}

/// Audibility, decoupled from mb-sim's type: (t_s, icao, receiver count).
pub struct AudibilitySecond {
    pub t_s: u64,
    pub icao: Icao,
    pub n_receivers: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ResourceSample {
    pub t_wall: f64,
    pub cpu_usec: Option<u64>,
    pub mem_bytes: Option<u64>,
}

// ----------------------------------------------------------------- outputs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub schema_version: u32,
    pub results_total: usize,
    pub results_matched: usize,
    /// Results for aircraft that exist but have no truth (e.g. ADS-B sync
    /// sources a server also multilaterates). Unscoreable — never ghosts.
    pub unscored_known_aircraft: usize,
    pub ghosts_unknown_icao: usize,
    pub ghosts_gross_error: usize,
    pub horizontal_error_m: ErrorStats,
    pub altitude_error_m: Option<ErrorStats>,
    /// Oracle's own err column vs the real error: >1 means overconfident.
    pub err_estimate_ratio_p50: Option<f64>,
    pub per_aircraft: BTreeMap<String, AircraftMetrics>,
    pub coverage: Coverage,
    pub resources: Resources,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorStats {
    pub n: usize,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AircraftMetrics {
    pub results: usize,
    pub p50_error_m: Option<f64>,
    /// Seconds with ≥1 result ÷ seconds theoretically trackable (≥4 rx).
    pub coverage_ratio: Option<f64>,
    /// First result relative to first trackable second.
    pub ttff_s: Option<f64>,
    pub update_rate_per_min: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    /// Only Mode-S-only aircraft count here — ADS-B aircraft don't need MLAT.
    pub trackable_aircraft_seconds: u64,
    pub tracked_aircraft_seconds: u64,
    pub ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Resources {
    pub cpu_mean_pct: Option<f64>,
    pub cpu_max_pct: Option<f64>,
    pub mem_max_mb: Option<f64>,
    pub samples: usize,
}

fn stats(sorted: &[f64]) -> Option<ErrorStats> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let q = |p: f64| sorted[((n as f64 * p) as usize).min(n - 1)];
    Some(ErrorStats {
        n,
        p50: q(0.50),
        p90: q(0.90),
        p99: q(0.99),
        max: *sorted.last().unwrap(),
        mean: sorted.iter().sum::<f64>() / n as f64,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn score(
    truth: &TruthIndex,
    rows: &[CsvRow],
    wall_t0: f64,
    audibility: &[AudibilitySecond],
    mlat_targets: &HashSet<Icao>,
    known_untruthed: &HashSet<Icao>,
    resources: &[ResourceSample],
) -> Metrics {
    let mut unscored = 0usize;
    let mut herr: Vec<f64> = Vec::new();
    let mut aerr: Vec<f64> = Vec::new();
    let mut est_ratio: Vec<f64> = Vec::new();
    let (mut ghosts_unknown, mut ghosts_gross, mut matched) = (0usize, 0usize, 0usize);
    let mut per_ac: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new(); // icao -> (sim_t, err)

    for r in rows {
        if !truth.known(r.icao) {
            if known_untruthed.contains(&r.icao) {
                unscored += 1;
            } else {
                ghosts_unknown += 1;
            }
            continue;
        }
        let sim_t = r.t_wall - wall_t0;
        let Some(tp) = truth.pos_at(r.icao, sim_t) else {
            continue; // outside scenario window (drain-phase stragglers)
        };
        let e = Geodetic {
            lat_deg: r.lat,
            lon_deg: r.lon,
            alt_m: 0.0,
        }
        .haversine_m(&Geodetic { alt_m: 0.0, ..tp });
        if e > GHOST_M {
            ghosts_gross += 1;
            continue;
        }
        matched += 1;
        herr.push(e);
        if let Some(alt_ft) = r.alt_ft {
            aerr.push((alt_ft * 0.3048 - tp.alt_m).abs());
        }
        if let Some(est) = r.err_est_m {
            if e > 1.0 {
                est_ratio.push(est / e);
            }
        }
        per_ac.entry(r.icao.to_hex()).or_default().push((sim_t, e));
    }
    herr.sort_by(f64::total_cmp);
    aerr.sort_by(f64::total_cmp);
    est_ratio.sort_by(f64::total_cmp);

    // Coverage over Mode-S-only aircraft.
    let mut trackable: HashMap<Icao, Vec<u64>> = HashMap::new();
    for a in audibility {
        if a.n_receivers >= MIN_RX && mlat_targets.contains(&a.icao) {
            trackable.entry(a.icao).or_default().push(a.t_s);
        }
    }
    let trackable_secs: u64 = trackable.values().map(|v| v.len() as u64).sum();
    let mut tracked_secs = 0u64;
    let mut per_aircraft = BTreeMap::new();
    for (icao, secs) in &trackable {
        let hex = icao.to_hex();
        let tracked: HashSet<u64> = per_ac
            .get(&hex)
            .map(|v| {
                v.iter()
                    .filter(|(t, _)| *t >= 0.0)
                    .map(|(t, _)| *t as u64)
                    .collect()
            })
            .unwrap_or_default();
        let on_air: HashSet<u64> = secs.iter().copied().collect();
        let both = tracked.intersection(&on_air).count() as u64;
        tracked_secs += both;

        let first_trackable = secs.iter().min().copied();
        let errs = per_ac.get(&hex);
        let mut sorted_err: Vec<f64> = errs
            .map(|v| v.iter().map(|(_, e)| *e).collect())
            .unwrap_or_default();
        sorted_err.sort_by(f64::total_cmp);
        let first_result = errs.and_then(|v| v.iter().map(|(t, _)| *t).min_by(f64::total_cmp));
        let span_min = (secs.len() as f64 / 60.0).max(1.0 / 60.0);
        per_aircraft.insert(
            hex,
            AircraftMetrics {
                results: errs.map(|v| v.len()).unwrap_or(0),
                p50_error_m: stats(&sorted_err).map(|s| s.p50),
                coverage_ratio: if on_air.is_empty() {
                    None
                } else {
                    Some(both as f64 / on_air.len() as f64)
                },
                ttff_s: match (first_result, first_trackable) {
                    (Some(r), Some(a)) => Some(r - a as f64),
                    _ => None,
                },
                update_rate_per_min: errs.map(|v| v.len() as f64 / span_min),
            },
        );
    }

    // Resources: CPU% from usage deltas.
    let mut cpu_pcts = Vec::new();
    for w in resources.windows(2) {
        if let (Some(a), Some(b)) = (w[0].cpu_usec, w[1].cpu_usec) {
            let dt = w[1].t_wall - w[0].t_wall;
            if dt > 0.0 && b >= a {
                cpu_pcts.push((b - a) as f64 / (dt * 1e6) * 100.0);
            }
        }
    }
    let mem_max = resources.iter().filter_map(|r| r.mem_bytes).max();

    Metrics {
        schema_version: 1,
        results_total: rows.len(),
        results_matched: matched,
        unscored_known_aircraft: unscored,
        ghosts_unknown_icao: ghosts_unknown,
        ghosts_gross_error: ghosts_gross,
        horizontal_error_m: stats(&herr).unwrap_or(ErrorStats {
            n: 0,
            p50: f64::NAN,
            p90: f64::NAN,
            p99: f64::NAN,
            max: f64::NAN,
            mean: f64::NAN,
        }),
        altitude_error_m: stats(&aerr),
        err_estimate_ratio_p50: stats(&est_ratio).map(|s| s.p50),
        per_aircraft,
        coverage: Coverage {
            trackable_aircraft_seconds: trackable_secs,
            tracked_aircraft_seconds: tracked_secs,
            ratio: if trackable_secs > 0 {
                Some(tracked_secs as f64 / trackable_secs as f64)
            } else {
                None
            },
        },
        resources: Resources {
            cpu_mean_pct: if cpu_pcts.is_empty() {
                None
            } else {
                Some(cpu_pcts.iter().sum::<f64>() / cpu_pcts.len() as f64)
            },
            cpu_max_pct: cpu_pcts.iter().copied().max_by(f64::total_cmp),
            mem_max_mb: mem_max.map(|m| m as f64 / 1e6),
            samples: resources.len(),
        },
    }
}

pub fn render_report(m: &Metrics, run_name: &str) -> String {
    let h = &m.horizontal_error_m;
    let mut s = String::new();
    s.push_str(&format!("# mlat-bench report — {run_name}\n\n"));
    s.push_str(&format!(
        "**{} results**, {} matched to truth, {} ghosts (unknown icao) + {} gross (>{} km)\n\n",
        m.results_total,
        m.results_matched,
        m.ghosts_unknown_icao,
        m.ghosts_gross_error,
        GHOST_M / 1000.0
    ));
    s.push_str("## Horizontal error\n\n");
    s.push_str("| p50 | p90 | p99 | max | mean |\n|---|---|---|---|---|\n");
    s.push_str(&format!(
        "| {:.0} m | {:.0} m | {:.0} m | {:.0} m | {:.0} m |\n\n",
        h.p50, h.p90, h.p99, h.max, h.mean
    ));
    if let Some(a) = &m.altitude_error_m {
        s.push_str(&format!(
            "Altitude error: p50 {:.0} m (n={})\n\n",
            a.p50, a.n
        ));
    }
    if let Some(r) = m.err_estimate_ratio_p50 {
        s.push_str(&format!(
            "Oracle self-estimate ÷ real error, p50: {r:.2} (<1 = overconfident)\n\n"
        ));
    }
    if let Some(c) = m.coverage.ratio {
        s.push_str(&format!(
            "## Coverage\n\n{:.1}% of trackable aircraft-seconds tracked ({}/{})\n\n",
            c * 100.0,
            m.coverage.tracked_aircraft_seconds,
            m.coverage.trackable_aircraft_seconds
        ));
    }
    s.push_str("## Per aircraft (MLAT targets)\n\n");
    s.push_str(
        "| icao | results | p50 err | coverage | TTFF | rate/min |\n|---|---|---|---|---|---|\n",
    );
    for (icao, a) in &m.per_aircraft {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            icao,
            a.results,
            a.p50_error_m.map_or("—".into(), |v| format!("{v:.0} m")),
            a.coverage_ratio
                .map_or("—".into(), |v| format!("{:.0}%", v * 100.0)),
            a.ttff_s.map_or("—".into(), |v| format!("{v:.0} s")),
            a.update_rate_per_min
                .map_or("—".into(), |v| format!("{v:.1}")),
        ));
    }
    s.push_str(&format!(
        "\n## Oracle resources\n\nCPU mean {} / max {}, RSS max {} ({} samples)\n",
        m.resources
            .cpu_mean_pct
            .map_or("—".into(), |v| format!("{v:.1}%")),
        m.resources
            .cpu_max_pct
            .map_or("—".into(), |v| format!("{v:.1}%")),
        m.resources
            .mem_max_mb
            .map_or("—".into(), |v| format!("{v:.0} MB")),
        m.resources.samples
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_core::SimNanos;

    fn tp(t: u64, icao: u32, lat: f64, lon: f64) -> TruthPoint {
        TruthPoint {
            t: SimNanos(t * 1_000_000_000),
            icao: Icao(icao),
            pos: Geodetic {
                lat_deg: lat,
                lon_deg: lon,
                alt_m: 6000.0,
            },
            gs_mps: 200.0,
            vrate_mps: 0.0,
        }
    }

    #[test]
    fn parses_csv_rows() {
        let text =
            "1788000010.5,3944f1,,7700,47.2001,-1.5002,20000,85.2,5,5,\"a,b,c,d,e\",2,0\nbadline\n";
        let rows = parse_results_csv(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].icao, Icao(0x3944F1));
        assert_eq!(rows[0].err_est_m, Some(85.2));
    }

    #[test]
    fn scores_known_geometry() {
        // Truth: straight line lat 47→47.001 over 10 s. Result at sim_t=5
        // sits 0.0005° lat off (~55 m).
        let truth_pts: Vec<TruthPoint> = (0..=10)
            .map(|t| tp(t, 0x3944F1, 47.0 + t as f64 * 1e-4, -1.5))
            .collect();
        let truth = TruthIndex::build(&truth_pts);
        let wall_t0 = 1000.0;
        let rows = vec![
            CsvRow {
                t_wall: 1005.0,
                icao: Icao(0x3944F1),
                lat: 47.0005 + 0.0005,
                lon: -1.5,
                alt_ft: None,
                err_est_m: Some(60.0),
                n_receivers: Some(5),
                distinct_receivers: Some(5),
            },
            // Ghost: unknown aircraft.
            CsvRow {
                t_wall: 1005.0,
                icao: Icao(0xABCDEF),
                lat: 47.0,
                lon: -1.5,
                alt_ft: None,
                err_est_m: None,
                n_receivers: None,
                distinct_receivers: None,
            },
        ];
        let aud: Vec<AudibilitySecond> = (0..=10)
            .map(|t| AudibilitySecond {
                t_s: t,
                icao: Icao(0x3944F1),
                n_receivers: 5,
            })
            .collect();
        let targets: HashSet<Icao> = [Icao(0x3944F1)].into();
        let m = score(&truth, &rows, wall_t0, &aud, &targets, &HashSet::new(), &[]);
        assert_eq!(m.results_matched, 1);
        assert_eq!(m.ghosts_unknown_icao, 1);
        assert!(
            (m.horizontal_error_m.p50 - 55.6).abs() < 3.0,
            "p50 {}",
            m.horizontal_error_m.p50
        );
        let ac = &m.per_aircraft["3944f1"];
        assert_eq!(ac.results, 1);
        // TTFF: first result at sim 5, trackable from 0.
        assert_eq!(ac.ttff_s, Some(5.0));
    }
}
