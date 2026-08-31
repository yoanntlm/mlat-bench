//! `gen`: scenario TOML → capture directory. `inspect`: capture → summary.

use anyhow::{Context, Result};
use mb_capture::{CaptureReader, CaptureWriter, ClientEntry, Manifest, REC_C2S, REC_CONNECT};
use std::path::Path;

pub fn gen(scenario_path: &Path, out: &Path) -> Result<()> {
    let toml_text = std::fs::read_to_string(scenario_path)
        .with_context(|| format!("read {}", scenario_path.display()))?;
    let sc = mb_sim::Scenario::from_toml(&toml_text).context("parse scenario TOML")?;
    let cap = mb_sim::generate(&sc).map_err(|e| anyhow::anyhow!("generate: {e}"))?;

    let w = CaptureWriter::create(out).map_err(|e| anyhow::anyhow!("{e}"))?;
    let sha = w.write_scenario_toml(&toml_text)?;
    w.write_truth(cap.truth.iter())?;
    w.write_audibility(cap.audibility.iter())?;

    let mut entries = Vec::new();
    for (client, spec) in cap.clients.iter().zip(&sc.receivers) {
        let mut cw = w.client_writer(&client.id)?;
        cw.record(0, REC_CONNECT, &client.handshake_line)?;
        for r in &client.records {
            cw.record(r.t.0, REC_C2S, &r.bytes)?;
        }
        cw.finish()?;
        entries.push(ClientEntry {
            id: client.id.clone(),
            file: format!("clients/{}.mbc.zst", client.id),
            clock_type: client.clock_type.clone(),
            compress: client.compress.clone(),
            lat: spec.lat,
            lon: spec.lon,
            alt_m: spec.alt_m,
        });
        println!(
            "  {}: {} msgs ({} sync, {} mlat), {} records",
            client.id,
            client.message_count,
            client.sync_count,
            client.mlat_count,
            client.records.len() + 1,
        );
    }

    w.write_manifest(&Manifest {
        format: "mbc".into(),
        version: 1,
        name: sc.meta.name.clone(),
        seed: sc.meta.seed,
        duration_s: sc.meta.duration_s,
        scenario_sha256: sha,
        clients: entries,
    })?;
    println!(
        "capture written: {} ({} aircraft, {} receivers, {}s)",
        out.display(),
        sc.aircraft.len(),
        sc.receivers.len(),
        sc.meta.duration_s
    );
    Ok(())
}

pub fn inspect(capture: &Path) -> Result<()> {
    let r = CaptureReader::open(capture).map_err(|e| anyhow::anyhow!("{e}"))?;
    let m = &r.manifest;
    println!(
        "{} — seed {}, {}s, scenario sha {}…",
        m.name,
        m.seed,
        m.duration_s,
        &m.scenario_sha256[..12.min(m.scenario_sha256.len())]
    );
    println!("truth points: {}", r.truth().map(|t| t.len()).unwrap_or(0));
    for c in &m.clients {
        let mut counts = std::collections::BTreeMap::new();
        let mut bytes = 0usize;
        let mut last_t = 0u64;
        for rec in r.client_records(c)? {
            let rec = rec?;
            *counts.entry(rec.kind).or_insert(0u64) += 1;
            bytes += rec.payload.len();
            last_t = last_t.max(rec.t_nanos);
        }
        println!(
            "  {} [{} {}] records {:?}, {} payload bytes, last event at {:.1}s",
            c.id,
            c.clock_type,
            c.compress,
            counts,
            bytes,
            last_t as f64 / 1e9
        );
    }
    Ok(())
}
