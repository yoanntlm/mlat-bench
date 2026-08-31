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
1. Replay send-timing must be accurate to well under ~0.9 s — trivially true
   in real time with absolute tokio deadlines.
2. Faster-than-real-time replay CANNOT work without faking the server's clock
   (libfaketime experiment, M6). Confirmed suspicion R7.
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

## Open questions

- [ ] `ssync` split-sync: when does the client prefer it, does the oracle need
      it for anything we measure? (M2/M3)
- [ ] `rate_report` semantics: does the server *require* rate reports for
      selective traffic to work, and does their absence change sync behavior?
      (M3)
- [ ] Exact `results.csv` write cadence and flush behavior. (M4)
- [ ] `sync.json` schema in the work dir. (M3 — parse defensively.)
