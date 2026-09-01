# Protocol conformance notes

Living document. Every fact here was verified against source or observed live,
and carries a date + provenance. When mlat-bench and this file disagree with
upstream, upstream wins and this file gets a dated correction.

Upstream pins studied:
- `wiedehopf/mlat-server` @ `9b27a6d609c5bb47549016412222a79ecb0f48e4` (2026-04-03)
- `wiedehopf/mlat-client` @ master (2026-08-31 clone)

## Timestamps (2026-08-31, from source)

**Wire timestamps are raw integer counter values, unscaled.** mlat-client
formats `message.timestamp` (the decoder's counter) directly into JSON:
`mlat/client/jsonclient.py:300-315` — `'{{"mlat":{{"t":{0},...'.format(message.timestamp)`.
No division by frequency on the client. Units per `modes_reader.c`:

- Beast/AVR: freerunning **48-bit counter @ 12 MHz**
- SBS: freerunning 20 MHz 24-bit (client widens it)
- Radarcape GPS: 1 GHz nanoseconds-since-GPS-midnight

The server resolves units via the handshake `clock_type` → `clocktrack.pyx
make_clock` frequency. Consequence for the simulator: emit integer counts of
the modeled clock, monotonic per connection, 48-bit wrapped.

## Message dispatch (2026-08-31, server jsonclient.py:598-617)

Recognized client keys: `sync`, `ssync` (split sync — exists in the client,
`send_tcp_split_sync`; not yet modeled in mb-proto), `mlat`, `seen`, `lost`,
`input_connected`, `input_disconnected`, `clock_reset`, `clock_jump`,
`heartbeat`, `rate_report`, `quine`. Anything else logs
"Received an unexpected message" — harmless.

## MLAT message grouping uses SERVER arrival time (2026-08-31)

`process_mlat_nongps` (jsonclient.py:650) pairs the raw counter `t` with the
server's wall clock `now`; `config.MLAT_DELAY = 0.9` s bounds the grouping
window. Consequences:
1. Replay send-timing must be accurate to well under ~0.9 s — satisfied
   in real time with absolute tokio deadlines.
2. Faster-than-real-time replay CANNOT work without faking the server's clock
   (libfaketime experiment, M6).
3. The GPS path (`process_mlat_gps`) is marked `#UNUSED` — all JSON clients go
   through the nongps path regardless of clock_type.

## Sync message validation is STRICT (2026-08-31, clocktrack.pyx)

The server fully decodes and cross-checks every sync pair. Our synthetic DF17
encoder must therefore be *correct*, not merely CRC-valid:

- Both messages decoded via modes_cython; must be DF17 airborne position,
  even (F=0) + odd (F=1).
- **CPR global decode of the even/odd pair must succeed** (clocktrack.pyx:432)
  — the pair must be jointly consistent, and emitted within the same NL zone.
- Decoded even/odd positions must be within `MAX_INTERMESSAGE_RANGE = 10 km`
  of each other (config.py:49) — pairs from a fast aircraft must be close in
  time (we'll emit them ~0.3–0.5 s apart).
- Decoded position must be within `MAX_RANGE = 500 km` of the receiver
  (clocktrack.pyx:58,489), and of both receivers of each pairing (line 124).
- Pair interval `(tB−tA)/freq` must be ≤ 5.0 s (line 355); syncpoint matching
  tolerates 0.75 ms interval difference (line 360).

## Handshake acceptance (2026-08-31, jsonclient.py:321-440 + live probe)

Validation: lat −90..90, lon −180..360, alt −1000..10000 m; failures reply
`{"deny": [reason], "reconnect_in": ~900}`. No apparent minimum receiver
separation at handshake time (colocation matters later, in sync geometry).

Observed live (probe, oracle @ pin above): offering `compress:["none"]` is
accepted and negotiated; reply carries `compress`, `heartbeat: true`,
`return_results: true`, `motd` (which includes the server's AGPL source
offer). Server heartbeats arrive as `{"heartbeat": {"server_time": ...}}`.

## Healthcheck note (2026-08-31, observed)

A bare TCP connect + close makes the server log "Badly formatted handshake"
and (upstream cosmetic bug) an AttributeError traceback
(`'JsonClient' object has no attribute 'handle_messages'`, jsonclient.py:302).
Our container healthcheck does exactly this every 5 s, so oracle logs contain
this noise by design. Filter it when reading logs; do not report it as a run
failure.

## Unsolicited mlat messages are accepted (2026-08-31, observed live)

The bench sends `mlat` for every Mode-S reception without waiting for
`start_sending` — the oracle processed them and produced positions
(120 s run: 180 result rows, p50 error 45 m). `start_sending`/`stop_sending`
is bandwidth steering, not authorization. The replay engine therefore does
not need to react to selective-traffic commands for correctness.

## sync.json shape (2026-08-31, observed)

Top level: one key per receiver username. Each has `peers`: map of peer
username → array of at least 8 numbers; field [0] is a sync/pair count and
field [2] tracks the measured pairwise clock offset in ppm — observed values
matched the scenario's simulated ppm offsets to within rounding (rx-001
(+3.2) vs rx-002 (−7.8) reported ≈ −11). Treat everything else as opaque.

## Why MAX_SYNC_AC = 15 exists (2026-08-31, measured while building the candidate)

The oracle caps sync work per aircraft (config.py MAX_SYNC_AC = 15,
MAX_GROUP = 15). Building the candidate revealed why: at 60 co-hearing
receivers, per-syncpoint pair training is k² — ~7200 model updates per sync
event, ~10⁶ updates/s at metro scale. Capping reporters at 15 keeps sync
quality sufficient and cuts the work 16×. These constants are congestion
control, not statistics.

## Oracle behavior under time compression (2026-08-31, observed)

At 5× replay of the 60-receiver metro scenario the oracle produced 31% gross
ghost positions with 6% coverage at 65% CPU — queue pressure, not algorithm
failure (see the 2× fairness run in the same day's bench data before citing
this). Accelerated comparisons at scale must check the oracle's CPU headroom
or they measure the Python event loop, not MLAT.

## Upstream bug: NaN covariance crash-loops group processing (2026-09-01)

Found by the bench on a 316-receiver LocaRDS replay: a degenerate solve
returns `var_est = numpy.trace(ecef_cov) = NaN`; `mlattrack.py:324`
`error = int(math.sqrt(abs(var_est)))` raises ValueError; the exception
unwinds through `[group.handle(group) for group in self.groups]`
(mlattrack.py:58), so EVERY pending group in that processing cycle is
dropped, not just the degenerate one. Observed as 18 crash-loops that
collapsed a 10-minute run's output to 28 results while the server was
otherwise healthy (316 clients synced, 1087 aircraft tracked, 40% CPU).
Bench-local guard patched into oracle/Dockerfile (skip non-finite var_est,
mirroring the existing "result is suspect" path); reproduction: any
LocaRDS import replayed at this density. Fairness note: numbers
from unpatched-oracle runs at scale measure this bug, not throughput.

## Downlink compression is asymmetric per mode (2026-09-01, from source)

`jsonclient.py:203` maps each compression mode to a read method AND a write
method: `zlib2` uses `write_zlib` (server→client is framed and compressed,
same 2-byte-length + stripped-sync-flush format, batched ~1 s, flushed
before 32 KiB of output), while `zlib` and `none` use `write_raw` (plain
NDJSON lines). The handshake reply itself is always a plain line.
Found by mlatc: its line-based reader hit binary bytes ~3 s after a zlib2
handshake (the server's first start_sending batch).

Resolved 2026-09-01: mlatd now negotiates zlib2 first (mlat-server's
order) and compresses its downlink with the same framing, batched up to
1 s; `zlib` and `none` connections keep the plain-line downlink.

## Beast results output and the stats push (2026-09-01, from source)

mlat-client's Beast results output (output.py BeastConnection) wraps each
returned position as a synthetic DF18 frame (CF=2, ME type 18, both CPR
parities carrying the same position, parity appended) in Beast long-frame
framing with the magic timestamp bytes `FF 00 4D 4C 41 54` ("MLAT") and
signal 0. Idle connections send an 11-byte Mode-A/C keepalive every 30 s.
readsb treats the magic timestamp as "MLAT result, do not use for timing".
mlatc reproduces this byte-exactly (verified by independent decode).

`--stats-json` is not client-side bookkeeping: most fields (peer_count,
outlier_percent, bad_sync_timeout) arrive in a server→client stats push,
a wiedehopf protocol extension (jsonclient.py:600 region). A server that
never sends it gets an empty stats file, not an error. mlatd emits the
push since 2026-09-01 (15 s cadence, quarantine mapped to
bad_sync_timeout); verified live: the real mlat-client writes its stats
file for mlatd exactly as it does for the other aggregators.

## Open questions

- [ ] `ssync` split-sync: when does the client prefer it, does the oracle need
      it for anything we measure? (M5+)
- [ ] Does prolonged absence of `rate_report` change server behavior at
      scale? (not needed for correctness — see above)
- [ ] Exact `results.csv` flush cadence (rows appeared promptly in the 120 s
      run; good enough for post-run scoring).
