# MBC capture format, version 1

Status: **frozen 2026-08-31.** Changes require a version bump and a reader
that still accepts v1; captures are long-lived artifacts.

## Layout

A capture is a directory:

```
capture/
├── manifest.json           required. See below.
├── scenario.toml           synthetic captures only: the verbatim scenario
├── truth.jsonl.zst         synthetic only: one TruthPoint JSON per line
├── audibility.jsonl.zst    synthetic only: per-second audibility rows
└── clients/<id>.mbc.zst    one record stream per client
```

`truth`/`audibility` are absent in real recordings — a capture without them
replays normally but scores only against external truth.

## manifest.json

```json
{
  "format": "mbc", "version": 1,
  "name": "...", "seed": 42, "duration_s": 600,
  "scenario_sha256": "…hex… (empty for recordings)",
  "clients": [
    {"id": "rx-000", "file": "clients/rx-000.mbc.zst",
     "clock_type": "dump1090", "compress": "none",
     "lat": 47.2181, "lon": -1.5528, "alt_m": 40.0}
  ]
}
```

## Record stream (.mbc, inside zstd)

```
"MBC1"                       4-byte magic
{"id": "..."}\n              one JSON header line (open-ended object)
then records, little-endian:
  u64  t_nanos               offset from the capture epoch T0
  u8   type
  u32  len
  [len bytes]                payload
```

Types:
| type | meaning | payload |
|---|---|---|
| 0x01 | client→server bytes | exactly the bytes to write |
| 0x02 | server→client bytes | recorder only; replay ignores |
| 0x03 | connect | the handshake line the client sent |
| 0x04 | disconnect | empty |

Rules:
- The first record of a stream is 0x03. Replay sends its payload, awaits the
  server's handshake reply, verifies the negotiated compression matches the
  manifest, then streams 0x01 records at `T0 + t_nanos`.
- Unknown record types are skipped, not fatal (forward compatibility).
- Compressed (`zlib`/`zlib2`) streams carry framed bytes with persistent
  compressor state: replaying a suffix of a stream is invalid; a subset of
  *clients* is fine.
- zstd level is an implementation detail; determinism guarantees apply to the
  decompressed stream.

## Recording time base

A recording's T0 is when the proxy started, not when the recorded session's
scenario began — the two timelines differ by a constant (observed ~8 s for a
proxied bench replay: oracle spin-up + the replay's own epoch delay). Replay
is unaffected (embedded receiver timestamps carry the precision), but scoring
a recording against externally attached truth must first align the time
bases. Verified 2026-08-31: a synthetic run recorded through the proxy and
replayed scored p50 44 m after a +7.7 s alignment, vs 45 m for the original:
statistically equivalent.

## Privacy

Receiver coordinates (manifest and handshake bytes) identify homes. Do not
publish recordings of real feeders without their consent. `mlat-bench fuzz`
copies a capture with each receiver moved by a seeded draw inside a radius;
run it before sharing any real recording, and treat unfuzzed recordings as
private. A fuzzed capture replays, but its coordinates contradict the
embedded timestamps, so do not score accuracy against it.
