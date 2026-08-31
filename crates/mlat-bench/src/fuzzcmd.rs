//! Coordinate fuzzing for shareable recordings.
//!
//! Receiver coordinates identify homes (capture-format.md, Privacy). This
//! command copies a capture and moves each receiver's reported position by
//! a seeded draw inside a radius: the manifest entry and the lat/lon/alt in
//! each recorded handshake line. Everything else is copied unchanged, so
//! the copy still replays.
//!
//! The offsets contradict the timestamps inside the payloads, so solve
//! accuracy against a fuzzed capture degrades with the radius. A fuzzed
//! capture is for sharing and protocol work, not for accuracy scoring.

use anyhow::{Context, Result};
use mb_capture::{CaptureReader, CaptureWriter};
use rand::Rng;
use std::path::Path;

pub fn fuzz(capture: &Path, out: &Path, radius_km: f64, seed: u64) -> Result<()> {
    let r = CaptureReader::open(capture).context("open capture")?;
    let w = CaptureWriter::create(out).context("create output capture")?;

    if capture.join("scenario.toml").exists() {
        println!(
            "note: this capture has a scenario (synthetic); its coordinates are already synthetic"
        );
        w.write_scenario_toml(&r.scenario_toml()?)?;
    }
    if capture.join("truth.jsonl.zst").exists() {
        let truth = r.truth()?;
        w.write_truth(truth.iter())?;
    }
    if capture.join("audibility.jsonl.zst").exists() {
        w.write_audibility(r.audibility_raw()?.into_iter())?;
    }

    let mut manifest = r.manifest.clone();
    manifest.name = format!("{}-fuzzed", manifest.name);
    for entry in &mut manifest.clients {
        let mut rng = mb_core::rng_for(seed, &format!("fuzz/{}", entry.id));
        // Uniform draw in a disk of the given radius.
        let theta: f64 = rng.gen_range(0.0..std::f64::consts::TAU);
        let dist_m = radius_km * 1000.0 * rng.gen_range(0.0f64..1.0).sqrt();
        let dlat = dist_m * theta.cos() / 111_320.0;
        let dlon = dist_m * theta.sin() / (111_320.0 * entry.lat.to_radians().cos().max(0.05));
        let dalt: f64 = rng.gen_range(-30.0..30.0);
        let (lat, lon, alt) = (entry.lat + dlat, entry.lon + dlon, entry.alt_m + dalt);

        let mut cw = w.client_writer(&entry.id)?;
        for rec in r.client_records(entry)? {
            let rec = rec?;
            if rec.kind == 0x03 {
                // The recorded handshake line: rewrite lat/lon/alt, keep the
                // rest of the JSON untouched.
                let mut v: serde_json::Value =
                    serde_json::from_slice(&rec.payload).context("handshake JSON")?;
                let obj = v.as_object_mut().context("handshake not an object")?;
                obj.insert("lat".into(), serde_json::json!(round5(lat)));
                obj.insert("lon".into(), serde_json::json!(round5(lon)));
                obj.insert("alt".into(), serde_json::json!(alt.round()));
                let mut line = serde_json::to_vec(&v)?;
                line.push(b'\n');
                cw.record(rec.t_nanos, rec.kind, &line)?;
            } else {
                cw.record(rec.t_nanos, rec.kind, &rec.payload)?;
            }
        }
        cw.finish()?;

        entry.lat = round5(lat);
        entry.lon = round5(lon);
        entry.alt_m = alt.round();
    }
    w.write_manifest(&manifest)?;
    println!(
        "fuzz: {} clients moved by up to {radius_km} km (seed {seed}) -> {}",
        manifest.clients.len(),
        out.display()
    );
    Ok(())
}

/// Five decimals ≈ 1 m: enough precision to replay, no more.
fn round5(x: f64) -> f64 {
    (x * 1e5).round() / 1e5
}
