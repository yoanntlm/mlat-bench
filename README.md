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

**Status: early. M0 (scaffold, oracle container, live handshake) works.**
See `docs/` for the protocol conformance notes and the capture format as they
stabilize.

## What this is not

- Not an RF simulator: no multipath, no antenna patterns, no signal strength
  modeling. Audibility is geometric (radio horizon + range cap + loss rate).
- Not a validation of any server's real-world accuracy — synthetic scenarios
  measure behavior under *controlled* conditions, which is what regression
  testing needs and field data can't give.

## Quickstart (current state)

```sh
docker compose -f oracle/compose.yaml up -d --wait   # build + start the oracle
cargo run -p mlat-bench -- doctor                    # environment check
cargo run -p mlat-bench -- probe                     # handshake + heartbeats
```

## Licensing boundary

The harness is MIT OR Apache-2.0. The oracle server is AGPLv3 and lives only
in its own container, built from upstream source at image build time; no
server code is vendored into this repository. Credit where it's due: the MLAT
protocol and server are the work of Oliver Jowett (mutability) and wiedehopf.
