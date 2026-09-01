//! `import-locards`: LocaRDS (real OpenSky receivers, published truth) → MBC
//! capture.
//!
//! What's real here — the part that matters: sensor geometry and each
//! sensor's raw nanosecond timestamps, with genuine crowdsourced clock
//! behavior (offsets, drift, jitter of real hardware). What's synthesized:
//! the frame bytes, re-encoded from the row's ground-truth position with
//! the bench's encoder — LocaRDS doesn't ship raw messages, and the servers only
//! need consistent decodable frames.
//!
//! A held-out fraction of aircraft is re-emitted as DF4-only (altitude
//! replies): the servers must multilaterate them while truth.jsonl knows
//! where they were. Everything else becomes DF17 sync traffic.
//!
//! Schema (set_N.csv): id,timeAtServer,aircraft,latitude,longitude,
//! baroAltitude,geoAltitude,numMeasurements,measurements where measurements
//! is a JSON array of [sensor_serial, timestampNs, RSSI].
//! Sensors (set_N_sensors.csv): serial,latitude,longitude,height,type,good.

use anyhow::{Context, Result};
use mb_capture::{CaptureWriter, ClientEntry, Manifest, REC_C2S, REC_CONNECT};
use mb_core::{rng_for, Geodetic, Icao, SimNanos, TruthPoint};
use rand::Rng;
use std::collections::HashMap;
use std::path::Path;

const M_TO_FT: f64 = 3.280_839_895;

struct Sensor {
    geo: Geodetic,
    user: String,
}

/// Per-sensor accumulating client stream (uncompressed lines).
#[derive(Default)]
struct ClientAcc {
    lines: Vec<(f64, String)>, // (timeAtServer, json line)
    pending: HashMap<Icao, (bool, f64, f64, String)>, // icao -> (odd, tAtServer, raw ns, hex)
    seen: std::collections::HashSet<Icao>,
}

pub fn import(
    set_csv: &Path,
    sensors_csv: &Path,
    out: &Path,
    duration_s: u64,
    holdout_frac: f64,
    seed: u64,
) -> Result<()> {
    // ---- sensors ---------------------------------------------------------
    let mut sensors: HashMap<u64, Sensor> = HashMap::new();
    let mut rd = csv::Reader::from_path(sensors_csv).context("open sensors csv")?;
    for rec in rd.records() {
        let r = rec?;
        let serial: u64 = r[0].parse()?;
        sensors.insert(
            serial,
            Sensor {
                geo: Geodetic {
                    lat_deg: r[1].parse()?,
                    lon_deg: r[2].parse()?,
                    alt_m: r[3].parse::<f64>().unwrap_or(0.0).clamp(-1000.0, 10000.0),
                },
                user: format!("os-{serial}"),
            },
        );
    }
    println!("import: {} sensors", sensors.len());

    // ---- pass over rows --------------------------------------------------
    let mut rd = csv::Reader::from_path(set_csv).context("open set csv")?;
    let mut clients: HashMap<u64, ClientAcc> = HashMap::new();
    let mut truth: Vec<TruthPoint> = Vec::new();
    let mut audibility: Vec<serde_json::Value> = Vec::new();
    let mut holdout: HashMap<u32, bool> = HashMap::new(); // aircraft id -> is holdout
    let mut parity: HashMap<u32, bool> = HashMap::new();
    let mut rng = rng_for(seed, "locards/holdout");
    let (mut rows_used, mut rows_no_truth) = (0u64, 0u64);

    for rec in rd.records() {
        let r = rec?;
        let t_server: f64 = r[1].parse()?;
        if t_server > duration_s as f64 {
            break; // rows are time-ordered
        }
        let ac_id: u32 = r[2].parse()?;
        let (Ok(lat), Ok(lon), Ok(baro_m)) = (
            r[3].parse::<f64>(),
            r[4].parse::<f64>(),
            r[5].parse::<f64>(),
        ) else {
            rows_no_truth += 1;
            continue; // competition-withheld truth: unusable for us
        };
        let meas: Vec<(u64, f64)> = serde_json::from_str::<Vec<serde_json::Value>>(&r[8])
            .unwrap_or_default()
            .iter()
            .filter_map(|m| Some((m.get(0)?.as_u64()?, m.get(1)?.as_f64()?)))
            .collect();
        if meas.len() < 2 {
            continue;
        }
        let icao = Icao(0x100000 + ac_id);
        let is_holdout = *holdout
            .entry(ac_id)
            .or_insert_with(|| rng.gen_range(0.0f64..1.0) < holdout_frac);
        let alt_ft = mb_modes::alt::quantize_25ft(baro_m * M_TO_FT);

        // Truth + audibility for holdout targets.
        if is_holdout {
            truth.push(TruthPoint {
                t: SimNanos((t_server * 1e9) as u64),
                icao,
                pos: Geodetic {
                    lat_deg: lat,
                    lon_deg: lon,
                    alt_m: baro_m,
                },
                gs_mps: 0.0,
                vrate_mps: 0.0,
            });
            audibility.push(serde_json::json!({
                "t_s": t_server as u64, "icao": icao.0,
                "receivers": meas.iter().map(|(s,_)| format!("os-{s}")).collect::<Vec<_>>(),
            }));
        }

        // Frame for this transmission (identical bytes for every sensor).
        let frame_hex = if is_holdout {
            match mb_modes::frames::df4(icao, 0, alt_ft) {
                Some(f) => hex::encode(f),
                None => continue,
            }
        } else {
            let odd = {
                let p = parity.entry(ac_id).or_insert(false);
                *p = !*p;
                *p
            };
            match mb_modes::frames::df17_airborne_position(icao, 5, 11, alt_ft, lat, lon, odd) {
                Some(f) => hex::encode(f),
                None => continue,
            }
        };
        rows_used += 1;

        for (serial, ns) in &meas {
            if !sensors.contains_key(serial) {
                continue;
            }
            let acc = clients.entry(*serial).or_default();
            // Wire clock: every sensor declared dump1090, raw ns → 12 MHz counts.
            let counts = (ns * 0.012).round().max(0.0) as u64;
            if acc.seen.insert(icao) {
                acc.lines
                    .push((t_server, format!("{{\"seen\":[\"{}\"]}}", icao.to_hex())));
            }
            if is_holdout {
                acc.lines.push((
                    t_server,
                    format!("{{\"mlat\":{{\"t\":{counts},\"m\":\"{frame_hex}\"}}}}"),
                ));
            } else {
                // Client-side even/odd pairing, exactly like mlat-client.
                let this_odd = parity[&ac_id];
                let paired = matches!(acc.pending.get(&icao),
                    Some((podd, pt, _, _)) if *podd != this_odd && (t_server - pt) < 2.0);
                if paired {
                    let (_, _, pns, phex) = acc.pending.remove(&icao).expect("checked");
                    let pcounts = (pns * 0.012).round().max(0.0) as u64;
                    let (et, em, ot, om) = if this_odd {
                        (pcounts, phex, counts, frame_hex.clone())
                    } else {
                        (counts, frame_hex.clone(), pcounts, phex)
                    };
                    acc.lines.push((
                        t_server,
                        format!(
                            "{{\"sync\":{{\"et\":{et},\"em\":\"{em}\",\"ot\":{ot},\"om\":\"{om}\"}}}}"
                        ),
                    ));
                } else {
                    acc.pending
                        .insert(icao, (this_odd, t_server, *ns, frame_hex.clone()));
                }
            }
        }
    }
    println!(
        "import: {rows_used} transmissions used, {rows_no_truth} without truth skipped, {} active sensors, {} holdout aircraft",
        clients.len(),
        holdout.values().filter(|h| **h).count()
    );

    // ---- write capture ---------------------------------------------------
    let w = CaptureWriter::create(out).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Minimal scenario so scoring knows the MLAT targets.
    let mut sc = String::from(
        "# synthesized by import-locards — targets only, no physics\n[meta]\nname = \"locards\"\n",
    );
    sc.push_str(&format!("seed = {seed}\nduration_s = {duration_s}\n"));
    for (ac, hold) in &holdout {
        let icao = Icao(0x100000 + ac);
        sc.push_str(&format!(
            "[[aircraft]]\nicao = \"{}\"\nkind = \"{}\"\ntraj = {{ type = \"great_circle\", from = [0,0,10000], to = [0,1,10000], gs_kts = 400 }}\n",
            icao.to_hex().to_uppercase(),
            if *hold { "modes_only" } else { "adsb" }
        ));
    }
    let sha = w.write_scenario_toml(&sc)?;
    w.write_truth(truth.iter())?;
    w.write_audibility(audibility.into_iter())?;

    let mut entries = Vec::new();
    let mut lat_rng = rng_for(seed, "locards/net");
    for (serial, mut acc) in clients {
        let sensor = &sensors[&serial];
        let latency = lat_rng.gen_range(0.01f64..0.05);
        acc.lines.sort_by(|a, b| a.0.total_cmp(&b.0));
        let handshake = format!(
            "{{\"version\":3,\"user\":\"{}\",\"compress\":[\"none\"],\"lat\":{},\"lon\":{},\"alt\":{},\"clock_type\":\"dump1090\",\"return_results\":false}}\n",
            sensor.user, sensor.geo.lat_deg, sensor.geo.lon_deg, sensor.geo.alt_m
        );
        let mut cw = w.client_writer(&sensor.user)?;
        cw.record(0, REC_CONNECT, handshake.as_bytes())?;
        for (t, line) in &acc.lines {
            let nanos = (((t + latency) * 1e9).round()).max(0.0) as u64;
            cw.record(nanos, REC_C2S, format!("{line}\n").as_bytes())?;
        }
        cw.finish()?;
        entries.push(ClientEntry {
            id: sensor.user.clone(),
            file: format!("clients/{}.mbc.zst", sensor.user),
            clock_type: "dump1090".into(),
            compress: "none".into(),
            lat: sensor.geo.lat_deg,
            lon: sensor.geo.lon_deg,
            alt_m: sensor.geo.alt_m,
        });
    }
    w.write_manifest(&Manifest {
        format: "mbc".into(),
        version: 1,
        name: "locards".into(),
        seed,
        duration_s,
        scenario_sha256: sha,
        clients: entries,
    })?;
    println!("import: capture at {}", out.display());
    Ok(())
}
