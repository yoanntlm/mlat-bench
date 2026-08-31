//! The offline generation pipeline: scenario → deterministic per-client byte
//! streams with ground truth. Pure (no I/O, no wall clock); all randomness
//! comes from domain-separated streams of the scenario seed, so the same
//! scenario file produces the same capture, byte for byte, forever.

use crate::clock::ClockModel;
use crate::scenario::{AircraftKind, ClockSpec, ReceiverSpec, Scenario};
use mb_core::{rng_for, Geodetic, Icao, SimNanos, TruthPoint, C_MPS};
use mb_proto::framing::ZlibFrameEncoder;
use mb_proto::{ClientMsg, ClockType, Compress, Handshake, HexMsg};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// One aircraft transmission, before any receiver hears it.
struct Broadcast {
    t_s: f64,
    icao: Icao,
    kind: BroadcastKind,
    bytes: Vec<u8>,
    pos: Geodetic,
}

#[derive(Clone, Copy, PartialEq)]
enum BroadcastKind {
    Df17 { odd: bool },
    Df11,
    Df4,
}

/// Per-second geometric audibility: which receivers can hear this aircraft
/// (horizon + range cap; loss not applied — this is the "theoretically
/// trackable" denominator for coverage metrics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibilityRow {
    pub t_s: u64,
    pub icao: Icao,
    pub receivers: Vec<String>,
}

/// One timestamped chunk of client→server bytes.
pub struct SendRecord {
    pub t: SimNanos,
    pub bytes: Vec<u8>,
}

pub struct ClientStream {
    pub id: String,
    pub handshake_line: Vec<u8>,
    pub compress: String,
    pub clock_type: String,
    pub records: Vec<SendRecord>,
    /// Messages before framing — kept for `inspect` and tests.
    pub message_count: u64,
    pub sync_count: u64,
    pub mlat_count: u64,
}

pub struct GeneratedCapture {
    pub truth: Vec<TruthPoint>,
    pub audibility: Vec<AudibilityRow>,
    pub clients: Vec<ClientStream>,
}

/// Radio horizon distance in km for receiver/aircraft heights in meters
/// (4/3-earth model: d ≈ 4.12·(√h₁ + √h₂)).
fn horizon_km(h_rx_m: f64, h_ac_m: f64) -> f64 {
    4.12 * (h_rx_m.max(0.0).sqrt() + h_ac_m.max(0.0).sqrt())
}

fn audible(rx: &ReceiverSpec, ac_pos: &Geodetic) -> bool {
    let surface_km = rx.pos().haversine_m(ac_pos) / 1000.0;
    if surface_km > horizon_km(rx.alt_m, ac_pos.alt_m) {
        return false;
    }
    if let Some(cap) = rx.max_range_km {
        if surface_km > cap {
            return false;
        }
    }
    true
}

pub fn generate(sc: &Scenario) -> Result<GeneratedCapture, String> {
    sc.validate()?;
    let seed = sc.meta.seed;
    let dur_s = sc.meta.duration_s as f64;

    // ---- 1. Broadcasts + truth ------------------------------------------
    let mut broadcasts: Vec<Broadcast> = Vec::new();
    let mut truth: Vec<TruthPoint> = Vec::new();

    for ac in &sc.aircraft {
        let icao = Icao::from_hex(&ac.icao).expect("validated");
        let mut rng = rng_for(seed, &format!("emit/{}", ac.icao));

        // Truth log, 1 Hz.
        for t in 0..sc.meta.duration_s {
            let pos = ac.traj.position_at(t as f64);
            truth.push(TruthPoint {
                t: SimNanos(t * 1_000_000_000),
                icao,
                pos,
                gs_mps: ac.traj.gs_mps(),
                vrate_mps: ac.traj.vrate_mps(t as f64),
            });
        }

        // False navigation: a fixed seeded offset applied to broadcast
        // positions only. Truth and propagation stay correct; the aircraft
        // reports wrong coordinates (degraded or spoofed navigation).
        let nav_off = if ac.nav_error_m > 0.0 {
            let th: f64 = rng.gen_range(0.0..std::f64::consts::TAU);
            (
                ac.nav_error_m * th.sin() / 111_320.0,
                ac.nav_error_m * th.cos() / 111_320.0, // corrected per-lat below
            )
        } else {
            (0.0, 0.0)
        };
        match ac.kind {
            AircraftKind::Adsb => {
                // Even/odd alternating at pos_rate_hz with a small phase
                // jitter, like a real transponder.
                let period = 1.0 / ac.pos_rate_hz;
                let phase: f64 = rng.gen_range(0.0..period);
                let mut odd = false;
                let mut t = phase;
                while t < dur_s {
                    let jt = t + rng.gen_range(-0.02..0.02);
                    if jt >= 0.0 && jt < dur_s {
                        let pos = ac.traj.position_at(jt);
                        let bpos_lat = pos.lat_deg + nav_off.0;
                        let bpos_lon =
                            pos.lon_deg + nav_off.1 / pos.lat_deg.to_radians().cos().max(0.2);
                        let alt_ft = mb_modes::alt::quantize_25ft(pos.alt_m / 0.3048);
                        if let Some(f) = mb_modes::frames::df17_airborne_position(
                            icao, 5, 11, alt_ft, bpos_lat, bpos_lon, odd,
                        ) {
                            broadcasts.push(Broadcast {
                                t_s: jt,
                                icao,
                                kind: BroadcastKind::Df17 { odd },
                                bytes: f.to_vec(),
                                pos,
                            });
                        }
                    }
                    odd = !odd;
                    t += period;
                }
            }
            AircraftKind::ModesOnly => {
                // Poisson processes for DF11 (spontaneous) and DF4
                // (interrogation replies).
                let mut t = exp_gap(&mut rng, ac.df11_rate_hz);
                while t < dur_s {
                    let pos = ac.traj.position_at(t);
                    broadcasts.push(Broadcast {
                        t_s: t,
                        icao,
                        kind: BroadcastKind::Df11,
                        bytes: mb_modes::frames::df11(icao, 5).to_vec(),
                        pos,
                    });
                    t += exp_gap(&mut rng, ac.df11_rate_hz);
                }
                let mut t = exp_gap(&mut rng, ac.df4_rate_hz);
                while t < dur_s {
                    let pos = ac.traj.position_at(t);
                    let alt_ft = mb_modes::alt::quantize_25ft(pos.alt_m / 0.3048);
                    if let Some(f) = mb_modes::frames::df4(icao, 0, alt_ft) {
                        broadcasts.push(Broadcast {
                            t_s: t,
                            icao,
                            kind: BroadcastKind::Df4,
                            bytes: f.to_vec(),
                            pos,
                        });
                    }
                    t += exp_gap(&mut rng, ac.df4_rate_hz);
                }
            }
        }
    }
    broadcasts.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));

    // ---- 2. Audibility (geometric, per second) ---------------------------
    let mut audibility = Vec::new();
    for ac in &sc.aircraft {
        let icao = Icao::from_hex(&ac.icao).expect("validated");
        for t in 0..sc.meta.duration_s {
            let pos = ac.traj.position_at(t as f64);
            let rxs: Vec<String> = sc
                .receivers
                .iter()
                .filter(|r| audible(r, &pos))
                .map(|r| r.id.clone())
                .collect();
            if !rxs.is_empty() {
                audibility.push(AudibilityRow {
                    t_s: t,
                    icao,
                    receivers: rxs,
                });
            }
        }
    }

    // ---- 3. Per-receiver reception + client message stream ---------------
    let mut clients = Vec::new();
    for rx in &sc.receivers {
        clients.push(build_client(sc, rx, &broadcasts, seed)?);
    }

    Ok(GeneratedCapture {
        truth,
        audibility,
        clients,
    })
}

/// Box-Muller, local copy (clock.rs keeps its own private one).
fn gaussian_pub(rng: &mut rand_chacha::ChaCha12Rng) -> f64 {
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn exp_gap(rng: &mut rand_chacha::ChaCha12Rng, rate_hz: f64) -> f64 {
    let u: f64 = rng.gen_range(f64::EPSILON..1.0);
    -u.ln() / rate_hz
}

/// A stored DF17 reception awaiting its opposite-parity partner.
struct PendingHalf {
    odd: bool,
    t_arrive: f64,
    count: u64,
    bytes: Vec<u8>,
}

fn build_client(
    sc: &Scenario,
    rx: &ReceiverSpec,
    broadcasts: &[Broadcast],
    seed: u64,
) -> Result<ClientStream, String> {
    let compress = match rx.compress.as_str() {
        "none" => Compress::None,
        "zlib" => Compress::Zlib,
        "zlib2" => Compress::Zlib2,
        other => return Err(format!("unknown compress {other}")),
    };
    let clock_type_str = ClockModel::wire_clock_type(&rx.clock);

    let mut net_rng = rng_for(seed, &format!("net/{}", rx.id));
    let latency_s =
        net_rng.gen_range(sc.network.latency_ms_min..=sc.network.latency_ms_max) / 1000.0;
    let jitter_s = sc.network.jitter_ms / 1000.0;

    // Reported (handshake) position: true position + a fixed seeded offset.
    let (rep_lat, rep_lon) = if rx.reported_pos_error_m > 0.0 {
        let th: f64 = net_rng.gen_range(0.0..std::f64::consts::TAU);
        (
            rx.lat + rx.reported_pos_error_m * th.sin() / 111_320.0,
            rx.lon
                + rx.reported_pos_error_m * th.cos()
                    / (111_320.0 * rx.lat.to_radians().cos().max(0.2)),
        )
    } else {
        (rx.lat, rx.lon)
    };
    let mut mp_rng = rng_for(seed, &format!("mp/{}", rx.id));

    let handshake = Handshake {
        version: 3,
        user: rx.id.clone(),
        uuid: None,
        compress: vec![compress],
        lat: rep_lat,
        lon: rep_lon,
        alt: rx.alt_m,
        clock_type: match rx.clock {
            ClockSpec::Dump1090 { .. } => ClockType::Dump1090,
            ClockSpec::RadarcapeGps { .. } => ClockType::RadarcapeGps,
        },
        // Results off: with hundreds of clients, echoing every fix to every
        // client is ~N×results lines of traffic the REPLAY process must
        // read — at 4× it drowned the bench's own heartbeat anchoring
        // (uniform 10.3 s scoring skew; server was fine). Result delivery is
        // verified by dedicated runs, not every capture.
        return_results: Some(false),
        return_result_format: None,
        client_version: Some(format!("mlat-bench {}", env!("CARGO_PKG_VERSION"))),
        selective_traffic: None,
        heartbeat: None,
    };

    let mut clock = ClockModel::new(&rx.clock);
    if clock.is_hostile() {
        clock.set_hostile_rng(rng_for(seed, &format!("hostile/{}", rx.id)));
    }
    // One jitter stream per (rx, aircraft) keeps draws stable when the
    // scenario gains or loses other aircraft.
    let mut jitter_rngs: std::collections::HashMap<Icao, rand_chacha::ChaCha12Rng> =
        std::collections::HashMap::new();
    let mut loss_rngs: std::collections::HashMap<Icao, rand_chacha::ChaCha12Rng> =
        std::collections::HashMap::new();

    // Timestamped JSON lines (pre-framing).
    let mut lines: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut seen: std::collections::HashSet<Icao> = Default::default();
    let mut pending: std::collections::HashMap<Icao, PendingHalf> = Default::default();
    let (mut message_count, mut sync_count, mut mlat_count) = (0u64, 0u64, 0u64);

    // mlat-client announces its input right after connecting; the server
    // then resets the (empty) clock state. Harmless, and matches the real
    // client.
    lines.push((0.1, ClientMsg::InputConnected("connected".into()).to_line()));
    message_count += 1;

    // Heartbeats every 30 s of scenario time.
    let mut next_hb = 30.0;

    for b in broadcasts {
        if !audible(rx, &b.pos) {
            continue;
        }
        let loss = loss_rngs
            .entry(b.icao)
            .or_insert_with(|| rng_for(seed, &format!("loss/{}/{}", rx.id, b.icao.to_hex())));
        if loss.gen_range(0.0..1.0) < rx.loss_prob {
            continue;
        }
        let mut t_arrive = b.t_s + rx.pos().slant_range_m(&b.pos) / C_MPS;
        if rx.multipath_prob > 0.0 && mp_rng.gen_range(0.0f64..1.0) < rx.multipath_prob {
            // A reflection's longer path: the direct pulse was missed and the
            // echo got stamped instead.
            t_arrive += mp_rng.gen_range(0.5e-6..3e-6);
        }
        while next_hb < t_arrive {
            lines.push((next_hb, ClientMsg::heartbeat_now().to_line()));
            message_count += 1;
            next_hb += 30.0;
        }
        let jit = jitter_rngs
            .entry(b.icao)
            .or_insert_with(|| rng_for(seed, &format!("clkjit/{}/{}", rx.id, b.icao.to_hex())));
        let (count, jumped) = clock.count_at_hostile(t_arrive, jit);
        if jumped {
            // Real mlat-client detects the discontinuity and tells the server.
            lines.push((t_arrive, ClientMsg::ClockJump("jump".into()).to_line()));
            message_count += 1;
        }

        if seen.insert(b.icao) {
            lines.push((t_arrive, ClientMsg::seen(&[b.icao]).to_line()));
            message_count += 1;
        }

        match b.kind {
            BroadcastKind::Df17 { odd } => {
                let make_sync = matches!(pending.get(&b.icao),
                    Some(p) if p.odd != odd && (t_arrive - p.t_arrive) < 2.0);
                if make_sync {
                    let p = pending.remove(&b.icao).expect("checked");
                    let (et, ot, em, om) = if odd {
                        // p is even, b is odd
                        (p.count, count, p.bytes.clone(), b.bytes.clone())
                    } else {
                        (count, p.count, b.bytes.clone(), p.bytes.clone())
                    };
                    lines.push((
                        t_arrive,
                        ClientMsg::Sync {
                            et: et.into(),
                            ot: ot.into(),
                            em: HexMsg::from_bytes(&em),
                            om: HexMsg::from_bytes(&om),
                        }
                        .to_line(),
                    ));
                    message_count += 1;
                    sync_count += 1;
                } else {
                    pending.insert(
                        b.icao,
                        PendingHalf {
                            odd,
                            t_arrive,
                            count,
                            bytes: b.bytes.clone(),
                        },
                    );
                }
            }
            BroadcastKind::Df11 | BroadcastKind::Df4 => {
                lines.push((
                    t_arrive,
                    ClientMsg::Mlat {
                        t: count.into(),
                        m: HexMsg::from_bytes(&b.bytes),
                    }
                    .to_line(),
                ));
                message_count += 1;
                mlat_count += 1;
            }
        }
    }

    // ---- Framing into timestamped send records ---------------------------
    // Broadcasts are iterated in emission order but propagation delay differs
    // per broadcast, so arrival order can differ by a few ms — sort, as the
    // real receiver's input naturally is.
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    let records = frame_lines(&lines, compress, latency_s, jitter_s, &mut net_rng)?;

    Ok(ClientStream {
        id: rx.id.clone(),
        handshake_line: handshake.to_line(),
        compress: rx.compress.clone(),
        clock_type: clock_type_str.to_string(),
        records,
        message_count,
        sync_count,
        mlat_count,
    })
}

/// Apply wire framing and network latency. `none`: one record per line.
/// `zlib`: one frame per line (each flush = one message batch, the client's
/// low-latency mode). `zlib2`: 1-second batches, the bandwidth mode.
fn frame_lines(
    lines: &[(f64, Vec<u8>)],
    compress: Compress,
    latency_s: f64,
    jitter_s: f64,
    rng: &mut rand_chacha::ChaCha12Rng,
) -> Result<Vec<SendRecord>, String> {
    let to_nanos = |t: f64, rng: &mut rand_chacha::ChaCha12Rng| {
        let j = if jitter_s > 0.0 {
            (gaussian_pub(rng) * jitter_s).max(-latency_s * 0.9)
        } else {
            0.0
        };
        SimNanos((((t + latency_s + j) * 1e9).round()).max(0.0) as u64)
    };
    match compress {
        Compress::None => Ok(lines
            .iter()
            .map(|(t, l)| SendRecord {
                t: to_nanos(*t, rng),
                bytes: l.clone(),
            })
            .collect()),
        Compress::Zlib => {
            let mut enc = ZlibFrameEncoder::new();
            lines
                .iter()
                .map(|(t, l)| {
                    Ok(SendRecord {
                        t: to_nanos(*t, rng),
                        bytes: enc.encode_frame(l).map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
        Compress::Zlib2 => {
            // Batch lines into 1 s windows; the frame goes out at window end.
            let mut enc = ZlibFrameEncoder::new();
            let mut out = Vec::new();
            let mut batch: Vec<u8> = Vec::new();
            let mut window_end = match lines.first() {
                Some((t, _)) => t.floor() + 1.0,
                None => return Ok(out),
            };
            for (t, l) in lines {
                while *t >= window_end {
                    if !batch.is_empty() {
                        out.push(SendRecord {
                            t: to_nanos(window_end, rng),
                            bytes: enc.encode_frame(&batch).map_err(|e| e.to_string())?,
                        });
                        batch.clear();
                    }
                    window_end += 1.0;
                }
                batch.extend_from_slice(l);
            }
            if !batch.is_empty() {
                out.push(SendRecord {
                    t: to_nanos(window_end, rng),
                    bytes: enc.encode_frame(&batch).map_err(|e| e.to_string())?,
                });
            }
            Ok(out)
        }
    }
}
