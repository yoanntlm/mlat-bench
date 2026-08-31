//! M0 smoke test: one synthetic receiver handshakes with a live oracle and
//! holds the connection, exchanging heartbeats. Proves: TCP path, handshake
//! field validation, reply parsing, heartbeat cadence — before any Mode S
//! bytes exist.

use anyhow::{bail, Context, Result};
use mb_proto::{ClientMsg, ClockType, Compress, Handshake, ServerMsg};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{interval, timeout, Duration, Instant};

pub async fn run(addr: &str, hold_s: u64) -> Result<()> {
    println!("probe: connecting to {addr}");
    let stream = TcpStream::connect(addr).await.with_context(|| {
        format!("connect {addr} — is the oracle up? (docker compose -f oracle/compose.yaml up -d)")
    })?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let hs = Handshake {
        version: 3,
        user: "mb-probe-000".into(),
        uuid: None,
        compress: vec![Compress::None],
        // Nantes-ish. Coordinates must be plausible or the server may deny.
        lat: 47.2181,
        lon: -1.5528,
        alt: 40.0,
        clock_type: ClockType::Dump1090,
        return_results: Some(true),
        return_result_format: None,
        client_version: Some(format!("mlat-bench {}", env!("CARGO_PKG_VERSION"))),
        selective_traffic: None,
        heartbeat: None,
    };
    wr.write_all(&hs.to_line()).await?;
    println!(
        "probe: handshake sent ({})",
        String::from_utf8_lossy(hs.to_line().trim_ascii_end())
    );

    let first = timeout(Duration::from_secs(10), lines.next_line())
        .await
        .context("no handshake reply within 10s")??
        .context("server closed before replying")?;
    match ServerMsg::parse_handshake_reply(first.as_bytes())? {
        ServerMsg::HandshakeAccept {
            compress,
            motd,
            heartbeat,
            return_results,
            ..
        } => {
            println!("probe: ACCEPTED  compress={compress:?} heartbeat={heartbeat} return_results={return_results}");
            if let Some(m) = motd {
                println!("probe: motd: {m}");
            }
            if compress != Compress::None {
                bail!(
                    "offered only none, server negotiated {compress:?} — protocol misunderstanding"
                );
            }
        }
        ServerMsg::Deny(d) => bail!("server DENIED handshake: {d}"),
        other => bail!("unexpected first message: {other:?}"),
    }

    // Hold: heartbeat every 30 s (mlat-client cadence), log whatever arrives.
    let deadline = Instant::now() + Duration::from_secs(hold_s);
    let mut hb = interval(Duration::from_secs(30));
    hb.tick().await; // first tick is immediate; consume it
    let mut server_msgs = 0u32;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = hb.tick() => {
                wr.write_all(&ClientMsg::heartbeat_now().to_line()).await?;
                println!("probe: heartbeat →");
            }
            line = lines.next_line() => {
                match line? {
                    Some(l) => {
                        server_msgs += 1;
                        match ServerMsg::parse_line(l.as_bytes())? {
                            ServerMsg::Heartbeat { server_time } =>
                                println!("probe: ← heartbeat (server_time={server_time:?})"),
                            ServerMsg::Unknown(v) =>
                                println!("probe: ← UNKNOWN (protocol-notes candidate): {v}"),
                            other => println!("probe: ← {other:?}"),
                        }
                    }
                    None => bail!("server closed connection after {server_msgs} messages"),
                }
            }
        }
    }

    println!("probe: held {hold_s}s, {server_msgs} server messages, clean exit");
    Ok(())
}
