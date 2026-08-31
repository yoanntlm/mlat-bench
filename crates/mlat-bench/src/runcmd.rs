//! `run` / `replay`: feed a capture to the oracle in real time and collect
//! everything a later `score` needs.
//!
//! Run directory layout:
//! ```text
//! runs/<stamp>_<name>_s<seed>/
//! ├── capture/            the capture being replayed (gen'd or symlink target copied)
//! ├── oracle-work/        the oracle's --work-dir (results.csv, sync.json…)
//! ├── results/<id>.jsonl  server→client messages per client, wall-timestamped
//! ├── sbs.log             basestation output lines, wall-timestamped
//! ├── sync_timeline.jsonl sync.json snapshots every 10 s
//! ├── resources.jsonl     oracle CPU/RSS samples
//! ├── oracle.log          container logs
//! └── run.json            status + timings
//! ```

use anyhow::{bail, Context, Result};
use mb_capture::{CaptureReader, ClientEntry, Record, REC_C2S, REC_CONNECT};
use mb_proto::{Compress, ServerMsg};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{sleep, sleep_until, Duration, Instant};

const ORACLE_ADDR: &str = "127.0.0.1:40147";
const SBS_ADDR: &str = "127.0.0.1:40148";
/// Kalman-filtered results can trail the last input by several seconds.
const DRAIN_S: u64 = 30;

pub async fn run(scenario_path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let sc_text = std::fs::read_to_string(scenario_path)?;
    let sc = mb_sim::Scenario::from_toml(&sc_text).context("parse scenario")?;
    let run_dir = PathBuf::from(format!("runs/{stamp}_{}_s{}", sc.meta.name, sc.meta.seed));
    std::fs::create_dir_all(&run_dir)?;
    let capture_dir = run_dir.join("capture");
    crate::gencmd::gen(scenario_path, &capture_dir)?;
    replay_capture(&capture_dir, &run_dir).await
}

pub async fn replay(capture: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let r = CaptureReader::open(capture).map_err(|e| anyhow::anyhow!("{e}"))?;
    let run_dir = PathBuf::from(format!(
        "runs/{stamp}_{}_replay_s{}",
        r.manifest.name, r.manifest.seed
    ));
    std::fs::create_dir_all(&run_dir)?;
    replay_capture(capture, &run_dir).await
}

async fn replay_capture(capture: &Path, run_dir: &Path) -> Result<()> {
    let reader = Arc::new(CaptureReader::open(capture).map_err(|e| anyhow::anyhow!("{e}"))?);
    let duration_s = reader.manifest.duration_s;
    std::fs::create_dir_all(run_dir.join("results"))?;
    let work_dir = run_dir.join("oracle-work");
    std::fs::create_dir_all(&work_dir)?;

    // ---- oracle up -------------------------------------------------------
    let compose = oracle_compose_path()?;
    println!("run: starting oracle (work dir {})", work_dir.display());
    let up = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose)
        .args(["up", "-d", "--wait"])
        .env("ORACLE_WORK_DIR", std::fs::canonicalize(&work_dir)?)
        .output()
        .await?;
    if !up.status.success() {
        bail!(
            "oracle failed to start:\n{}",
            String::from_utf8_lossy(&up.stderr)
        );
    }
    // The shared epoch, in both clock domains: tokio deadlines use `t0`,
    // scoring maps oracle wall-clock output back to sim time via `wall_t0`.
    let t0 = Instant::now() + Duration::from_secs(2);
    let wall_t0 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64()
        + 2.0;

    // Teardown guard: whatever happens below, capture logs and stop it.
    let result = drive(reader.clone(), run_dir, duration_s, t0).await;
    teardown(&compose, run_dir).await;
    let status = if result.is_ok() { "ok" } else { "failed" };
    std::fs::write(
        run_dir.join("run.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "status": status,
            "capture": capture.display().to_string(),
            "duration_s": duration_s,
            "wall_t0": wall_t0,
        }))?,
    )?;
    println!("run: artifacts in {}", run_dir.display());
    if result.is_ok() {
        // One command → report: score immediately while the artifacts are hot.
        if let Err(e) = crate::scorecmd::score(run_dir) {
            println!("run: scoring failed (artifacts intact): {e:#}");
        }
    }
    result
}

fn oracle_compose_path() -> Result<PathBuf> {
    // Resolve relative to the binary's repo checkout: walk up from cwd.
    let mut d = std::env::current_dir()?;
    loop {
        let p = d.join("oracle/compose.yaml");
        if p.exists() {
            return Ok(p);
        }
        if !d.pop() {
            bail!("oracle/compose.yaml not found above cwd — run from the mlat-bench repo");
        }
    }
}

async fn drive(
    reader: Arc<CaptureReader>,
    run_dir: &Path,
    duration_s: u64,
    t0: Instant,
) -> Result<()> {
    let stop_at = t0 + Duration::from_secs(duration_s + DRAIN_S);
    let result_count = Arc::new(AtomicU64::new(0));

    // ---- background collectors ------------------------------------------
    let sbs_task = tokio::spawn(collect_sbs(run_dir.join("sbs.log"), stop_at));
    let sync_task = tokio::spawn(poll_sync_json(
        run_dir.join("oracle-work/sync.json"),
        run_dir.join("sync_timeline.jsonl"),
        stop_at,
    ));
    let res_task = tokio::spawn(sample_resources(run_dir.join("resources.jsonl"), stop_at));

    // ---- one task per client --------------------------------------------
    let mut tasks = Vec::new();
    for entry in reader.manifest.clients.clone() {
        let reader = reader.clone();
        let out = run_dir.join(format!("results/{}.jsonl", entry.id));
        let count = result_count.clone();
        tasks.push(tokio::spawn(async move {
            feed_client(&reader, &entry, t0, out, count)
                .await
                .with_context(|| format!("client {}", entry.id))
        }));
    }
    let n = tasks.len();
    for t in tasks {
        t.await??;
    }
    println!("run: all {n} clients done, draining {DRAIN_S}s for late results");
    sleep_until(stop_at).await;

    let _ = sbs_task.await;
    let _ = sync_task.await;
    let _ = res_task.await;

    // ---- immediate signal, before scoring exists -------------------------
    let results_csv = run_dir.join("oracle-work/results.csv");
    let csv_rows = std::fs::read_to_string(&results_csv)
        .map(|s| s.lines().count())
        .unwrap_or(0);
    println!(
        "run: {} mlat result rows in results.csv, {} results returned to clients",
        csv_rows,
        result_count.load(Ordering::Relaxed)
    );
    if csv_rows == 0 {
        println!("run: ZERO results — check oracle.log and sync_timeline.jsonl");
    }
    Ok(())
}

/// Connect, handshake, then replay REC_C2S records on absolute deadlines.
async fn feed_client(
    reader: &CaptureReader,
    entry: &ClientEntry,
    t0: Instant,
    results_path: PathBuf,
    result_count: Arc<AtomicU64>,
) -> Result<()> {
    let mut records = reader
        .client_records(entry)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let first = records
        .next()
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .context("empty client stream")?;
    if first.kind != REC_CONNECT {
        bail!("first record must be connect, got type {}", first.kind);
    }

    let stream = TcpStream::connect(ORACLE_ADDR)
        .await
        .context("connect oracle")?;
    stream.set_nodelay(true)?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    wr.write_all(&first.payload).await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .context("handshake reply timeout")??
        .context("closed at handshake")?;
    match ServerMsg::parse_handshake_reply(reply.as_bytes()).map_err(|e| anyhow::anyhow!("{e}"))? {
        ServerMsg::HandshakeAccept { compress, .. } => {
            let want = match entry.compress.as_str() {
                "none" => Compress::None,
                "zlib" => Compress::Zlib,
                "zlib2" => Compress::Zlib2,
                other => bail!("capture has unknown compress {other}"),
            };
            if compress != want {
                bail!("negotiated {compress:?} but capture is framed as {want:?}");
            }
        }
        ServerMsg::Deny(d) => bail!("oracle DENIED {}: {d}", entry.id),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    // Reader side: log every server message with a wall timestamp.
    let reader_task = tokio::spawn(async move {
        let mut out = tokio::fs::File::create(results_path).await?;
        while let Ok(Some(line)) = lines.next_line().await {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            if matches!(
                ServerMsg::parse_line(line.as_bytes()),
                Ok(ServerMsg::Result(_))
            ) {
                result_count.fetch_add(1, Ordering::Relaxed);
            }
            out.write_all(format!("{{\"t\":{now:.3},\"msg\":{line}}}\n").as_bytes())
                .await?;
        }
        anyhow::Ok(())
    });

    // Writer side: absolute deadlines — no cumulative drift.
    for rec in records {
        let rec: Record = rec.map_err(|e| anyhow::anyhow!("{e}"))?;
        if rec.kind != REC_C2S {
            continue;
        }
        sleep_until(t0 + Duration::from_nanos(rec.t_nanos)).await;
        wr.write_all(&rec.payload).await?;
    }
    wr.shutdown().await?;
    // Give the reader a moment to log trailing results for this client.
    let _ = tokio::time::timeout(Duration::from_secs(DRAIN_S), reader_task).await;
    Ok(())
}

async fn collect_sbs(out_path: PathBuf, stop_at: Instant) -> Result<()> {
    // The SBS listener may accept a beat later than the client port; retry.
    let mut out = tokio::fs::File::create(&out_path).await?;
    let stream = loop {
        match TcpStream::connect(SBS_ADDR).await {
            Ok(s) => break s,
            Err(_) if Instant::now() < stop_at => sleep(Duration::from_millis(500)).await,
            Err(e) => return Err(e.into()),
        }
    };
    let mut lines = BufReader::new(stream).lines();
    loop {
        tokio::select! {
            _ = sleep_until(stop_at) => break,
            line = lines.next_line() => match line? {
                Some(l) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
                    out.write_all(format!("{now:.3} {l}\n").as_bytes()).await?;
                }
                None => break,
            }
        }
    }
    Ok(())
}

async fn poll_sync_json(sync_path: PathBuf, out_path: PathBuf, stop_at: Instant) -> Result<()> {
    let mut out = tokio::fs::File::create(&out_path).await?;
    loop {
        if Instant::now() >= stop_at {
            break;
        }
        sleep(Duration::from_secs(10)).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        // Schema is unofficial: store the raw document, interpret later.
        let snapshot = std::fs::read_to_string(&sync_path).unwrap_or_default();
        let compact: serde_json::Value =
            serde_json::from_str(&snapshot).unwrap_or(serde_json::Value::Null);
        out.write_all(format!("{}\n", serde_json::json!({"t": now, "sync": compact})).as_bytes())
            .await?;
    }
    Ok(())
}

/// cgroup v2 first, `docker stats` never — one long-lived stats child is the
/// documented fallback but the cgroup files have been reliable; degrade to
/// "no samples" rather than failing the run.
async fn sample_resources(out_path: PathBuf, stop_at: Instant) -> Result<()> {
    let id = Command::new("docker")
        .args(["inspect", "-f", "{{.Id}}", "mlat-bench-oracle"])
        .output()
        .await?;
    let id = String::from_utf8_lossy(&id.stdout).trim().to_string();
    if id.is_empty() {
        return Ok(());
    }
    let cg = PathBuf::from(format!("/sys/fs/cgroup/system.slice/docker-{id}.scope"));
    let mut out = tokio::fs::File::create(&out_path).await?;
    while Instant::now() < stop_at {
        sleep(Duration::from_secs(2)).await;
        let cpu_usec = std::fs::read_to_string(cg.join("cpu.stat"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("usage_usec"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            });
        let mem = std::fs::read_to_string(cg.join("memory.current"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        out.write_all(
            format!(
                "{}\n",
                serde_json::json!({"t": now, "cpu_usec": cpu_usec, "mem_bytes": mem})
            )
            .as_bytes(),
        )
        .await?;
    }
    Ok(())
}

async fn teardown(compose: &Path, run_dir: &Path) {
    // Best-effort: logs first, then down. Failures here must not mask the
    // run's own error.
    if let Ok(out) = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose)
        .args(["logs", "--no-color"])
        .stdout(Stdio::piped())
        .output()
        .await
    {
        let _ = std::fs::write(run_dir.join("oracle.log"), &out.stdout);
    }
    let _ = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose)
        .args(["down", "-v"])
        .output()
        .await;
}
