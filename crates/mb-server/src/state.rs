//! Shared server state. One mutex over everything — at bench message rates
//! (a few thousand msgs/s even at 10× replay) contention is not the
//! bottleneck, and the simplicity is worth more than a lock hierarchy.

use crate::clocksync::PairModel;
use crate::solve::{self, Observation};
use mb_core::{Ecef, Geodetic, Icao, C_MPS};
use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

pub struct ReceiverInfo {
    pub user: String,
    pub geo: Geodetic,
    pub ecef: Ecef,
    pub freq_hz: f64,
    pub gps: bool,
    /// Expected timing error fed to the weighted solve, seconds (1σ).
    /// Covers clock jitter plus pair-model slack; per clock type.
    pub jitter_s: f64,
}

/// Per-aircraft publication state — the oracle's tail-control heuristics
/// (mlattrack.py), ported: warm starts, solve backoff, and accuracy-scaled
/// output rate.
#[derive(Clone, Copy, Default)]
struct Track {
    last_pos: Option<Geodetic>,
    last_time_scaled: f64,
    last_attempt_scaled: f64,
}

struct SyncPoint {
    created: Instant,
    /// (receiver idx, corrected even transmit time, corrected odd transmit time)
    reporters: Vec<(usize, f64, f64)>,
}

struct Group {
    created: Instant,
    icao: Icao,
    /// (receiver idx, arrival time in that receiver's clock s, output clock
    /// at insertion). The insertion stamp rides along because one content key
    /// can hold SEVERAL transmissions — see solve_group.
    entries: Vec<(usize, f64, f64)>,
}

/// Learned per-receiver systematic timing bias — OUR addition, not an oracle
/// port. Wrong reported coordinates, cable/processing delay, altitude error:
/// they all present as a stable signed solve residual for that receiver, so
/// learn it (slow EMA from well-observed solves) and subtract it.
#[derive(Clone, Copy, Default)]
struct RxBias {
    bias_s: f64,
    /// EMA of squared residual deviation AFTER bias removal — the receiver's
    /// non-correctable scatter (e.g. the bearing-dependent part of a wrong
    /// reported position). Feeds the observation weight honestly.
    var_s2: f64,
    n: u32,
}

pub struct State {
    rx_bias: Vec<RxBias>,
    pub receivers: Vec<ReceiverInfo>,
    reference: Option<usize>,
    pairs: HashMap<(usize, usize), PairModel>,
    syncpoints: HashMap<(String, String), SyncPoint>,
    groups: HashMap<String, Group>,
    alts_ft: HashMap<Icao, i32>,
    tracks: HashMap<Icao, Track>,
    csv: std::io::BufWriter<std::fs::File>,
    // Scaled output clock (matches the oracle's faked-clock behavior at
    // accelerated replay; see the harness's scoring anchor).
    t0_real: Instant,
    t0_unix: f64,
    time_scale: f64,
    pub stats_solved: u64,
    pub stats_rejected: u64,
    pub stats_sync_obs: u64,
}

impl State {
    pub fn new(csv_path: &std::path::Path, time_scale: f64) -> anyhow::Result<Self> {
        Ok(State {
            rx_bias: Vec::new(),
            receivers: Vec::new(),
            reference: None,
            pairs: HashMap::new(),
            syncpoints: HashMap::new(),
            groups: HashMap::new(),
            alts_ft: HashMap::new(),
            tracks: HashMap::new(),
            csv: std::io::BufWriter::new(std::fs::File::create(csv_path)?),
            t0_real: Instant::now(),
            t0_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs_f64(),
            time_scale,
            stats_solved: 0,
            stats_rejected: 0,
            stats_sync_obs: 0,
        })
    }

    /// The server's output clock: real time, scaled. At time_scale 1 this is
    /// plain unix time.
    pub fn scaled_now(&self) -> f64 {
        self.t0_unix + self.t0_real.elapsed().as_secs_f64() * self.time_scale
    }

    pub fn add_receiver(&mut self, info: ReceiverInfo) -> usize {
        self.rx_bias.push(RxBias::default());
        let gps = info.gps;
        self.receivers.push(info);
        let idx = self.receivers.len() - 1;
        // A GPS-disciplined clock is the best possible hub for the pairwise
        // star; otherwise first-connected serves.
        match self.reference {
            None => self.reference = Some(idx),
            Some(r) if gps && !self.receivers[r].gps => self.reference = Some(idx),
            _ => {}
        }
        idx
    }

    pub fn clock_reset(&mut self, rx: usize) {
        self.pairs.retain(|(a, b), _| *a != rx && *b != rx);
    }

    /// A sync message from receiver `rx`: the same DF17 even/odd pair seen by
    /// several receivers is the shared event that trains pair clocks.
    pub fn on_sync(&mut self, rx: usize, et: f64, ot: f64, em_hex: &str, om_hex: &str) {
        let (Ok(em), Ok(om)) = (hex::decode(em_hex), hex::decode(om_hex)) else {
            return;
        };
        let (Some(de), Some(do_)) = (
            mb_modes::decode::parse_df17_airborne(&em),
            mb_modes::decode::parse_df17_airborne(&om),
        ) else {
            return;
        };
        if de.icao != do_.icao || de.odd || !do_.odd {
            return;
        }
        // Both decodes of the pair — each message gets its own position for
        // the propagation correction (aircraft move ~150 m between them).
        let even = (de.cpr_lat, de.cpr_lon);
        let odd = (do_.cpr_lat, do_.cpr_lon);
        let (Some(pe), Some(po)) = (
            mb_modes::cpr::global_decode_airborne(even, odd, false),
            mb_modes::cpr::global_decode_airborne(even, odd, true),
        ) else {
            return;
        };
        let alt_m = de.alt_ft.unwrap_or(0) as f64 * 0.3048;
        let pos_e = Geodetic {
            lat_deg: pe.0,
            lon_deg: pe.1,
            alt_m,
        }
        .to_ecef();
        let pos_o = Geodetic {
            lat_deg: po.0,
            lon_deg: po.1,
            alt_m,
        }
        .to_ecef();

        let freq = self.receivers[rx].freq_hz;
        let rxe = self.receivers[rx].ecef;
        let te = et / freq - dist(&rxe, &pos_e) / C_MPS;
        let to = ot / freq - dist(&rxe, &pos_o) / C_MPS;

        if let Some(alt) = de.alt_ft {
            self.alts_ft.insert(de.icao, alt);
        }

        let sp = self
            .syncpoints
            .entry((em_hex.to_string(), om_hex.to_string()))
            .or_insert_with(|| SyncPoint {
                created: Instant::now(),
                reporters: Vec::new(),
            });
        // Train every pair this receiver now shares the event with.
        let others: Vec<(usize, f64, f64)> = sp.reporters.clone();
        sp.reporters.push((rx, te, to));
        for (rx2, te2, to2) in others {
            if rx2 == rx {
                continue;
            }
            for (a, b, ta, tb) in [(rx, rx2, te, te2), (rx, rx2, to, to2)] {
                self.pairs.entry((a, b)).or_default().push(ta, tb);
                self.pairs.entry((b, a)).or_default().push(tb, ta);
                self.stats_sync_obs += 1;
            }
        }
    }

    /// An mlat message: group identical frames across receivers.
    pub fn on_mlat(&mut self, rx: usize, t_counts: f64, m_hex: &str) {
        let Ok(m) = hex::decode(m_hex) else { return };
        let icao = match mb_modes::decode::df_of(&m) {
            Some(4) => {
                let Some((icao, alt)) = mb_modes::decode::parse_df4(&m) else {
                    return;
                };
                if let Some(a) = alt {
                    self.alts_ft.insert(icao, a);
                }
                icao
            }
            Some(11) => match mb_modes::decode::parse_df11(&m) {
                Some(i) => i,
                None => return,
            },
            _ => return,
        };
        let t_s = t_counts / self.receivers[rx].freq_hz;
        let at_scaled = self.scaled_now();
        let g = self
            .groups
            .entry(m_hex.to_string())
            .or_insert_with(|| Group {
                created: Instant::now(),
                icao,
                entries: Vec::new(),
            });
        g.entries.push((rx, t_s, at_scaled));
    }

    /// Sweep: solve groups older than the window, expire stale sync points.
    pub fn sweep(&mut self, window: std::time::Duration) {
        let now = Instant::now();
        self.syncpoints
            .retain(|_, sp| now.duration_since(sp.created).as_secs_f64() < 4.0);

        let ready: Vec<String> = self
            .groups
            .iter()
            .filter(|(_, g)| now.duration_since(g.created) >= window)
            .map(|(k, _)| k.clone())
            .collect();
        for key in ready {
            let g = self.groups.remove(&key).expect("just listed");
            self.solve_group(&g);
        }
    }

    /// Publication gates, from the oracle's accumulated behavior:
    /// solve backoff per aircraft, covariance error ceiling, and the
    /// accuracy-scaled rate rule `elapsed/20 < err/max_err → skip`.
    const RESOLVE_BACKOFF_S: f64 = 0.4; // oracle: 0.7; we keep more rate
    const MAX_ERR_M: f64 = 10_000.0;
    /// Throttle scale. The oracle throttles with err/10 km — but its error
    /// estimates run ~9× real (bench, lab scenario), so its EFFECTIVE
    /// strictness is ~err_true/1.1 km. Our estimates are calibrated, so we
    /// throttle against a matching honest scale rather than copying the
    /// constant.
    const THROTTLE_SCALE_M: f64 = 1_500.0;

    fn solve_group(&mut self, g: &Group) {
        let Some(reference) = self.reference else {
            return;
        };
        // A level-flight aircraft's DF4/DF11 frames are byte-identical across
        // transmissions, so one content key collects SEVERAL distinct
        // broadcasts within the window; with packet loss, receivers'
        // "first" entries can belong to different transmissions. Solving such
        // a mix blends events ~0.25 s apart and reads as a ~1.5 s × speed
        // position bias (bench: 300 m bursts at p99). The cure is the
        // oracle's _cluster_timestamps idea: convert to the common timebase
        // FIRST, then split into physically consistent clusters — receivers
        // can only disagree by the network's light-crossing time.
        const CLUSTER_SPAN_S: f64 = 2.5e-3; // ~500 km network diameter / c

        let mut conv: Vec<(usize, f64, f64, f64)> = Vec::new(); // (rx, t_ref, sigma, at_scaled)
        for &(rx, t_s, at_scaled) in &g.entries {
            let t_ref = if rx == reference {
                Some((t_s, self.receivers[rx].jitter_s))
            } else {
                self.pairs
                    .get(&(rx, reference))
                    .and_then(|p| p.convert(t_s))
            };
            if let Some((t, sigma)) = t_ref {
                conv.push((rx, t, sigma.max(self.receivers[rx].jitter_s), at_scaled));
            }
        }
        if conv.len() < 4 {
            return;
        }
        conv.sort_by(|a, b| a.1.total_cmp(&b.1));

        // Greedy consistent clusters, each solved independently (each is a
        // distinct physical transmission).
        let mut i = 0;
        while i < conv.len() {
            let start_t = conv[i].1;
            let mut j = i;
            while j < conv.len() && conv[j].1 - start_t <= CLUSTER_SPAN_S {
                j += 1;
            }
            self.solve_cluster(g.icao, &conv[i..j]);
            i = j;
        }
    }

    fn solve_cluster(&mut self, icao: Icao, cluster: &[(usize, f64, f64, f64)]) {
        // One observation per receiver: earliest (direct path; any duplicate
        // within a cluster would be multipath in the real world).
        let mut seen = std::collections::HashSet::new();
        let mut obs = Vec::new();
        let mut users = Vec::new();
        let mut rx_ids = Vec::new();
        let mut stamp = f64::INFINITY;
        for &(rx, t, sigma, at_scaled) in cluster {
            if !seen.insert(rx) {
                continue;
            }
            // Apply the learned systematic bias: residual = predicted −
            // measured, so a positive stable residual means this receiver's
            // effective range is modeled too long — advance its clock reading.
            let b = self.rx_bias[rx];
            obs.push(Observation {
                rx: self.receivers[rx].ecef,
                t_s: t + b.bias_s,
                // NOTE: folding the learned residual variance into this
                // weight was tried and benched WORSE on the hostile world
                // (109/336/910 vs 105/293/852 m) — the pair-model sigma
                // already carries the receiver's scatter; double-counting it
                // over-flattens the weights. Scalar bias only.
                err_s: sigma,
            });
            users.push(self.receivers[rx].user.clone());
            rx_ids.push(rx);
            stamp = stamp.min(at_scaled);
        }
        if obs.len() < 4 {
            return;
        }
        let now_scaled = self.scaled_now();
        let track = *self.tracks.entry(icao).or_default();
        if now_scaled - track.last_attempt_scaled < Self::RESOLVE_BACKOFF_S {
            return;
        }
        let Some(&alt_ft) = self.alts_ft.get(&icao) else {
            return; // no altitude yet (DF11-only so far) — wait for a DF4
        };
        let alt_m = alt_ft as f64 * 0.3048;
        self.tracks
            .get_mut(&icao)
            .expect("entry above")
            .last_attempt_scaled = now_scaled;
        // Warm start from the last accepted fix when fresh (< 60 s), the
        // oracle's convergence aid; else the receivers' centroid.
        let init = match track.last_pos {
            Some(p) if now_scaled - track.last_time_scaled < 60.0 => Geodetic { alt_m, ..p },
            _ => {
                let n = obs.len() as f64;
                Geodetic {
                    lat_deg: users
                        .iter()
                        .filter_map(|u| self.receivers.iter().find(|r| &r.user == u))
                        .map(|r| r.geo.lat_deg)
                        .sum::<f64>()
                        / n,
                    lon_deg: users
                        .iter()
                        .filter_map(|u| self.receivers.iter().find(|r| &r.user == u))
                        .map(|r| r.geo.lon_deg)
                        .sum::<f64>()
                        / n,
                    alt_m,
                }
            }
        };
        match solve::solve_robust(&obs, alt_m, init) {
            Some(sol) => {
                // Covariance error ceiling + accuracy-scaled output rate:
                // bad fixes only pass after proportionally more silence.
                if sol.err_est_m > Self::MAX_ERR_M {
                    self.stats_rejected += 1;
                    return;
                }
                let elapsed = now_scaled - track.last_time_scaled;
                if track.last_pos.is_some()
                    && elapsed / 20.0 < sol.err_est_m / Self::THROTTLE_SCALE_M
                {
                    self.stats_rejected += 1;
                    return;
                }
                self.stats_solved += 1;
                // Learn per-receiver bias only from well-observed, full-set
                // solves (residual order matches rx_ids) with a slow EMA —
                // it must absorb the receiver's systematic error, not the
                // geometry of any single fix.
                if sol.residuals_s.len() == rx_ids.len()
                    && rx_ids.len() >= 5
                    && sol.err_est_m < 500.0
                {
                    for (i, &rx) in rx_ids.iter().enumerate() {
                        let b = &mut self.rx_bias[rx];
                        let k = if b.n < 50 { 0.10 } else { 0.02 };
                        let r = sol.residuals_s[i];
                        b.bias_s += k * r;
                        let dev = r - b.bias_s;
                        b.var_s2 += k * (dev * dev - b.var_s2);
                        b.n += 1;
                    }
                }
                let t = self.tracks.get_mut(&icao).expect("entry above");
                t.last_pos = Some(sol.pos);
                t.last_time_scaled = now_scaled;
                let err_m = sol.err_est_m;
                let row = format!(
                    "{:.3},{},,,{:.5},{:.5},{},{:.1},{},{},\"{}\",{},\n",
                    stamp,
                    icao.to_hex(),
                    sol.pos.lat_deg,
                    sol.pos.lon_deg,
                    alt_ft,
                    err_m,
                    obs.len(),
                    obs.len(),
                    users.join(","),
                    obs.len().saturating_sub(4),
                );
                let _ = self.csv.write_all(row.as_bytes());
                let _ = self.csv.flush();
            }
            None => {
                self.stats_rejected += 1;
            }
        }
    }
}

fn dist(a: &Ecef, b: &Ecef) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt()
}
