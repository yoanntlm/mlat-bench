//! mb-server — a candidate MLAT server, benched by mlat-bench.
//!
//! v0 scope, on purpose: mlat-client protocol over `compress: none`,
//! pairwise clock sync (windowed linear fit, star topology to one reference
//! receiver, GPS-preferred), content-keyed message grouping, fixed-altitude
//! Gauss-Newton TDOA. Everything it doesn't do (zlib framing, sync graph
//! traversal, Kalman track filtering, result return to clients) is a
//! deliberate gap the bench will price.
//!
//! Bench it:
//!   cargo run -p mb-server -- --write-csv /tmp/cand.csv --time-scale 10 --group-window-ms 90
//!   cargo run -p mlat-bench -- replay <capture> --speed 10 \
//!       --addr 127.0.0.1:40160 --results-csv /tmp/cand.csv

mod clocksync;
mod solve;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use mb_proto::framing::ZlibFrameDecoder;
use state::{ReceiverInfo, State};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;

#[derive(Parser)]
#[command(name = "mb-server", version, about)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:40160")]
    listen: String,
    /// Oracle-format results CSV output (the bench's scoring input).
    #[arg(long)]
    write_csv: std::path::PathBuf,
    /// Message-grouping window, milliseconds of REAL time. At an accelerated
    /// replay divide the usual 900 by the speed factor.
    #[arg(long, default_value_t = 900)]
    group_window_ms: u64,
    /// Output/heartbeat clock runs this many times real speed — match the
    /// bench's --speed so scoring maps time correctly.
    #[arg(long, default_value_t = 1.0)]
    time_scale: f64,
    /// Oracle-compatible alias for --listen ([host:]port accepted).
    #[arg(long)]
    client_listen: Option<String>,
    /// SBS/BaseStation output listener (what readsb ingests), e.g.
    /// 127.0.0.1:40161. Oracle flag name kept for drop-in swaps.
    #[arg(long)]
    basestation_listen: Option<String>,
    /// Work dir: sync.json is written here every 15 s in the oracle's shape
    /// so existing monitoring keeps working.
    #[arg(long)]
    work_dir: Option<std::path::PathBuf>,
    /// Multilaterate DF17 (ADS-B) frames too and score each fix against the
    /// aircraft's own broadcast position — real-world accuracy without
    /// external truth. Rows: t,icao,err_m,est_m,n → this CSV.
    #[arg(long)]
    self_truth_csv: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mlat_adsb = cli.self_truth_csv.is_some();
    let state = Arc::new(Mutex::new(State::new(
        &cli.write_csv,
        cli.time_scale,
        cli.self_truth_csv.as_deref(),
        mlat_adsb,
    )?));
    let listen = cli
        .client_listen
        .as_deref()
        .map(|s| {
            // Accept the oracle's bare-port form.
            if s.contains(':') {
                s.to_string()
            } else {
                format!("0.0.0.0:{s}")
            }
        })
        .unwrap_or_else(|| cli.listen.clone());

    // Sweeper: solves aged groups.
    {
        let state = state.clone();
        let window = Duration::from_millis(cli.group_window_ms);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            loop {
                tick.tick().await;
                state.lock().unwrap().sweep(window);
            }
        });
    }
    // Stats line every 10 s so a run is observable.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                let s = state.lock().unwrap();
                println!(
                    "mb-server: rx={} sync_obs={} solved={} rejected={}",
                    s.receivers.len(),
                    s.stats_sync_obs,
                    s.stats_solved,
                    s.stats_rejected
                );
            }
        });
    }

    // SBS output listener: each consumer gets the broadcast fix stream.
    if let Some(addr) = cli.basestation_listen.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            let Ok(l) = TcpListener::bind(&addr).await else {
                eprintln!("mb-server: cannot bind SBS listener {addr}");
                return;
            };
            println!("mb-server: SBS output on {addr}");
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    break;
                };
                let mut rx = state.lock().unwrap().publish.subscribe();
                tokio::spawn(async move {
                    while let Ok(p) = rx.recv().await {
                        if sock.write_all(p.sbs_line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }
    // sync.json export for existing monitoring.
    if let Some(dir) = cli.work_dir.clone() {
        let state = state.clone();
        let _ = std::fs::create_dir_all(&dir);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                let v = state.lock().unwrap().sync_json();
                let _ = std::fs::write(
                    dir.join("sync.json"),
                    serde_json::to_vec(&v).unwrap_or_default(),
                );
            }
        });
    }

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!("mb-server: listening on {listen}");
    let hb_real = Duration::from_secs_f64(30.0 / cli.time_scale);
    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, state, hb_real).await {
                eprintln!("mb-server: {peer}: {e:#}");
            }
        });
    }
}

async fn handle_client(
    stream: TcpStream,
    state: Arc<Mutex<State>>,
    hb_real: Duration,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // ---- handshake -------------------------------------------------------
    let mut hs_line = Vec::new();
    rd.read_until(b'\n', &mut hs_line).await?;
    if hs_line.is_empty() {
        anyhow::bail!("closed before handshake");
    }
    let hs: serde_json::Value = serde_json::from_slice(&hs_line).context("handshake not JSON")?;
    // Compression preference: none (cheapest for us), else zlib2, else zlib —
    // real feeders overwhelmingly offer zlib2 and denying them would be
    // instant disqualification in the field.
    let offered: Vec<String> = hs["compress"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let negotiated = ["none", "zlib2", "zlib"]
        .into_iter()
        .find(|m| offered.iter().any(|o| o == m));
    let Some(negotiated) = negotiated else {
        wr.write_all(b"{\"deny\":[\"no supported compression offered\"],\"reconnect_in\":300}\n")
            .await?;
        anyhow::bail!("client offered none of none/zlib/zlib2");
    };
    let (Some(lat), Some(lon), Some(alt)) =
        (hs["lat"].as_f64(), hs["lon"].as_f64(), hs["alt"].as_f64())
    else {
        wr.write_all(b"{\"deny\":[\"missing position\"],\"reconnect_in\":300}\n")
            .await?;
        anyhow::bail!("handshake missing position");
    };
    let clock_type = hs["clock_type"].as_str().unwrap_or("unknown").to_string();
    let freq_hz = match clock_type.as_str() {
        "radarcape_gps" | "radarcape" => 1e9,
        "sbs" => 20e6,
        _ => 12e6, // dump1090, beast, radarcape_12mhz, unknown
    };
    let user = hs["user"].as_str().unwrap_or("anon").to_string();
    let geo = mb_core::Geodetic {
        lat_deg: lat,
        lon_deg: lon,
        alt_m: alt,
    };
    let gps = clock_type.starts_with("radarcape_gps");
    let rx = state.lock().unwrap().add_receiver(ReceiverInfo {
        user: user.clone(),
        ecef: geo.to_ecef(),
        geo,
        freq_hz,
        gps,
        // Effective timing error: clock jitter + pair-model slack. GPS
        // clocks convert near-losslessly; free-running clocks carry the
        // sync model's noise on top of their own.
        jitter_s: if gps { 30e-9 } else { 150e-9 },
    });
    let wants_results = hs["return_results"].as_bool().unwrap_or(false);
    // Real mlat-client withholds ALL traffic until asked: selective traffic
    // is not optional politeness, it is the request channel (dress-rehearsal
    // finding: 5 real clients connected, decoded beast, sent nothing —
    // "0 of 8 ADS-B used"). Mimic the oracle: enable it, ask for rate
    // reports, and start_sending every aircraft the client reports.
    let reply = format!(
        "{{\"compress\":\"{negotiated}\",\"reconnect_in\":300,\"selective_traffic\":true,\
         \"heartbeat\":true,\"return_results\":{wants_results},\"rate_reports\":true,\
         \"motd\":\"mlat-bench candidate mb-server\"}}\n"
    );
    wr.write_all(reply.as_bytes()).await?;
    println!("mb-server: {user} connected ({clock_type}, {negotiated})");

    // Single writer task: heartbeats and (if subscribed) result messages
    // funnel through one mpsc so the socket has exactly one writer.
    let (tx_line, mut rx_line) = tokio::sync::mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(l) = rx_line.recv().await {
            if wr.write_all(l.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    if wants_results {
        let mut sub = state.lock().unwrap().publish.subscribe();
        let tx = tx_line.clone();
        tokio::spawn(async move {
            while let Ok(p) = sub.recv().await {
                if tx.send(p.result_line.clone()).await.is_err() {
                    break;
                }
            }
        });
    }

    // ---- message loop ----------------------------------------------------
    let mut hb = tokio::time::interval(hb_real);
    hb.tick().await; // consume immediate first tick
    let mut zdec = if negotiated == "none" {
        None
    } else {
        Some(ZlibFrameDecoder::new())
    };
    let mut requested: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        match &mut zdec {
            None => {
                let mut line = Vec::new();
                tokio::select! {
                    _ = hb.tick() => send_heartbeat(&state, &tx_line).await?,
                    r = rd.read_until(b'\n', &mut line) => {
                        if r? == 0 { break }
                        process_line_tx(&state, rx, &line, Some(&tx_line), &mut requested);
                    }
                }
            }
            Some(dec) => {
                // Framed: 2-byte BE length + zlib payload with persistent
                // dictionary state (mb-proto framing; the same code the
                // capture generator uses, exercised from the other side).
                let mut lenb = [0u8; 2];
                tokio::select! {
                    _ = hb.tick() => { send_heartbeat(&state, &tx_line).await?; continue }
                    r = rd.read_exact(&mut lenb) => {
                        if r.is_err() { break }
                    }
                }
                let want = u16::from_be_bytes(lenb) as usize;
                let mut payload = vec![0u8; 2 + want];
                payload[..2].copy_from_slice(&lenb);
                if rd.read_exact(&mut payload[2..]).await.is_err() {
                    break;
                }
                let Ok(chunk) = dec.decode_frame(&payload) else {
                    anyhow::bail!("zlib frame decode failed for {user}");
                };
                for line in chunk.split(|b| *b == b'\n') {
                    if !line.is_empty() {
                        process_line_tx(&state, rx, line, Some(&tx_line), &mut requested);
                    }
                }
            }
        }
    }
    drop(tx_line);
    let _ = writer.await;
    println!("mb-server: {user} disconnected");
    Ok(())
}

async fn send_heartbeat(
    state: &Arc<Mutex<State>>,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    let st = state.lock().unwrap().scaled_now();
    let line = format!("{{\"heartbeat\":{{\"server_time\":{st:.3}}}}}\n");
    let _ = tx.send(line).await;
    Ok(())
}

/// seen/rate_report trigger start_sending for aircraft not yet requested on
/// this connection — a real mlat-client withholds everything until asked.
fn process_line_tx(
    state: &Arc<Mutex<State>>,
    rx: usize,
    line: &[u8],
    tx: Option<&tokio::sync::mpsc::Sender<String>>,
    requested: &mut std::collections::HashSet<String>,
) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    // Aircraft the client offers (seen list / rate_report keys) → request
    // everything we have not requested yet.
    if let Some(tx) = tx {
        let mut fresh: Vec<String> = Vec::new();
        if let Some(seen) = v.get("seen").and_then(|x| x.as_array()) {
            for a in seen {
                if let Some(h) = a.as_str() {
                    if requested.insert(h.to_lowercase()) {
                        fresh.push(h.to_lowercase());
                    }
                }
            }
        }
        if let Some(rr) = v.get("rate_report").and_then(|x| x.as_object()) {
            for k in rr.keys() {
                if requested.insert(k.to_lowercase()) {
                    fresh.push(k.to_lowercase());
                }
            }
        }
        if !fresh.is_empty() {
            let msg = format!(
                "{{\"start_sending\":{}}}\n",
                serde_json::to_string(&fresh).unwrap_or_default()
            );
            let _ = tx.try_send(msg);
        }
    }
    let mut s = state.lock().unwrap();
    if let Some(sy) = v.get("sync") {
        let (Some(et), Some(ot), Some(em), Some(om)) = (
            sy["et"].as_f64(),
            sy["ot"].as_f64(),
            sy["em"].as_str(),
            sy["om"].as_str(),
        ) else {
            return;
        };
        s.on_sync(rx, et, ot, em, om);
    } else if let Some(ml) = v.get("mlat") {
        let (Some(t), Some(m)) = (ml["t"].as_f64(), ml["m"].as_str()) else {
            return;
        };
        s.on_mlat(rx, t, m);
    } else if v.get("clock_reset").is_some() || v.get("clock_jump").is_some() {
        s.clock_reset(rx);
    }
    // seen/lost/heartbeat/rate_report/input_*: no state needed yet.
}
