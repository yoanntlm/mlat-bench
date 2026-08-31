# mlat-bench

A replay + benchmark harness for Mode S multilateration (MLAT) servers.

mlat-bench generates fully synthetic but physically realistic receiver traffic
— aircraft on known trajectories, receivers with realistic clock drift and
jitter, radio-horizon audibility, propagation delay — where the ground truth
is known by construction. It feeds that traffic to a real MLAT server over the
actual mlat-client wire protocol, in real time, then scores the server's
output against the truth: position error percentiles, coverage, time to first
fix, ghost positions, CPU/RAM.

The reference server ("the oracle") is
[wiedehopf/mlat-server](https://github.com/wiedehopf/mlat-server), built from
a pinned upstream commit inside its own container. The same capture can later
be replayed against any other implementation and the two reports diffed —
that's the point.

## What a run looks like

`scenarios/smoke.toml` — 5 receivers around Nantes (one GPS-disciplined, four
consumer dump1090 clocks at −7.8…+12.5 ppm), 8 ADS-B aircraft as sync
sources, 3 Mode-S-only aircraft to multilaterate, 10 minutes:

```
$ cargo run -p mlat-bench -- run scenarios/smoke.toml
...
score: 981 results, 977 matched
score: horizontal error p50 27 m / p90 67 m / p99 146 m
score: ghosts 0 unknown + 4 gross
```

The oracle's sync state converges on the *simulated* clock offsets (its
pairwise ppm estimates match the scenario's values), the high-rate targets
acquire in ~2 s, and the low-rate one takes 126 s — the physics behaves.
Reports also surface things worth knowing: e.g. the oracle's self-reported
error estimate ran ~9× larger than its true error on this scenario.

## Quickstart

```sh
docker compose -f oracle/compose.yaml up -d --wait   # build + start the oracle
cargo run -p mlat-bench -- doctor                    # environment check
cargo run -p mlat-bench -- run scenarios/smoke.toml  # gen + replay + score
uv run plots/plot.py runs/<run-dir>                  # error CDF, map, CPU
```

Other commands: `gen` (scenario → capture), `inspect`, `replay` (existing
capture), `score` (re-score a run), `record` (transparent proxy tap that
captures real mlat-client traffic for later replay).

## What this is not

- Not an RF simulator: no multipath, no antenna patterns, no signal strength
  modeling. Audibility is geometric (radio horizon + range cap + loss rate).
- Not a validation of any server's real-world accuracy — synthetic scenarios
  measure behavior under *controlled* conditions, which is what regression
  testing needs and field data can't give.

## Determinism

`gen` is a pure function of the scenario file: same TOML → byte-identical
capture, forever (domain-separated seeded RNG; adding an aircraft never
perturbs another's byte stream). Replay timing is real-time and OS-jittered,
but MLAT precision lives in the receiver-clock timestamps *inside* the
payload, which are fixed at gen time. The oracle itself is not deterministic
— compare runs via metrics, not bytes.

## The candidate

`crates/mb-server` is a from-scratch Rust MLAT server benched against the
oracle on identical captures — the comparison the harness exists for. ~1000
lines: pairwise clock sync (windowed linear fit with honest prediction-
interval sigmas, star topology to a GPS-preferred reference), timestamp
clustering before the solve, fixed-altitude weighted Gauss-Newton TDOA with
covariance gating and the oracle's accuracy-scaled output throttle.

Scored on the 600 s smoke scenario at 10× replay, two independent seeds:

| | oracle s42 | mb-server s42 | oracle s1337 | mb-server s1337 |
|---|---|---|---|---|
| p50 error | 27.1 m | **18.1 m** | 25.7 m | **18.9 m** |
| p90 | 65.8 m | **48.1 m** | 62.1 m | **48.0 m** |
| p99 | 169 m | **123 m** | 190 m | **112 m** |
| results | 1070 | 1773 | 1145 | 1436 |
| ghosts (gross) | 4 | 1 | 3 | 0 |
| coverage | 59 % | 85 % | 62 % | 69 % |
| CPU (10×) | 7.0 % | **4.1 %** | 13.9 % | **3.9 %** |
| RSS | 73 MB | **5.8 MB** | 76 MB | **5.9 MB** |

Every improvement was bench-driven, most of them ports of the oracle's own
accumulated heuristics with the bench as referee: reception-time stamping
(165 m bias caught in one iteration), covariance error estimates and the
accuracy-scaled throttle, warm starts, per-measurement weighting, timestamp
clustering (byte-identical level-flight frames merge distinct transmissions
— 300 m p99 bursts until clustered, exactly why mutability wrote
_cluster_timestamps), and prediction-interval sigmas on the pair models
(young/extrapolating sync models hid km-scale errors behind 24 m fit
residuals; seed 1337's p99 went 592 m → 112 m).

### Three worlds

The lab table above is the friendly world. Two harder ones, same protocol:

**Hostile** (`scenarios/hostile.toml`: a lying ADS-B sync source, multipath
spikes, a receiver with wrong reported coordinates, out-of-spec wandering
and jumping clocks, 4 ms network jitter, heavy loss):

| | oracle | mb-server |
|---|---|---|
| p50 / p90 / p99 | **80** / 301 / 919 m | 105 / **293** / **852** m |
| ghosts (gross) | 5 | **0** |
| CPU / RSS (10×) | 13.9 % / 72 MB | **4.3 % / 6 MB** |

The oracle's decade of field scars is real and measurable: it still wins the
hostile median. The candidate wins the tail, the ghost discipline, and the
resources. Closing that median gap is the open accuracy frontier.

**Metro scale** (`scenarios/metro-scale.toml`: 60 receivers, 60 aircraft,
150 km radius, both servers at a gentle 2× replay):

| | oracle | mb-server |
|---|---|---|
| results | 637 | **6143** |
| coverage | 4 % | **47 %** |
| ghost rate | 5.2 % | **0.1 %** |
| p50 | 47 m* | 65 m |
| CPU / RSS | 32.5 % / 91 MB | **15.8 % / 32 MB** |

*computed over the 4 % of aircraft-seconds it covered — survivorship, not
superiority. A single-partition oracle at this receiver density abdicates;
real deployments shard regionally, which is exactly the operational cost the
candidate's headroom avoids. (At 5× compression the oracle's ghost rate hit
31 % — see docs/protocol-notes.md on time-compression fairness.)

Bench-rejected ideas are kept in the code as comments (unconditional
leave-one-out, residual-variance weighting) so they don't get retried; the
k²-sync wall at scale turned out to be why the oracle's MAX_SYNC_AC = 15
exists — congestion control, not statistics.

Honest scope: all synthetic. No real RF, no real feeder zoo, no result
return to clients, no reconnect/blacklist plumbing. Real-data import
(LocaRDS; live recordings via `record`) is the next chapter.

## Docs

- `docs/protocol-notes.md` — verified wire-protocol facts, with dates
- `docs/capture-format.md` — the MBC capture container, frozen v1
- `docs/metrics.md` — exact metric definitions
- `docs/prior-art.md` — annotated bibliography (datasets, competitions,
  methods worth benchmarking on this bench)

## Licensing boundary

The harness is MIT OR Apache-2.0. The oracle server is AGPLv3 and lives only
in its own container, built from upstream source at image build time; no
server code is vendored into this repository. Credit where it's due: the MLAT
protocol and server are the work of Oliver Jowett (mutability) and wiedehopf.

## Roadmap

- Candidate-server slot-in: same capture, two `metrics.json`, one diff.
- Faster-than-real-time replay (the oracle groups mlat messages by its own
  wall clock within 0.9 s, so this needs libfaketime in the container).
- Import of external ground-truth datasets (LocaRDS) as captures.
- Coordinate fuzzing for shareable real recordings.
