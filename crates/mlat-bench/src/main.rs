//! mlat-bench — replay + benchmark harness for Mode S multilateration servers.

mod doctor;
mod gencmd;
mod probe;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mlat-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the environment: docker, compose, ports, oracle image.
    Doctor,
    /// M0 smoke: handshake with a running oracle and exchange heartbeats.
    Probe {
        /// host:port of the oracle's client listener.
        #[arg(long, default_value = "127.0.0.1:40147")]
        addr: String,
        /// Seconds to stay connected exchanging heartbeats.
        #[arg(long, default_value_t = 60)]
        hold_s: u64,
    },
    /// Generate a capture from a scenario (M2).
    Gen {
        scenario: std::path::PathBuf,
        #[arg(short, long)]
        out: std::path::PathBuf,
    },
    /// Generate + replay + score in one go (M3/M4).
    Run { scenario: std::path::PathBuf },
    /// Replay an existing capture against the oracle (M5).
    Replay { capture: std::path::PathBuf },
    /// Score an existing run directory (M4).
    Score { run_dir: std::path::PathBuf },
    /// Summarize a capture (M2).
    Inspect { capture: std::path::PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Doctor => doctor::run().await,
        Cmd::Probe { addr, hold_s } => probe::run(&addr, hold_s).await,
        Cmd::Gen { scenario, out } => gencmd::gen(&scenario, &out),
        Cmd::Inspect { capture } => gencmd::inspect(&capture),
        Cmd::Run { .. } | Cmd::Replay { .. } | Cmd::Score { .. } => {
            anyhow::bail!("not implemented yet — see plan milestones (M3+)")
        }
    }
}
