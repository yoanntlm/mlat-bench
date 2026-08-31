//! mlat-bench — replay + benchmark harness for Mode S multilateration servers.

mod doctor;
mod gencmd;
mod probe;
mod recordcmd;
mod runcmd;
mod scorecmd;

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
    Run {
        scenario: std::path::PathBuf,
        /// Replay speed multiplier (oracle clock accelerated to match via
        /// libfaketime). 1 = real time.
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
    /// Replay an existing capture against the oracle (M5), or against an
    /// external candidate server with --addr.
    Replay {
        capture: std::path::PathBuf,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Feed an already-running external server at this address instead of
        /// managing the oracle container. Requires --results-csv.
        #[arg(long, requires = "results_csv")]
        addr: Option<String>,
        /// Where the external server writes its oracle-format results CSV;
        /// copied into the run dir for scoring.
        #[arg(long)]
        results_csv: Option<std::path::PathBuf>,
    },
    /// Compare two scored runs' metrics.json side by side.
    Diff {
        a: std::path::PathBuf,
        b: std::path::PathBuf,
    },
    /// Score an existing run directory (M4).
    Score { run_dir: std::path::PathBuf },
    /// Summarize a capture (M2).
    Inspect { capture: std::path::PathBuf },
    /// Transparent proxy tap: record real mlat-client traffic into a capture.
    Record {
        /// Listen address for clients, e.g. 0.0.0.0:40150
        #[arg(long)]
        listen: String,
        /// The real server to forward to, e.g. feed.example.net:31090
        #[arg(long)]
        upstream: String,
        #[arg(short, long)]
        out: std::path::PathBuf,
        /// Stop after this many seconds.
        #[arg(long, default_value_t = 3600)]
        duration_s: u64,
    },
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
        Cmd::Run { scenario, speed } => runcmd::run(&scenario, speed).await,
        Cmd::Replay {
            capture,
            speed,
            addr,
            results_csv,
        } => runcmd::replay(&capture, speed, addr.as_deref(), results_csv.as_deref()).await,
        Cmd::Diff { a, b } => scorecmd::diff(&a, &b),
        Cmd::Score { run_dir } => scorecmd::score(&run_dir),
        Cmd::Record {
            listen,
            upstream,
            out,
            duration_s,
        } => recordcmd::record(&listen, &upstream, &out, duration_s).await,
    }
}
