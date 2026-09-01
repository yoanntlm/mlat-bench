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
    /// MLAT server, host:port. Repeatable: one client feeds several
    /// servers from a single Beast decode and one aircraft table, each
    /// server with its own selective-traffic session.
    #[arg(long, required = true)]
    server: Vec<String>,
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
    /// Write the stats file mlat-client writes: server-pushed per-receiver
    /// stats plus a one-hour sync-quality history. Updated atomically.
    #[arg(long)]
    stats_json: Option<std::path::PathBuf>,
    /// Result output, repeatable: "none", "basestation,listen,PORT",
    /// "beast,listen,PORT", or "beast,connect,HOST:PORT" (what ultrafeeder
    /// uses to feed MLAT positions back into readsb).
    #[arg(long)]
    results: Vec<String>,
}

enum Ev {
    Receptions(Vec<beast::Reception>),
    InputUp,
    InputDown,
    Start(usize, Vec<String>),
    Stop(usize, Vec<String>),
    ServerReset(usize),
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
    let (res_tx, _) = broadcast::channel::<ResultPos>(256);
    let mut want_results = false;
    for spec in &cli.results {
        match spec.split(',').collect::<Vec<_>>().as_slice() {
            ["none"] => {}
            ["basestation", "listen", port] => {
                want_results = true;
                spawn_sbs_listener(format!("0.0.0.0:{port}"), res_tx.clone()).await?;
            }
            ["beast", "listen", port] => {
                want_results = true;
                spawn_beast_listener(format!("0.0.0.0:{port}"), res_tx.clone()).await?;
            }
            ["beast", "connect", addr] => {
                want_results = true;
                spawn_beast_connector(addr.to_string(), res_tx.clone());
            }
            _ => bail!(
                "--results {spec}: none, basestation,listen,PORT, beast,listen,PORT, or beast,connect,HOST:PORT"
            ),
        }
    }

    let n_servers = cli.server.len();
    let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(1024);
    let mut up_txs = Vec::new();
    let mut up_rxs = Vec::new();
    for _ in 0..n_servers {
        let (tx, rx) = mpsc::channel::<ClientMsg>(4096);
        up_txs.push(tx);
        up_rxs.push(rx);
    }

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

    // Engine task: single owner of the protocol state, fanning out to the
    // per-server uplinks.
    {
        let user = cli.user.clone();
        tokio::spawn(async move {
            let mut eng = Engine::new(&user, Instant::now(), n_servers);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                let msgs: Vec<engine::Outbound> = tokio::select! {
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
                                (0..n_servers)
                                    .map(|s| (s, ClientMsg::InputConnected("input connected".into())))
                                    .collect()
                            }
                            Ev::InputDown => {
                                eng.input_reset();
                                (0..n_servers)
                                    .map(|s| (s, ClientMsg::InputDisconnected("input disconnected".into())))
                                    .collect()
                            }
                            Ev::Start(srv, v) => { eng.start_sending(srv, &v); Vec::new() }
                            Ev::Stop(srv, v) => { eng.stop_sending(srv, &v); Vec::new() }
                            Ev::ServerReset(srv) => { eng.server_reset(srv); Vec::new() }
                        }
                    }
                };
                for (srv, m) in msgs {
                    // Never block on one server's queue: a stalled server
                    // must lose its own traffic, not stall the others
                    // (the head-of-line isolation the process-per-server
                    // setup had by accident). MLAT traffic is continuous;
                    // dropped messages cost that server a little sync.
                    match up_txs[srv].try_send(m) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        });
    }

    // One session loop per server: connect, handshake, pump; reconnect
    // with backoff, independently of the other servers.
    let cli = std::sync::Arc::new(cli);
    for (idx, mut up_rx) in up_rxs.into_iter().enumerate() {
        let cli = cli.clone();
        let res_tx = res_tx.clone();
        let ev_tx = ev_tx.clone();
        let stats_path = stats_path_for(&cli, idx);
        tokio::spawn(async move {
            let addr = cli.server[idx].clone();
            let mut backoff_s = 5.0f64;
            loop {
                match server_session(
                    &cli,
                    idx,
                    &addr,
                    stats_path.as_deref(),
                    alt_m,
                    clock_type,
                    want_results,
                    &mut up_rx,
                    &res_tx,
                    &ev_tx,
                )
                .await
                {
                    Ok(()) => backoff_s = 5.0,
                    Err(e) => eprintln!("mlatc: [{addr}] session ended: {e:#}"),
                }
                let _ = ev_tx.send(Ev::ServerReset(idx)).await;
                while up_rx.try_recv().is_ok() {} // stale traffic is useless
                sleep(Duration::from_secs_f64(backoff_s)).await;
                backoff_s = (backoff_s * 2.0).min(60.0);
            }
        });
    }
    std::future::pending::<()>().await;
    unreachable!()
}

/// The stats file for one server: the given path as-is for a single
/// server; with the server address folded into the name when feeding
/// several, so each session writes its own file.
fn stats_path_for(cli: &Cli, idx: usize) -> Option<std::path::PathBuf> {
    let base = cli.stats_json.as_ref()?;
    if cli.server.len() == 1 {
        return Some(base.clone());
    }
    let tag: String = cli.server[idx]
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("stats");
    Some(base.with_file_name(format!("{stem}-{tag}.json")))
}

#[allow(clippy::too_many_arguments)]
async fn server_session(
    cli: &Cli,
    idx: usize,
    addr: &str,
    stats_path: Option<&std::path::Path>,
    alt_m: f64,
    clock_type: ClockType,
    want_results: bool,
    up_rx: &mut mpsc::Receiver<ClientMsg>,
    res_tx: &broadcast::Sender<ResultPos>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<()> {
    let sock = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
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
        return_stats: stats_path.map(|_| true),
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
                "mlatc: connected to {addr} ({compress:?}){}",
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
    let mut stats = StatsFile::default();
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
                    ServerMsg::StartSending(v) => { let _ = ev_tx.send(Ev::Start(idx, v)).await; }
                    ServerMsg::StopSending(v) => { let _ = ev_tx.send(Ev::Stop(idx, v)).await; }
                    ServerMsg::Result(r) => {
                        if let Some(p) = ResultPos::from_json(&r) {
                            let _ = res_tx.send(p);
                        }
                    }
                    ServerMsg::Stats(v) => {
                        if let Some(path) = stats_path {
                            stats.push(v);
                            stats.write(path);
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

/// One returned MLAT position, fanned out to every result sink.
#[derive(Clone, Copy, Debug)]
struct ResultPos {
    addr: u32,
    lat: f64,
    lon: f64,
    alt_ft: i32,
}

impl ResultPos {
    /// From a result message in the "old" format.
    fn from_json(r: &serde_json::Value) -> Option<ResultPos> {
        Some(ResultPos {
            addr: u32::from_str_radix(r.get("addr")?.as_str()?, 16).ok()?,
            lat: r.get("lat")?.as_f64()?,
            lon: r.get("lon")?.as_f64()?,
            alt_ft: r.get("alt").and_then(|a| a.as_f64()).unwrap_or(0.0) as i32,
        })
    }

    fn sbs_line(&self) -> String {
        format!(
            "MSG,3,1,1,{:06X},1,,,,,,{},,,{:.5},{:.5},,,,,,0\r\n",
            self.addr, self.alt_ft, self.lat, self.lon
        )
    }

    /// The synthetic DF18 pair in Beast framing with the magic MLAT
    /// timestamp, exactly as mlat-client's Beast results output builds it.
    fn beast_bytes(&self) -> Option<Vec<u8>> {
        let pair = mb_modes::frames::df18_position_pair(
            mb_modes::Icao(self.addr),
            self.alt_ft,
            self.lat,
            self.lon,
        )?;
        let mut out = Vec::with_capacity(2 * 40);
        for f in &pair {
            out.extend_from_slice(b"\x1A3\xFF\x00MLAT\x00");
            for &b in f.iter() {
                out.push(b);
                if b == 0x1A {
                    out.push(0x1A);
                }
            }
        }
        Some(out)
    }
}

/// The --stats-json file, as mlat-client builds it: the latest server
/// push merged with a one-hour history of sync quality.
#[derive(Default)]
struct StatsFile {
    latest: serde_json::Map<String, serde_json::Value>,
    /// (unix time, state): 1 good sync, 0 no sync, -1 bad sync.
    history: std::collections::VecDeque<(u64, i8)>,
    last_bad_sync: u64,
}

impl StatsFile {
    fn push(&mut self, v: serde_json::Value) {
        let now = unix_now();
        let obj = v.as_object().cloned().unwrap_or_default();
        let get = |k: &str| obj.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let state = if get("bad_sync_timeout") > 0.0 {
            self.last_bad_sync = now;
            -1
        } else if get("peer_count") > 0.0 {
            1
        } else {
            0
        };
        self.history.push_back((now, state));
        while self
            .history
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > 3600)
        {
            self.history.pop_front();
        }
        self.latest = obj;
    }

    fn write(&self, path: &std::path::Path) {
        let mut out = self.latest.clone();
        out.insert("now".into(), unix_now().into());
        let n = self.history.len() as f64;
        let (good, bad) = self.history.iter().fold((0.0, 0.0), |(g, b), (_, st)| {
            (g + f64::from(*st == 1), b + f64::from(*st == -1))
        });
        let pct = |x: f64| {
            if n > 0.0 {
                serde_json::json!((x / n * 100.0).round())
            } else {
                serde_json::json!(-1)
            }
        };
        out.insert("good_sync_percentage_last_hour".into(), pct(good));
        out.insert("bad_sync_percentage_last_hour".into(), pct(bad));
        out.insert("last_bad_sync".into(), self.last_bad_sync.into());
        let tmp = path.with_extension("tmp");
        let Ok(f) = std::fs::File::create(&tmp) else {
            return;
        };
        if serde_json::to_writer_pretty(std::io::BufWriter::new(f), &serde_json::Value::Object(out))
            .is_ok()
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// mlat-client's Beast keepalive frame, sent after 30 s of idle.
const BEAST_KEEPALIVE: &[u8] = b"\x1A1\x00\x00\x00\x00\x00\x00\x00\x00\x00";

async fn spawn_sbs_listener(addr: String, tx: broadcast::Sender<ResultPos>) -> Result<()> {
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
                while let Ok(p) = rx.recv().await {
                    if sock.write_all(p.sbs_line().as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(())
}

async fn spawn_beast_listener(addr: String, tx: broadcast::Sender<ResultPos>) -> Result<()> {
    let l = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    println!("mlatc: Beast results on {addr}");
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(beast_pump(sock, tx.subscribe()));
        }
    });
    Ok(())
}

fn spawn_beast_connector(addr: String, tx: broadcast::Sender<ResultPos>) {
    tokio::spawn(async move {
        loop {
            if let Ok(sock) = TcpStream::connect(&addr).await {
                println!("mlatc: Beast results connected ({addr})");
                beast_pump(sock, tx.subscribe()).await;
                println!("mlatc: Beast results lost ({addr}), retrying");
            }
            sleep(Duration::from_secs(15)).await;
        }
    });
}

async fn beast_pump(mut sock: TcpStream, mut rx: broadcast::Receiver<ResultPos>) {
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.tick().await;
    loop {
        tokio::select! {
            p = rx.recv() => {
                let Ok(p) = p else { return };
                let Some(bytes) = p.beast_bytes() else { continue };
                keepalive.reset();
                if sock.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            _ = keepalive.tick() => {
                if sock.write_all(BEAST_KEEPALIVE).await.is_err() {
                    return;
                }
            }
        }
    }
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
