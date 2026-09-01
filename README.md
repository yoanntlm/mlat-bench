# mlat-bench

A replay and benchmark harness for Mode S multilateration (MLAT) servers.

mlat-bench generates synthetic receiver traffic with known ground truth:
aircraft on known trajectories, receivers with modeled clock drift and
jitter, radio-horizon audibility, and propagation delay. It sends this
traffic to a real MLAT server over the mlat-client wire protocol, in real
time. It then scores the server output against the truth: position error
percentiles, coverage, time to first fix, ghost positions, CPU, and memory.

The reference server ("the oracle") is
[wiedehopf/mlat-server](https://github.com/wiedehopf/mlat-server), built
from a pinned upstream commit in its own container. The same capture can be
replayed against any other implementation, and the two reports compared.

## Example run

`scenarios/smoke.toml` describes 5 receivers around Nantes (one
GPS-disciplined, four consumer dump1090 clocks at −7.8 to +12.5 ppm),
8 ADS-B aircraft as sync sources, and 3 Mode-S-only aircraft to
multilaterate, for 10 minutes:

```
$ cargo run -p mlat-bench -- run scenarios/smoke.toml
...
score: 981 results, 977 matched
score: horizontal error p50 27 m / p90 67 m / p99 146 m
score: ghosts 0 unknown + 4 gross
```

In this run the oracle's pairwise ppm estimates converge on the simulated
clock offsets. The high-rate targets acquire in approximately 2 s; the
low-rate target takes 126 s. The report also shows that the oracle's
self-reported error estimate is approximately 9× larger than its true error
on this scenario.

## Quickstart

```sh
docker compose -f oracle/compose.yaml up -d --wait   # build + start the oracle
cargo run -p mlat-bench -- doctor                    # environment check
cargo run -p mlat-bench -- run scenarios/smoke.toml  # gen + replay + score
uv run plots/plot.py runs/<run-dir>                  # error CDF, map, CPU
```

Other commands: `gen` (scenario → capture), `inspect`, `replay` (existing
capture), `score` (re-score a run), `record` (transparent proxy tap that
captures real mlat-client traffic for later replay), `beast-serve` and
`import-locards` (below).

## What this is not

- Not an RF simulator. There is no multipath, no antenna pattern, and no
  signal-strength model. Audibility is geometric: radio horizon, a range
  cap, and a loss rate.
- Not a measurement of real-world accuracy by itself. Synthetic scenarios
  measure behavior under controlled conditions. For real conditions, use
  the LocaRDS import and the `record` proxy (below).

## Determinism

`gen` is a pure function of the scenario file: the same TOML produces a
byte-identical capture. The RNG is seeded and domain-separated; an added
aircraft does not change another aircraft's byte stream. Replay timing is
real time with OS jitter, but MLAT precision lives in the receiver-clock
timestamps inside the payload, and those are fixed at gen time. The oracle
is not deterministic; compare runs with metrics, not bytes.

## The candidate

`crates/mlatd` is a Rust MLAT server developed against this bench. Its
product repo is [flightportrait/mlatd](https://github.com/flightportrait/mlatd).
Design summary: per-pair clock sync with prediction-interval sigmas, a
reference receiver elected per cluster, timestamp clustering before the
solve, and fixed-altitude weighted Gauss-Newton TDOA with covariance gating
and the oracle's accuracy-scaled output throttle.

Scores on the 600 s smoke scenario at 10× replay, two seeds:

| | oracle s42 | mlatd s42 | oracle s1337 | mlatd s1337 |
|---|---|---|---|---|
| p50 error | 27.1 m | **18.1 m** | 25.7 m | **18.9 m** |
| p90 | 65.8 m | **48.1 m** | 62.1 m | **48.0 m** |
| p99 | 169 m | **123 m** | 190 m | **112 m** |
| results | 1070 | 1773 | 1145 | 1436 |
| ghosts (gross) | 4 | 1 | 3 | 0 |
| coverage | 59 % | 85 % | 62 % | 69 % |
| CPU (10×) | 7.0 % | **4.1 %** | 13.9 % | **3.9 %** |
| RSS | 73 MB | **5.8 MB** | 76 MB | **5.9 MB** |

Most of the candidate's improvements are ports of the oracle's own
heuristics, accepted or rejected by measurement here: reception-time
stamping (a 165 m bias, found in one iteration), covariance error estimates
and the accuracy-scaled throttle, warm starts, per-measurement weighting,
timestamp clustering (byte-identical level-flight frames from different
transmissions merge without it and caused 300 m p99 bursts; this is why
mlat-server has `_cluster_timestamps`), and prediction-interval sigmas on
the pair models (young sync models hid km-scale errors behind 24 m fit
residuals; seed 1337's p99 decreased from 592 m to 112 m).

### Harder scenarios

**Hostile** (`scenarios/hostile.toml`: an ADS-B sync source that transmits
false positions, multipath spikes, a receiver with wrong reported
coordinates, out-of-spec wandering and jumping clocks, 4 ms network jitter,
heavy loss):

| | oracle | mlatd |
|---|---|---|
| p50 / p90 / p99 | **80** / 301 / 919 m | 105 / **293** / **852** m |
| ghosts (gross) | 5 | **0** |
| CPU / RSS (10×) | 13.9 % / 72 MB | **4.3 % / 6 MB** |

The oracle keeps the better median. The candidate has better tails, no
ghosts, and lower resource use. The median gap is an open item.

**Metro scale** (`scenarios/metro-scale.toml`: 60 receivers, 60 aircraft,
150 km radius, both servers at 2× replay):

| | oracle | mlatd |
|---|---|---|
| results | 637 | **6143** |
| coverage | 4 % | **47 %** |
| ghost rate | 5.2 % | **0.1 %** |
| p50 | 47 m* | 65 m |
| CPU / RSS | 32.5 % / 91 MB | **15.8 % / 32 MB** |

*The oracle's p50 covers only the 4 % of aircraft-seconds it tracked. One
mlat-server partition loses most coverage at this receiver density; real
deployments shard regionally by hand. At 5× compression the oracle's ghost
rate reached 31 % (see docs/protocol-notes.md on time-compression
fairness).

Rejected ideas stay in the code as comments (unconditional leave-one-out,
residual-variance weighting) so that they are not retried. The k² growth of
sync traffic at scale is the reason for the oracle's MAX_SYNC_AC = 15:
congestion control, not statistics.

## The client

`crates/mlatc` is a Rust MLAT client developed against the same harness: a
compatible replacement for mutability's mlat-client. Beast input, the
mlat-client wire protocol out (zlib2 both directions, selective traffic,
sync pairing, clock_reset, rate reports), and an SBS listener for returned
positions. mlat-client's flag names.

Verified two ways on the smoke capture, five instances fed by
`beast-serve`:

- Against mlatd: 1,750 positions at p50 20 m / p90 44 m / p99 106 m, zero
  gross ghosts. The real mlat-client on the same pipeline scored
  p50 31 / p90 77 / p99 144 m.
- Against the real mlat-server (the oracle container), 10 minutes real
  time: no disconnects, clock sync trains (coordinator: 0 bad sync, 0 %
  outliers), 879 positions at p50 104 m returned to the client's SBS
  output. No same-pipeline real-mlat-client baseline exists yet for this
  number; the oracle steers real clients' sync traffic, so it is not
  comparable to the replay rows above. This run also found an undocumented
  protocol fact: zlib2 compresses the server-to-client direction too
  (docs/protocol-notes.md).

The Beast results output (`beast,connect` / `beast,listen`) emits the
same synthetic DF18 frames as the real client, so readsb ingests MLAT
positions unchanged; verified frame-exact against an independent
decoder. `--stats-json` writes the same stats file as the real client,
from the server's per-receiver stats push (which mlatd now emits; the
real mlat-client on the live trial writes its stats file from it). Not
implemented yet: radarcape_gps and SBS inputs, UDP transport.

## One binary, world scale

The earlier single-mutex candidate collapsed at approximately 800 dense
receivers. The server now uses lock-free geographic shards (each shard owns
its state in one task, with message passing only) and constant-size
clock-pair models. Measured: the 800-receiver world that collapsed the old
build at 2× time compression runs at 4× — a sustained load equivalent to
3,200 receivers — with unchanged accuracy (p50 48 vs 47 m), ~3.7 cores,
and 72 MB RSS. Real data does not regress: 35.2k results at p50 96 m on
LocaRDS with default flags, within a few percent of the pre-shard best; the
difference is the boundary cost of the partition. `--shard-cell-deg` and
`--shard-cap` tune the partition to the deployment geometry. From the
measured per-message cost, ~10k global feeders fit one multi-core box; this
is an extrapolation, not a measurement.

Three bench bugs were found and fixed during this work, in sequence:
divergent scaled-clock epochs, timestamps taken after queue lag, and the
bench's own result echo corrupting its measurement anchor. Details are in
the commit history.

## Real clients, real data

- **`beast-serve`** replays a capture client's receptions as a Mode-S Beast
  stream, as input for the real wiedehopf mlat-client, without an SDR. Five
  real clients ran end-to-end against mlatd: p50 31 m / p90 77 m /
  p99 144 m against capture truth. Finding: a real client sends no traffic
  until the server sends `start_sending`; selective traffic is the request
  channel, not an optimization.
- **`import-locards`** converts a LocaRDS set (real OpenSky receivers,
  CC BY-SA, published truth) into a capture: real sensor geometry and raw
  per-sensor nanosecond timestamps, with frames re-encoded from the truth
  rows, and 25 % of aircraft held out as DF4-only targets the servers must
  locate. One 10-minute slice: 1.09 M transmissions, 316 active sensors,
  455 holdout aircraft. `tools/fetch_locards.sh` fetches the dataset.

### Real-data results

LocaRDS slice, both servers, identical input, 2× replay. The oracle runs
with the crash guard described under Corrections:

| 316 real sensors · 455 holdout aircraft · 10 min | oracle (patched) | mlatd |
|---|---|---|
| scoreable positions | 6,568 | **35,769** |
| p50 / p90 / p99 | 135 / 504 / 2,069 m | **94 / 298 / 901 m** |
| ghost rate | 0.67 % | **0.18 %** |
| coverage | 4.6 % | **25.1 %** |
| CPU / RSS | 54 % / 775 MB | **~14 % / 55 MB** |

Also measured on real data:

- Alpha-beta track smoothing makes results worse (p99 901 → 3,757 m); the
  `--write-filtered-csv` flag stays experimental.
- Self-truth calibration holds: it reports p50 126 m where the holdout
  truth measures 94–101 m.
- Generalization: a LocaRDS slice not used during development (different
  hour, different sensor mix) scores the same as the development slice:
  p50 91 vs 93 m, coverage 27 vs 25 %, ghost rate 0.17 vs 0.18 %.
- One hour of the same feed at 2×: 202,903 positions, p50 104 m, ghost
  rate stable at 0.13 %, RSS ≤ 70 MB, CPU ≤ 29 %. No memory growth, no
  sync decay.

Two lessons from the first real-data runs: a single global sync reference
produces zero solves on continental geometry (2.17 M sync observations, no
solutions) — the reference is now elected per message group, because
receivers that hear the same frame are geographic neighbors. And the replay
engine must survive the oracle's idle-client reaper, as a real network
survives feeder churn.

Provenance of the comparison target: adsb.lol's public k8s spec deploys
ghcr.io/katlol/mlat-server, a wiedehopf/mlat-server fork 25 commits ahead
and 0 behind; all 25 are packaging and ops changes. The algorithms benched
here are the algorithms that aggregator runs.

Open items on real data: the p99 near 1 km, and the 0.15 % ghost floor
(diffuse geometry events, not attributable to single sensors; receiver
quarantine benched neutral on this slice).

### Corrections

Two errors were found and fixed on 2026-09-01, and are kept here on
purpose:

1. The unpatched oracle produced only 28 results on the LocaRDS capture.
   Cause: a NaN covariance raises ValueError inside mlattrack's group
   processing, which drops every pending group in that cycle. This is an
   upstream bug found by this bench. The oracle image now carries a
   one-line guard; an issue is drafted for upstream.
2. An earlier scorer version labeled the oracle's 813 legitimate ADS-B
   multilaterations as ghosts (11.6 %). The scorer now separates
   unscoreable known aircraft from ghosts; the correct oracle ghost rate
   is 0.67 %.
3. Real-time (1x) replays scored ~35 m worse at p50 than accelerated
   replays of the same capture. Cause: the scorer applied its
   heartbeat-based time anchor only to accelerated runs, so at 1x a
   server-clock skew plus transport latency (measured: 0.69 s) shifted
   every truth lookup. The anchor now applies at every speed; the
   production run rescored from 128 m to 92 m. The engine was never
   slower in real time.

## Docs

- `docs/protocol-notes.md` — verified wire-protocol facts, with dates
- `docs/capture-format.md` — the MBC capture container, frozen v1
- `docs/metrics.md` — exact metric definitions
- `docs/prior-art.md` — annotated bibliography (datasets, competitions,
  methods worth benchmarking on this bench)

## Licensing boundary

The harness is MIT, with one exception: `crates/mlatd`, the candidate
server, is AGPL-3.0-or-later (LICENSE-AGPL) — the same license as the
mlat-server it replaces; its product repo is
github.com/flightportrait/mlatd. The oracle server is AGPLv3 and lives only
in its own container, built from upstream source at image build time; no
server code is vendored into this repository. The MLAT protocol and the
original server and client are the work of Oliver Jowett (mutability);
wiedehopf maintains the fork in production use.

## Roadmap

- Close the hostile-scenario median gap (80 vs 105 m).
- Reduce the real-data p99 (~1 km) and the 0.15 % ghost floor.
- Measure a 10k-receiver world instead of extrapolating to it.
- UDP transport and the remaining mlat-server surface (tracked in the
  mlatd repo).
- The remaining mlatc surface: radarcape_gps input, Beast result outputs.
- Coordinate fuzzing for shareable real recordings.
