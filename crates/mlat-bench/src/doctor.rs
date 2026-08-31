//! Environment checks. Each check prints one line; any hard failure exits
//! non-zero so `doctor` can gate CI and drills, same spirit as
//! flightportrait's ops check scripts.

use anyhow::Result;
use std::process::Stdio;
use tokio::process::Command;

pub async fn run() -> Result<()> {
    let mut hard_fail = false;

    hard_fail |= !check_cmd("docker", &["version", "--format", "{{.Server.Version}}"]).await;
    hard_fail |= !check_cmd("docker", &["compose", "version", "--short"]).await;

    // Port free? (only advisory — the oracle may already be up, which is fine)
    match tokio::net::TcpStream::connect("127.0.0.1:40147").await {
        Ok(_) => println!("ok    port 40147: something is listening (oracle already up?)"),
        Err(_) => println!("ok    port 40147: free"),
    }

    // Oracle image present?
    let img = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            "mlat-bench-oracle",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match img {
        Ok(s) if s.success() => println!("ok    oracle image built (mlat-bench-oracle)"),
        _ => println!(
            "warn  oracle image not built yet — run: docker compose -f oracle/compose.yaml build"
        ),
    }

    if hard_fail {
        anyhow::bail!("doctor found hard failures");
    }
    println!("\ndoctor: environment usable");
    Ok(())
}

async fn check_cmd(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).output().await {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            println!("ok    {cmd} {}: {}", args.join(" "), v.trim());
            true
        }
        _ => {
            println!("FAIL  {cmd} {} — not available", args.join(" "));
            false
        }
    }
}
