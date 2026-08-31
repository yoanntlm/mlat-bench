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
oracle on identical captures — the comparison the harness exists for. v0 is
~800 lines: pairwise clock sync (windowed linear fit, star topology to a
GPS-preferred reference), content-keyed grouping, fixed-altitude Gauss-Newton
TDOA, hard residual gate. First scored round, same 600 s scenario, 10× replay:

| | oracle | mb-server v0 |
|---|---|---|
| results | 1070 | 2017 |
| p50 error | 27.1 m | **21.8 m** |
| p90 / p99 | **65.8 / 169 m** | 88.4 / 301 m |
| ghosts | 0 + 4 gross | 0 + 0 |
| coverage | 59 % | 81 % |

Read it honestly: the candidate wins median, rate, and ghost count; the
oracle's tail (p90/p99) is better — that's where its decade of outlier
heuristics lives, and closing it is the candidate's roadmap. The bench also
caught the candidate's first real bug in one iteration: results stamped at
solve time instead of reception time read as a clean 165 m bias at cruise
speed.

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
