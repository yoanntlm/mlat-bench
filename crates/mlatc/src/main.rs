//! mlatc — an MLAT client for the mlat-server protocol.
//!
//! Reads Mode S frames from a Beast TCP input (readsb port 30005), pairs
//! and forwards them to an MLAT server (mlatd or mlat-server), and serves
//! returned positions on an optional SBS listener. Flag names follow
//! mlat-client. Wiring lives in this file; protocol behavior in engine.rs;
//! the Beast format in beast.rs.

mod beast;
mod engine;

use anyhow::{bail, Context, Result};
use clap::Parser;
use engine::Engine;
use mb_proto::framing::{ZlibFrameDecoder, ZlibFrameEncoder};
use mb_proto::{ClientMsg, ClockType, Compress, Handshake, ServerMsg};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{sleep, Duration};

#[derive(Parser)]
#[command(name = "mlatc", version, about)]
struct Cli {
    /// Input clock type: dump1090, beast, radarcape_12mhz, or auto
    /// (= dump1090). radarcape_gps and sbs inputs are not implemented.
    #[arg(long, default_value = "auto")]
    input_type: String,
    /// Beast input to read, host:port (readsb serves this on 30005).
    #[arg(long)]
    input_connect: String,
    /// MLAT server, host:port.
    #[arg(long)]
    server: String,
    /// Receiver name sent to the server.
    #[arg(long)]
    user: String,
    /// Receiver position. The server solves with it; use the antenna's
    /// real coordinates.
    #[arg(long, allow_negative_numbers = true)]
    lat: f64,
    #[arg(long, allow_negative_numbers = true)]
    lon: f64,
    /// Receiver altitude: meters by default, or with an explicit unit
    /// ("65m", "213ft") as mlat-client accepts.
    #[arg(long, allow_negative_numbers = true)]
    alt: String,
    #[arg(long)]
    uuid: Option<String>,
    /// Result output: "none" or "basestation,listen,PORT". Repeatable.
    #[arg(long)]
    results: Vec<String>,
}

enum Ev {
    Receptions(Vec<beast::Reception>),
    InputUp,
    InputDown,
    Start(Vec<String>),
    Stop(Vec<String>),
    ServerReset,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let clock_type = match cli.input_type.as_str() {
        "auto" | "dump1090" => ClockType::Dump1090,
        "beast" => ClockType::Beast,
        "radarcape_12mhz" => ClockType::Radarcape12Mhz,
        other => bail!("--input-type {other} is not implemented (12 MHz inputs only)"),
    };

    let alt_m = parse_alt(&cli.alt)?;
    let (sbs_tx, _) = broadcast::channel::<String>(256);
    let mut want_results = false;
    for spec in &cli.results {
        match spec.split(',').collect::<Vec<_>>().as_slice() {
            ["none"] => {}
            ["basestation", "listen", port] => {
                want_results = true;
                spawn_sbs_listener(format!("0.0.0.0:{port}"), sbs_tx.clone()).await?;
            }
            _ => bail!("--results {spec}: only \"none\" and \"basestation,listen,PORT\""),
        }
    }

    let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(1024);
    let (up_tx, mut up_rx) = mpsc::channel::<ClientMsg>(4096);

    // Input task: read Beast bytes, decode, forward. Owns its reconnects.
    {
        let ev = ev_tx.clone();
        let addr = cli.input_connect.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(mut sock) = TcpStream::connect(&addr).await {
                    println!("mlatc: input connected ({addr})");
                    let _ = ev.send(Ev::InputUp).await;
                    let mut dec = beast::BeastDecoder::default();
                    let mut buf = [0u8; 8192];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let rs = dec.feed(&buf[..n]);
                                if !rs.is_empty() && ev.send(Ev::Receptions(rs)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    println!("mlatc: input lost, retrying");
                    let _ = ev.send(Ev::InputDown).await;
                }
                sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // Engine task: single owner of the protocol state.
    {
        let user = cli.user.clone();
        tokio::spawn(async move {
            let mut eng = Engine::new(&user, Instant::now());
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                let msgs = tokio::select! {
                    _ = tick.tick() => eng.tick(Instant::now()),
                    ev = ev_rx.recv() => {
                        let Some(ev) = ev else { return };
                        match ev {
                            Ev::Receptions(rs) => {
                                let now = Instant::now();
                                let mut out = Vec::new();
                                for r in &rs {
                                    out.extend(eng.on_reception(r, now));
                                }
                                out
                            }
                            Ev::InputUp => {
                                eng.input_reset();
                                vec![ClientMsg::InputConnected("input connected".into())]
                            }
                            Ev::InputDown => {
                                eng.input_reset();
                                vec![ClientMsg::InputDisconnected("input disconnected".into())]
                            }
                            Ev::Start(v) => { eng.start_sending(&v); Vec::new() }
                            Ev::Stop(v) => { eng.stop_sending(&v); Vec::new() }
                            Ev::ServerReset => { eng.server_reset(); Vec::new() }
                        }
                    }
                };
                for m in msgs {
                    if up_tx.send(m).await.is_err() {
                        return;
                    }
                }
            }
        });
    }

    // Server loop: connect, handshake, pump; reconnect with backoff.
    let mut backoff_s = 5.0f64;
    loop {
        match server_session(
            &cli,
            alt_m,
            clock_type,
            want_results,
            &mut up_rx,
            &sbs_tx,
            &ev_tx,
        )
        .await
        {
            Ok(()) => backoff_s = 5.0,
            Err(e) => eprintln!("mlatc: server session ended: {e:#}"),
        }
        let _ = ev_tx.send(Ev::ServerReset).await;
        while up_rx.try_recv().is_ok() {} // stale traffic is useless
        sleep(Duration::from_secs_f64(backoff_s)).await;
        backoff_s = (backoff_s * 2.0).min(60.0);
    }
}

async fn server_session(
    cli: &Cli,
    alt_m: f64,
    clock_type: ClockType,
    want_results: bool,
    up_rx: &mut mpsc::Receiver<ClientMsg>,
    sbs_tx: &broadcast::Sender<String>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<()> {
    let sock = TcpStream::connect(&cli.server)
        .await
        .with_context(|| format!("connect {}", cli.server))?;
    sock.set_nodelay(true)?;
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);

    let hs = Handshake {
        version: 3,
        user: cli.user.clone(),
        uuid: cli.uuid.clone(),
        compress: vec![Compress::Zlib2, Compress::None],
        lat: cli.lat,
        lon: cli.lon,
        alt: alt_m,
        clock_type,
        return_results: Some(want_results),
        return_result_format: None,
        client_version: Some(format!("mlatc {}", env!("CARGO_PKG_VERSION"))),
        selective_traffic: Some(true),
        heartbeat: Some(true),
    };
    wr.write_all(&hs.to_line()).await?;
    // The handshake reply is always a plain line; compression starts after.
    let mut first = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), rd.read_until(b'\n', &mut first))
        .await
        .context("no handshake reply within 15 s")??;
    if first.is_empty() {
        bail!("server closed before replying");
    }
    let compress = match ServerMsg::parse_handshake_reply(&first)? {
        ServerMsg::HandshakeAccept { compress, motd, .. } => {
            println!(
                "mlatc: connected to {} ({compress:?}){}",
                cli.server,
                motd.map(|m| format!(" — {m}")).unwrap_or_default()
            );
            compress
        }
        ServerMsg::Deny(v) => bail!("server denied: {v}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    };

    let mut enc = match compress {
        Compress::None => None,
        Compress::Zlib2 | Compress::Zlib => Some(ZlibFrameEncoder::new()),
    };
    // Downlink asymmetry (jsonclient.py _compression_methods): zlib2
    // compresses server->client with the same framing; zlib and none send
    // plain lines.
    let mut down = Downlink::new(rd, matches!(compress, Compress::Zlib2));
    // zlib2 batches ~1 s of lines per frame; the wire length field caps the
    // uncompressed batch.
    let mut batch: Vec<u8> = Vec::new();
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            msg = up_rx.recv() => {
                let Some(msg) = msg else { bail!("engine gone") };
                let line = msg.to_line();
                match &mut enc {
                    None => wr.write_all(&line).await?,
                    Some(_) => {
                        if batch.len() + line.len() > 32 * 1024 {
                            send_batch(&mut wr, enc.as_mut().unwrap(), &mut batch).await?;
                        }
                        batch.extend_from_slice(&line);
                    }
                }
            }
            _ = flush.tick() => {
                if let Some(e) = enc.as_mut() {
                    send_batch(&mut wr, e, &mut batch).await?;
                }
            }
            _ = heartbeat.tick() => {
                let line = ClientMsg::heartbeat_now().to_line();
                match &mut enc {
                    None => wr.write_all(&line).await?,
                    Some(_) => batch.extend_from_slice(&line),
                }
            }
            line = down.next_line() => {
                let Some(line) = line? else { bail!("server closed the connection") };
                match ServerMsg::parse_line(&line)? {
                    ServerMsg::StartSending(v) => { let _ = ev_tx.send(Ev::Start(v)).await; }
                    ServerMsg::StopSending(v) => { let _ = ev_tx.send(Ev::Stop(v)).await; }
                    ServerMsg::Result(r) => {
                        if let Some(l) = sbs_line(&r) {
                            let _ = sbs_tx.send(l);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

struct Downlink {
    rd: BufReader<tokio::net::tcp::OwnedReadHalf>,
    dec: Option<ZlibFrameDecoder>,
    queue: std::collections::VecDeque<Vec<u8>>,
}

impl Downlink {
    fn new(rd: BufReader<tokio::net::tcp::OwnedReadHalf>, framed: bool) -> Self {
        Downlink {
            rd,
            dec: framed.then(ZlibFrameDecoder::new),
            queue: Default::default(),
        }
    }

    async fn next_line(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(l) = self.queue.pop_front() {
            return Ok(Some(l));
        }
        match &mut self.dec {
            None => {
                let mut line = Vec::new();
                let n = self.rd.read_until(b'\n', &mut line).await?;
                Ok((n > 0).then_some(line))
            }
            Some(dec) => {
                let mut head = [0u8; 2];
                if self.rd.read_exact(&mut head).await.is_err() {
                    return Ok(None);
                }
                let len = u16::from_be_bytes(head) as usize;
                let mut frame = vec![0u8; 2 + len];
                frame[..2].copy_from_slice(&head);
                self.rd.read_exact(&mut frame[2..]).await?;
                let bytes = dec.decode_frame(&frame)?;
                for l in bytes.split(|&b| b == b'\n') {
                    if !l.is_empty() {
                        self.queue.push_back(l.to_vec());
                    }
                }
                // A frame always holds at least one line; recurse-free retry
                // for the empty-frame edge.
                if let Some(l) = self.queue.pop_front() {
                    Ok(Some(l))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

async fn send_batch(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    enc: &mut ZlibFrameEncoder,
    batch: &mut Vec<u8>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let frame = enc.encode_frame(batch)?;
    batch.clear();
    wr.write_all(&frame).await?;
    Ok(())
}

async fn spawn_sbs_listener(addr: String, tx: broadcast::Sender<String>) -> Result<()> {
    let l = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    println!("mlatc: SBS results on {addr}");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            let mut rx = tx.subscribe();
            tokio::spawn(async move {
                while let Ok(line) = rx.recv().await {
                    if sock.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(())
}

/// Altitude with mlat-client's unit suffixes: bare number or "m" =
/// meters, "ft" = feet.
fn parse_alt(s: &str) -> Result<f64> {
    let t = s.trim();
    let (num, scale) = if let Some(n) = t.strip_suffix("ft") {
        (n, 0.3048)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 1.0)
    } else {
        (t, 1.0)
    };
    num.trim()
        .parse::<f64>()
        .map(|v| v * scale)
        .map_err(|_| anyhow::anyhow!("--alt {s}: not a number with optional m/ft suffix"))
}

/// Result message ("old" format) → one SBS MSG,3 line.
fn sbs_line(r: &serde_json::Value) -> Option<String> {
    let addr = r.get("addr")?.as_str()?.to_uppercase();
    let lat = r.get("lat")?.as_f64()?;
    let lon = r.get("lon")?.as_f64()?;
    let alt = r.get("alt").and_then(|a| a.as_f64()).unwrap_or(0.0);
    Some(format!(
        "MSG,3,1,1,{addr},1,,,,,,{alt:.0},,,{lat:.5},{lon:.5},,,,,,0\r\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_alt;

    #[test]
    fn alt_suffixes() {
        assert_eq!(parse_alt("65").unwrap(), 65.0);
        assert_eq!(parse_alt("65m").unwrap(), 65.0);
        assert!((parse_alt("100ft").unwrap() - 30.48).abs() < 1e-9);
        assert!(parse_alt("65x").is_err());
    }
}
