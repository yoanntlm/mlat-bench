# Metric definitions (metrics.json schema v1)

Scoring inputs: capture truth + audibility, oracle `results.csv`,
`run.json.wall_t0`, `resources.jsonl`. All positions compare via haversine on
the mean sphere (error of that approximation ≪ MLAT error).

- **matched result** — csv row whose ICAO exists in truth, whose time maps
  into the scenario window, and whose horizontal error ≤ 10 km.
- **ghost (unknown icao)** — csv row for an aircraft the scenario never flew.
- **ghost (gross)** — known aircraft, error > 10 km. Ghosts never enter the
  error percentiles; they are counted separately. Rationale: a 25 km "fix"
  averaged into a p99 hides as noise; counted as a ghost it stays visible.
- **horizontal_error_m** — percentiles over matched results only.
- **err_estimate_ratio_p50** — oracle's `err` column ÷ real error, median.
  < 1 means the oracle is overconfident about its own accuracy.
- **trackable aircraft-second** — a (Mode-S-only aircraft, second) pair with
  ≥ 4 receivers geometrically audible (radio horizon + range cap; loss is NOT
  applied — it's the theoretical ceiling).
- **coverage ratio** — trackable seconds that got ≥ 1 result ÷ trackable
  seconds. ADS-B aircraft are excluded: they don't need MLAT.
  Caveat: this is bounded above by the server's own output cadence — a server
  emitting one fix per 2 s can never exceed ~50 % under this definition even
  while tracking continuously. Compare coverage between servers at equal
  output cadence, or lean on TTFF + update rate.
- **TTFF** — first result's sim-time minus the aircraft's first trackable
  second. Includes the oracle's sync warm-up, deliberately.
- **cpu %** — from cgroup v2 `usage_usec` deltas over ~2 s samples; one full
  core = 100 %.
