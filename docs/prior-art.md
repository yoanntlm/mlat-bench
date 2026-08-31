# Prior art

Annotated bibliography for the algorithm track. Each entry states how to
verify its claim on this bench, on identical input.

## Production systems

- **mutability/mlat-server** (Oliver Jowett, 2015–2017) — the root
  implementation: pairwise receiver clock modeling from shared DF17 traffic,
  scipy least-squares solve, pykalman track filtering. AGPLv3.
- **wiedehopf/mlat-server** (2019–present) — the maintained fork every open
  aggregator runs; a decade of empirical tuning (outlier thresholds, sync
  heuristics, CPU work). **This is our oracle.** Its constants are the
  accumulated value; any rewrite must reproduce its behavior before deviating.

## Datasets & competitions

- **OpenSky Aircraft Localization Competition** — crowdsourced-receiver
  localization; winning entry synchronized 241 receivers (36 GPS-equipped,
  some with broken clocks) and achieved **81.9 m RMSE 2D**. A reference
  point for achievable accuracy on consumer hardware.
  (Engineering Proceedings 13(1):12, mdpi.com/2673-4591/13/1/12)
- **LocaRDS** — a published localization reference dataset from OpenSky data,
  built exactly for comparable evaluation of MLAT methods.
  (arXiv:2012.00116) — candidate real-world complement to our synthetic
  scenarios once the capture importer exists.

## Methods worth benchmarking (later, on this bench)

- **Joint position + clock estimation** — estimate aircraft state and receiver
  clock offsets in one filter instead of sync-then-solve.
  Kaune et al. direction; concretely: "Wide-area Multilateration Airspace
  Surveillance with Unsynchronized Low-Cost ADS-B Receivers Using TDOA
  Observations", NAVIGATION 72(3), 2025 (navi.ion.org/content/72/3/navi.704).
  This attacks the pairwise-sync bookkeeping that is mlat-server's scaling
  bottleneck; the highest-priority algorithm experiment for this bench.
- **Closed-form TDOA initializers** — Chan-Ho and related methods; near-Taylor
  accuracy without iteration/initialization concerns. Candidate for the
  solve stage's seed.
- **Robust estimation for outliers** — Student's-t / variational Bayes
  (IMM-VB), Huber-loss WLS. Candidate replacements for hand-tuned rejection
  thresholds; the bench's ghost/error metrics are the referee.

## Verification discipline

A method earns a claim here only one way: same capture → oracle and candidate →
`metrics.json` diff. No cross-paper accuracy comparisons — receiver geometry,
traffic, and noise models differ too much between publications to compare
numbers across them.
