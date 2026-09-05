//! The traffic engine: receptions in, per-server protocol messages out.
//!
//! One engine serves N MLAT servers from one Beast decode and one aircraft
//! table — the reason mlatc can replace a process-per-aggregator setup.
//! Aircraft state is shared; what is per-server is the selective-traffic
//! set, the announcement state, and the sync throttle, because each server
//! requests independently.
//!
//! Behavior follows mlat-client:
//! - An aircraft becomes known only through a CRC-validated DF11 or DF17;
//!   frames whose address is recovered from AP parity (DF0/4/5/16/20/21)
//!   count only for aircraft already known. This is the junk filter: a
//!   corrupted AP recovers to a random address.
//! - Nothing is sent to a server for an aircraft until that server
//!   requests it (start_sending). seen/lost/rate_report tell each server
//!   what is on offer.
//! - DF17 airborne position pairs (even + odd, close in time) become sync
//!   messages; other frames of requested aircraft become mlat messages.

use crate::beast::Reception;
use mb_proto::{ClientMsg, HexMsg};
use std::collections::HashMap;
use std::time::Instant;

const FREQ_12MHZ: f64 = 12e6;
/// Server-side pair-interval limit is 5 s; stay under it.
const MAX_PAIR_S: f64 = 4.0;
/// Minimum spacing between sync emissions per aircraft, per server. Zero:
/// every valid pair goes out, as mlat-client does; the server rate-limits
/// pairings itself. A 1 s limit here cost sync peers on a thin sky
/// (adsb.lol, 2026-09-05: 1 peer against mlat-client's 3 on the same
/// stream).
const SYNC_INTERVAL_S: f64 = 0.0;
const LOST_AFTER_S: f64 = 60.0;
const RATE_REPORT_S: f64 = 30.0;
/// A counter that moves backward by more than this is a clock reset
/// (a 48-bit wrap moves backward by nearly 2^48 and is exempt).
const RESET_BACKWARD_S: f64 = 1.0;
const WRAP_MARGIN: u64 = 1 << 46;

/// One server's view of one aircraft.
#[derive(Clone, Copy, Default)]
struct PerServer {
    requested: bool,
    announced: bool,
    last_sync: Option<Instant>,
}

struct Aircraft {
    last_seen: Instant,
    msgs_in_window: u32,
    even: Option<(u64, String)>,
    odd: Option<(u64, String)>,
    servers: Vec<PerServer>,
}

impl Aircraft {
    fn new(now: Instant, n_servers: usize) -> Self {
        Aircraft {
            last_seen: now,
            msgs_in_window: 0,
            even: None,
            odd: None,
            servers: vec![PerServer::default(); n_servers],
        }
    }
}

/// A message bound for one server.
pub type Outbound = (usize, ClientMsg);

pub struct Engine {
    aircraft: HashMap<u32, Aircraft>,
    n_servers: usize,
    last_counter: Option<u64>,
    last_rate_report: Instant,
    user: String,
}

impl Engine {
    pub fn new(user: &str, now: Instant, n_servers: usize) -> Self {
        Engine {
            aircraft: HashMap::new(),
            n_servers,
            last_counter: None,
            last_rate_report: now,
            user: user.to_string(),
        }
    }

    pub fn start_sending(&mut self, server: usize, hexes: &[String]) {
        self.set_requested(server, hexes, true);
    }

    pub fn stop_sending(&mut self, server: usize, hexes: &[String]) {
        self.set_requested(server, hexes, false);
    }

    fn set_requested(&mut self, server: usize, hexes: &[String], v: bool) {
        for h in hexes {
            if let Ok(addr) = u32::from_str_radix(h, 16) {
                if let Some(a) = self.aircraft.get_mut(&addr) {
                    a.servers[server].requested = v;
                }
            }
        }
    }

    /// One server's connection restarted: it no longer knows this client's
    /// aircraft. Re-announce everything to it; it has requested nothing.
    pub fn server_reset(&mut self, server: usize) {
        for a in self.aircraft.values_mut() {
            a.servers[server] = PerServer::default();
        }
    }

    /// The input reconnected or its clock restarted: drop pairing and
    /// counter state.
    pub fn input_reset(&mut self) {
        self.last_counter = None;
        for a in self.aircraft.values_mut() {
            a.even = None;
            a.odd = None;
        }
    }

    pub fn on_reception(&mut self, r: &Reception, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        if let Some(last) = self.last_counter {
            let backward = last.saturating_sub(r.counts_12mhz);
            if backward as f64 / FREQ_12MHZ > RESET_BACKWARD_S && backward < WRAP_MARGIN {
                for s in 0..self.n_servers {
                    out.push((s, ClientMsg::ClockReset(self.user.clone())));
                }
                self.input_reset();
            }
        }
        self.last_counter = Some(r.counts_12mhz);

        let f = &r.frame;
        let df = match mb_modes::decode::df_of(f) {
            Some(df) => df,
            None => return out,
        };
        let addr = match df {
            17 => {
                if mb_modes::crc24(f) != 0 {
                    return out;
                }
                let addr = be24(&f[1..4]);
                self.touch(addr, now, true);
                if let Some(d) = mb_modes::decode::parse_df17_airborne(f) {
                    self.pair_sync(d.icao.0, d.odd, r, now, &mut out);
                }
                return out; // DF17 contributes sync, not mlat
            }
            11 => {
                // PI = CRC ⊕ interrogator code; accept the 80 assigned codes.
                if mb_modes::crc24(f) >= 80 {
                    return out;
                }
                let addr = be24(&f[1..4]);
                self.touch(addr, now, true);
                addr
            }
            0 | 4 | 5 | 16 | 20 | 21 => {
                let n = f.len();
                let addr = (mb_modes::crc24(&f[..n - 3]) ^ be24(&f[n - 3..])) & 0xFF_FFFF;
                if !self.aircraft.contains_key(&addr) {
                    return out; // AP-recovered address, aircraft unknown
                }
                self.touch(addr, now, false);
                addr
            }
            _ => return out,
        };
        let a = self.aircraft.get_mut(&addr).expect("touched above");
        for (s, ps) in a.servers.iter().enumerate() {
            if ps.requested {
                out.push((
                    s,
                    ClientMsg::Mlat {
                        t: serde_json::Number::from(r.counts_12mhz),
                        m: HexMsg::from_bytes(f),
                    },
                ));
            }
        }
        out
    }

    fn touch(&mut self, addr: u32, now: Instant, validated: bool) {
        let n = self.n_servers;
        match self.aircraft.get_mut(&addr) {
            Some(a) => {
                a.last_seen = now;
                a.msgs_in_window += 1;
            }
            None if validated => {
                let mut a = Aircraft::new(now, n);
                a.msgs_in_window = 1;
                self.aircraft.insert(addr, a);
            }
            None => {}
        }
    }

    fn pair_sync(
        &mut self,
        addr: u32,
        odd: bool,
        r: &Reception,
        now: Instant,
        out: &mut Vec<Outbound>,
    ) {
        let Some(a) = self.aircraft.get_mut(&addr) else {
            return;
        };
        let this = (r.counts_12mhz, hex::encode(&r.frame));
        let slot = if odd { &mut a.odd } else { &mut a.even };
        *slot = Some(this);
        let (Some(even), Some(oddm)) = (&a.even, &a.odd) else {
            return;
        };
        let span = even.0.abs_diff(oddm.0) as f64 / FREQ_12MHZ;
        if span > MAX_PAIR_S {
            return;
        }
        for (s, ps) in a.servers.iter_mut().enumerate() {
            if !ps.requested {
                continue;
            }
            if let Some(t) = ps.last_sync {
                if now.duration_since(t).as_secs_f64() < SYNC_INTERVAL_S {
                    continue;
                }
            }
            ps.last_sync = Some(now);
            out.push((
                s,
                ClientMsg::Sync {
                    et: serde_json::Number::from(even.0),
                    ot: serde_json::Number::from(oddm.0),
                    em: HexMsg(even.1.clone()),
                    om: HexMsg(oddm.1.clone()),
                },
            ));
        }
    }

    /// Periodic bookkeeping: announce new aircraft, expire silent ones,
    /// report rates — per server, since each announces independently.
    pub fn tick(&mut self, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        let n = self.n_servers;
        let mut seen: Vec<Vec<String>> = vec![Vec::new(); n];
        let mut lost: Vec<Vec<String>> = vec![Vec::new(); n];
        self.aircraft.retain(|addr, a| {
            if now.duration_since(a.last_seen).as_secs_f64() > LOST_AFTER_S {
                for (s, ps) in a.servers.iter().enumerate() {
                    if ps.announced {
                        lost[s].push(format!("{addr:06x}"));
                    }
                }
                return false;
            }
            for (s, ps) in a.servers.iter_mut().enumerate() {
                if !ps.announced {
                    ps.announced = true;
                    seen[s].push(format!("{addr:06x}"));
                }
            }
            true
        });
        for s in 0..n {
            if !seen[s].is_empty() {
                out.push((s, ClientMsg::Seen(std::mem::take(&mut seen[s]))));
            }
            if !lost[s].is_empty() {
                out.push((s, ClientMsg::Lost(std::mem::take(&mut lost[s]))));
            }
        }
        let elapsed = now.duration_since(self.last_rate_report).as_secs_f64();
        if elapsed >= RATE_REPORT_S {
            self.last_rate_report = now;
            let mut rates = serde_json::Map::new();
            for (addr, a) in self.aircraft.iter_mut() {
                let rate = a.msgs_in_window as f64 / elapsed;
                a.msgs_in_window = 0;
                if let Some(num) = serde_json::Number::from_f64((rate * 100.0).round() / 100.0) {
                    rates.insert(format!("{addr:06x}"), serde_json::Value::Number(num));
                }
            }
            if !rates.is_empty() {
                for s in 0..n {
                    out.push((s, ClientMsg::RateReport(rates.clone())));
                }
            }
        }
        out
    }
}

fn be24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_core::Icao;
    use mb_modes::frames;

    fn rx(counts: u64, frame: Vec<u8>) -> Reception {
        Reception {
            counts_12mhz: counts,
            frame,
        }
    }

    fn engine(n: usize) -> (Engine, Instant) {
        let now = Instant::now();
        (Engine::new("t", now, n), now)
    }

    #[test]
    fn silent_until_requested_per_server() {
        let (mut e, now) = engine(2);
        let f = frames::df11(Icao(0xABCDEF), 0).to_vec();
        assert!(e.on_reception(&rx(1000, f.clone()), now).is_empty());
        let msgs = e.tick(now);
        assert_eq!(msgs.len(), 2, "both servers get the seen announcement");
        e.start_sending(1, &["abcdef".into()]);
        let out = e.on_reception(&rx(2000, f), now);
        assert_eq!(out.len(), 1, "only the requesting server gets traffic");
        assert_eq!(out[0].0, 1);
        assert!(matches!(out[0].1, ClientMsg::Mlat { .. }));
    }

    #[test]
    fn ap_recovered_needs_known_aircraft() {
        let (mut e, now) = engine(1);
        let df4 = frames::df4(Icao(0x3944F1), 0, 21000).unwrap().to_vec();
        assert!(e.on_reception(&rx(1000, df4.clone()), now).is_empty());
        assert!(
            e.tick(now).is_empty(),
            "unknown DF4 must not create aircraft"
        );
        let df11 = frames::df11(Icao(0x3944F1), 0).to_vec();
        e.on_reception(&rx(2000, df11), now);
        e.tick(now);
        e.start_sending(0, &["3944f1".into()]);
        let out = e.on_reception(&rx(3000, df4), now);
        assert!(matches!(out[0].1, ClientMsg::Mlat { .. }));
    }

    #[test]
    fn df17_pair_becomes_sync_for_each_requester() {
        let (mut e, now) = engine(2);
        let even = frames::df17_airborne_position(Icao(0x3C6444), 5, 11, 35000, 47.2, -1.5, false)
            .unwrap()
            .to_vec();
        let odd = frames::df17_airborne_position(Icao(0x3C6444), 5, 11, 35000, 47.2, -1.5, true)
            .unwrap()
            .to_vec();
        e.on_reception(&rx(12_000_000, even.clone()), now);
        e.tick(now);
        e.start_sending(0, &["3c6444".into()]);
        e.start_sending(1, &["3c6444".into()]);
        e.on_reception(&rx(13_000_000, even), now);
        let out = e.on_reception(&rx(16_000_000, odd), now);
        assert_eq!(out.len(), 2, "one sync per requesting server");
        for (_, m) in &out {
            let ClientMsg::Sync { et, ot, .. } = m else {
                panic!("expected sync, got {m:?}");
            };
            assert_eq!(et.as_u64(), Some(13_000_000));
            assert_eq!(ot.as_u64(), Some(16_000_000));
        }
    }

    #[test]
    fn server_reset_touches_only_that_server() {
        let (mut e, now) = engine(2);
        let f = frames::df11(Icao(0xABCDEF), 0).to_vec();
        e.on_reception(&rx(1000, f.clone()), now);
        e.tick(now);
        e.start_sending(0, &["abcdef".into()]);
        e.start_sending(1, &["abcdef".into()]);
        e.server_reset(0);
        let out = e.on_reception(&rx(2000, f), now);
        assert_eq!(out.len(), 1, "reset server no longer requests");
        assert_eq!(out[0].0, 1);
    }

    #[test]
    fn backward_counter_is_clock_reset_to_all() {
        let (mut e, now) = engine(3);
        let f = frames::df11(Icao(0xABCDEF), 0).to_vec();
        e.on_reception(&rx(120_000_000, f.clone()), now);
        let out = e.on_reception(&rx(1_000, f), now);
        assert_eq!(out.len(), 3);
        assert!(out
            .iter()
            .all(|(_, m)| matches!(m, ClientMsg::ClockReset(_))));
    }

    #[test]
    fn wrap_is_not_a_reset() {
        let (mut e, now) = engine(1);
        let f = frames::df11(Icao(0xABCDEF), 0).to_vec();
        e.on_reception(&rx(0xFFFF_FFFF_FF00, f.clone()), now);
        assert!(e.on_reception(&rx(0x10, f), now).is_empty());
    }

    #[test]
    fn corrupted_frames_ignored() {
        let (mut e, now) = engine(1);
        let mut f = frames::df11(Icao(0xABCDEF), 0).to_vec();
        f[2] ^= 0xFF;
        e.on_reception(&rx(1000, f), now);
        assert!(e.tick(now).is_empty());
    }
}
