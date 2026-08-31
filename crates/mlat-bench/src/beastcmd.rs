//! `beast-serve`: replay one capture client's receptions as a Mode-S Beast
//! TCP stream — the input a REAL mlat-client eats. This lets the genuine
//! wiedehopf mlat-client sit in the loop (its handshake, its sync pairing,
//! its zlib2, its ssync/rate_report behavior) with no SDR anywhere.
//!
//! Reconstruction: the capture's client stream carries {sync:{et,em,ot,om}}
//! and {mlat:{t,m}} lines — each is one or two receptions (12 MHz-domain
//! counter + raw frame). Beast framing: 0x1a, 0x32 (7-byte short) or 0x33
//! (14-byte long), 6-byte big-endian timestamp at 12 MHz, signal byte,
//! frame; every 0x1a in the payload doubled.

use anyhow::{bail, Context, Result};
use mb_capture::{CaptureReader, REC_C2S};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::time::{sleep_until, Duration, Instant};

struct Reception {
    t_record_nanos: u64,
    counts_12mhz: u64,
    frame: Vec<u8>,
}

pub async fn beast_serve(capture: &Path, client_id: &str, listen: &str, speed: f64) -> Result<()> {
    let reader = CaptureReader::open(capture).map_err(|e| anyhow::anyhow!("{e}"))?;
    let entry = reader
        .manifest
        .clients
        .iter()
        .find(|c| c.id == client_id)
        .with_context(|| format!("client {client_id} not in capture"))?
        .clone();
    if entry.compress != "none" {
        bail!(
            "beast-serve needs an uncompressed capture stream (client is {})",
            entry.compress
        );
    }
    // Source clock → 12 MHz beast domain.
    let freq = match entry.clock_type.as_str() {
        "radarcape_gps" => 1e9,
        "sbs" => 20e6,
        _ => 12e6,
    };
    let scale = 12e6 / freq;

    let mut receptions: Vec<Reception> = Vec::new();
    for rec in reader
        .client_records(&entry)
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        let rec = rec.map_err(|e| anyhow::anyhow!("{e}"))?;
        if rec.kind != REC_C2S {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&rec.payload) else {
            continue;
        };
        let mut push = |t: Option<f64>, m: Option<&str>| {
            if let (Some(t), Some(m)) = (t, m) {
                if let Ok(frame) = hex::decode(m) {
                    receptions.push(Reception {
                        t_record_nanos: rec.t_nanos,
                        counts_12mhz: (t * scale).round().max(0.0) as u64 & 0xFFFF_FFFF_FFFF,
                        frame,
                    });
                }
            }
        };
        if let Some(sy) = v.get("sync") {
            push(sy["et"].as_f64(), sy["em"].as_str());
            push(sy["ot"].as_f64(), sy["om"].as_str());
        } else if let Some(ml) = v.get("mlat") {
            push(ml["t"].as_f64(), ml["m"].as_str());
        }
    }
    // Receptions from sync pairs arrive out of order (et before ot of the
    // same line but interleaved across lines) — beast consumers expect a
    // mostly-monotonic stream.
    receptions.sort_by_key(|r| r.counts_12mhz);
    println!(
        "beast-serve[{client_id}]: {} receptions, listening on {listen} (lat {:.4} lon {:.4} alt {:.0})",
        receptions.len(),
        entry.lat,
        entry.lon,
        entry.alt_m
    );

    let l = TcpListener::bind(listen).await?;
    let (mut sock, peer) = l.accept().await?;
    sock.set_nodelay(true)?;
    println!("beast-serve[{client_id}]: {peer} connected, streaming at {speed}x");
    let t0 = Instant::now() + Duration::from_secs(1);
    for r in &receptions {
        sleep_until(t0 + Duration::from_secs_f64(r.t_record_nanos as f64 / 1e9 / speed)).await;
        let mut buf = Vec::with_capacity(2 + 7 + r.frame.len() * 2);
        buf.push(0x1a);
        buf.push(if r.frame.len() == 7 { 0x32 } else { 0x33 });
        let ts = r.counts_12mhz.to_be_bytes();
        let mut payload = Vec::with_capacity(7 + r.frame.len());
        payload.extend_from_slice(&ts[2..8]); // 6-byte big-endian timestamp
        payload.push(0xA0); // signal level
        payload.extend_from_slice(&r.frame);
        for b in payload {
            buf.push(b);
            if b == 0x1a {
                buf.push(0x1a); // escape
            }
        }
        if sock.write_all(&buf).await.is_err() {
            break;
        }
    }
    println!("beast-serve[{client_id}]: stream complete");
    // Hold the socket so the client doesn't reconnect-loop mid-drain.
    sleep_until(Instant::now() + Duration::from_secs(30)).await;
    Ok(())
}
