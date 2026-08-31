//! `record`: a transparent TCP tap between mlat clients and a real server.
//!
//! Clients point at us, we pump bytes to the upstream unmodified and write
//! both directions into an MBC capture. The handshake line becomes the
//! connect record, so a recording replays through the same engine as a
//! synthetic capture. No truth/audibility files — recordings score only
//! against external truth.
//!
//! Privacy: real handshakes carry real receiver coordinates. See
//! docs/capture-format.md — recordings are private artifacts for now.

use anyhow::{Context, Result};
use mb_capture::{
    CaptureWriter, ClientEntry, Manifest, REC_C2S, REC_CONNECT, REC_DISCONNECT, REC_S2C,
};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};

pub async fn record(listen: &str, upstream: &str, out: &Path, duration_s: u64) -> Result<()> {
    let writer = Arc::new(CaptureWriter::create(out).map_err(|e| anyhow::anyhow!("{e}"))?);
    let entries: Arc<Mutex<Vec<ClientEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let conn_seq = Arc::new(AtomicU32::new(0));
    let t0 = Instant::now();

    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!(
        "record: {listen} → {upstream}, for {duration_s}s, into {}",
        out.display()
    );

    let deadline = t0 + Duration::from_secs(duration_s);
    let mut taps = JoinSet::new();
    loop {
        let accept = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            a = listener.accept() => a,
        };
        let (client, peer) = accept?;
        let seq = conn_seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("conn-{seq:03}");
        println!("record: {id} from {peer}");
        let upstream = upstream.to_string();
        let writer = writer.clone();
        let entries = entries.clone();
        taps.spawn(async move {
            if let Err(e) = tap(client, &upstream, &id, t0, &writer, &entries).await {
                eprintln!("record: {id}: {e:#}");
            }
        });
    }
    // Cut still-open taps; aborting drops their zstd encoders, which flush.
    taps.abort_all();
    while taps.join_next().await.is_some() {}

    let entries = entries.lock().await.clone();
    let n = entries.len();
    writer
        .write_manifest(&Manifest {
            format: "mbc".into(),
            version: 1,
            name: "recording".into(),
            seed: 0,
            duration_s,
            scenario_sha256: String::new(),
            clients: entries,
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("record: done, {n} connections captured");
    Ok(())
}

async fn tap(
    client: TcpStream,
    upstream: &str,
    id: &str,
    t0: Instant,
    writer: &CaptureWriter,
    entries: &Mutex<Vec<ClientEntry>>,
) -> Result<()> {
    client.set_nodelay(true)?;
    let server = TcpStream::connect(upstream)
        .await
        .with_context(|| format!("connect upstream {upstream}"))?;
    server.set_nodelay(true)?;

    let (crd, mut cwr) = client.into_split();
    let (mut srd, mut swr) = server.into_split();
    let mut crd = BufReader::new(crd);

    // First client line = handshake = the connect record.
    let mut handshake = Vec::new();
    crd.read_until(b'\n', &mut handshake).await?;
    if handshake.is_empty() {
        anyhow::bail!("client closed before handshake");
    }
    let hs: serde_json::Value = serde_json::from_slice(&handshake).unwrap_or_default();
    let entry = ClientEntry {
        id: id.to_string(),
        file: format!("clients/{id}.mbc.zst"),
        clock_type: hs
            .get("clock_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .into(),
        // Best effort: the server's *choice* isn't visible in the client line;
        // record the first offer, which is what our own clients pin.
        compress: hs
            .get("compress")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .into(),
        lat: hs.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
        lon: hs.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
        alt_m: hs.get("alt").and_then(|v| v.as_f64()).unwrap_or(0.0),
    };

    let cw = Arc::new(Mutex::new(
        writer
            .client_writer(id)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    ));
    let now = |t0: Instant| Instant::now().duration_since(t0).as_nanos() as u64;
    cw.lock()
        .await
        .record(now(t0), REC_CONNECT, &handshake)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    swr.write_all(&handshake).await?;
    // Register in the manifest NOW, so a connection still open when the
    // recording deadline hits is not lost.
    entries.lock().await.push(entry);

    // client → server
    let cw_up = cw.clone();
    let up = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = crd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            cw_up
                .lock()
                .await
                .record(now(t0), REC_C2S, &buf[..n])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            swr.write_all(&buf[..n]).await?;
        }
        swr.shutdown().await.ok();
        anyhow::Ok(())
    };
    // server → client
    let cw_down = cw.clone();
    let down = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = srd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            cw_down
                .lock()
                .await
                .record(now(t0), REC_S2C, &buf[..n])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            cwr.write_all(&buf[..n]).await?;
        }
        cwr.shutdown().await.ok();
        anyhow::Ok(())
    };

    let (a, b) = tokio::join!(up, down);
    cw.lock()
        .await
        .record(now(t0), REC_DISCONNECT, &[])
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // AutoFinishEncoder flushes on drop; the Arc unwinds here.
    a?;
    b?;
    Ok(())
}
