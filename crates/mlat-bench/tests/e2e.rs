//! Full-loop integration test: gen → replay against the real oracle → score.
//! Slow (3+ minutes) and needs docker + the oracle image, so it's opt-in:
//!
//!     MLAT_BENCH_E2E=1 cargo test -p mlat-bench --test e2e -- --ignored
//!
//! CI runs it nightly, not per-push.

use std::process::Command;

#[test]
#[ignore = "slow; needs docker; set MLAT_BENCH_E2E=1"]
fn smoke_run_produces_results() {
    if std::env::var("MLAT_BENCH_E2E").as_deref() != Ok("1") {
        eprintln!("MLAT_BENCH_E2E not set; skipping");
        return;
    }
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf();

    // A 150 s cut of the smoke scenario: long enough to sync, short enough
    // for a test.
    let toml = std::fs::read_to_string(repo.join("scenarios/smoke.toml"))
        .unwrap()
        .replace("duration_s = 600", "duration_s = 150")
        .replace("name = \"smoke\"", "name = \"e2e\"");
    let dir = tempdir();
    let scenario = dir.join("e2e.toml");
    std::fs::write(&scenario, toml).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_mlat-bench"))
        .current_dir(&repo)
        .arg("run")
        .arg(&scenario)
        .output()
        .expect("spawn mlat-bench");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    assert!(out.status.success(), "run failed: {stdout}");

    // The gate: the oracle synced and produced scored results.
    assert!(
        stdout.contains("score:"),
        "run did not reach scoring: {stdout}"
    );
    let matched: usize = stdout
        .lines()
        .find(|l| l.starts_with("score:") && l.contains("matched"))
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(matched > 20, "too few matched results: {stdout}");
}

fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mlat-bench-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
