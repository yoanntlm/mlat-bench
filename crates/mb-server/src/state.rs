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
}

struct SyncPoint {
    created: Instant,
    /// (receiver idx, corrected even transmit time, corrected odd transmit time)
    reporters: Vec<(usize, f64, f64)>,
}

struct Group {
    created: Instant,
    /// Output clock at first reception — results are stamped with WHEN THE
    /// SIGNAL ARRIVED, not when we got around to solving it. Stamping at
    /// solve time cost 165 m of apparent error at cruise speed on the first
    /// bench run (group window + sweep lag × 450 kts); the bench caught it.
    created_scaled: f64,
    icao: Icao,
    /// (receiver idx, arrival time in that receiver's clock, seconds)
    entries: Vec<(usize, f64)>,
}

pub struct State {
    pub receivers: Vec<ReceiverInfo>,
    reference: Option<usize>,
    pairs: HashMap<(usize, usize), PairModel>,
    syncpoints: HashMap<(String, String), SyncPoint>,
    groups: HashMap<String, Group>,
    alts_ft: HashMap<Icao, i32>,
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
            receivers: Vec::new(),
            reference: None,
            pairs: HashMap::new(),
            syncpoints: HashMap::new(),
            groups: HashMap::new(),
            alts_ft: HashMap::new(),
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
        let created_scaled = self.scaled_now();
        let g = self
            .groups
            .entry(m_hex.to_string())
            .or_insert_with(|| Group {
                created: Instant::now(),
                created_scaled,
                icao,
                entries: Vec::new(),
            });
        g.entries.push((rx, t_s));
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

    fn solve_group(&mut self, g: &Group) {
        let Some(reference) = self.reference else {
            return;
        };
        // One arrival per receiver (first wins), converted to reference time.
        let mut seen = std::collections::HashSet::new();
        let mut obs = Vec::new();
        let mut users = Vec::new();
        for &(rx, t_s) in &g.entries {
            if !seen.insert(rx) {
                continue;
            }
            let t_ref = if rx == reference {
                Some(t_s)
            } else {
                self.pairs
                    .get(&(rx, reference))
                    .and_then(|p| p.convert(t_s))
            };
            if let Some(t) = t_ref {
                obs.push(Observation {
                    rx: self.receivers[rx].ecef,
                    t_s: t,
                });
                users.push(self.receivers[rx].user.clone());
            }
        }
        if obs.len() < 4 {
            return;
        }
        let Some(&alt_ft) = self.alts_ft.get(&g.icao) else {
            return; // no altitude yet (DF11-only so far) — wait for a DF4
        };
        let alt_m = alt_ft as f64 * 0.3048;
        // Init at the receivers' centroid — always inside the polygon.
        let n = obs.len() as f64;
        let init = Geodetic {
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
        };
        match solve::solve(&obs, alt_m, init) {
            Some(sol) => {
                self.stats_solved += 1;
                let err_m = sol.rms_s * C_MPS;
                let row = format!(
                    "{:.3},{},,,{:.5},{:.5},{},{:.1},{},{},\"{}\",{},\n",
                    g.created_scaled,
                    g.icao.to_hex(),
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
