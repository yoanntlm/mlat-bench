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
use state::{ReceiverInfo, State};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let state = Arc::new(Mutex::new(State::new(&cli.write_csv, cli.time_scale)?));

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

    let listener = TcpListener::bind(&cli.listen)
        .await
        .with_context(|| format!("bind {}", cli.listen))?;
    println!("mb-server: listening on {}", cli.listen);
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
    let mut lines = BufReader::new(rd).lines();

    // ---- handshake -------------------------------------------------------
    let hs_line = lines
        .next_line()
        .await?
        .context("closed before handshake")?;
    let hs: serde_json::Value = serde_json::from_str(&hs_line).context("handshake not JSON")?;
    let offers_none = hs["compress"]
        .as_array()
        .map(|a| a.iter().any(|c| c == "none"))
        .unwrap_or(false);
    if !offers_none {
        wr.write_all(
            b"{\"deny\":[\"mb-server v0 speaks compress=none only\"],\"reconnect_in\":300}\n",
        )
        .await?;
        anyhow::bail!("client did not offer compress=none");
    }
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
    wr.write_all(
        b"{\"compress\":\"none\",\"reconnect_in\":300,\"selective_traffic\":false,\
          \"heartbeat\":true,\"return_results\":false,\"rate_reports\":false,\
          \"motd\":\"mlat-bench candidate mb-server\"}\n",
    )
    .await?;
    println!("mb-server: {user} connected ({clock_type})");

    // ---- message loop ----------------------------------------------------
    let mut hb = tokio::time::interval(hb_real);
    hb.tick().await; // consume immediate first tick
    loop {
        tokio::select! {
            _ = hb.tick() => {
                let st = state.lock().unwrap().scaled_now();
                let line = format!("{{\"heartbeat\":{{\"server_time\":{st:.3}}}}}\n");
                wr.write_all(line.as_bytes()).await?;
            }
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let mut s = state.lock().unwrap();
                if let Some(sy) = v.get("sync") {
                    let (Some(et), Some(ot), Some(em), Some(om)) = (
                        sy["et"].as_f64(), sy["ot"].as_f64(),
                        sy["em"].as_str(), sy["om"].as_str(),
                    ) else { continue };
                    s.on_sync(rx, et, ot, em, om);
                } else if let Some(ml) = v.get("mlat") {
                    let (Some(t), Some(m)) = (ml["t"].as_f64(), ml["m"].as_str()) else { continue };
                    s.on_mlat(rx, t, m);
                } else if v.get("clock_reset").is_some() || v.get("clock_jump").is_some() {
                    s.clock_reset(rx);
                }
                // seen/lost/heartbeat/rate_report/input_*: no state needed in v0.
            }
        }
    }
    println!("mb-server: {user} disconnected");
    Ok(())
}
