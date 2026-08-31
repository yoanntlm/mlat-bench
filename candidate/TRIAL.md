# Real-feeder trial runbook

Status 2026-08-31: staged, waiting on the hub host (leserveur) returning to
the tailnet. Everything below is one command per host once it's back.

## On the hub (leserveur)

```sh
git clone git@github.com:yoanntlm/mlat-bench && cd mlat-bench/candidate
MB_BIND=100.114.32.128 docker compose up -d --build
docker logs -f mlatd        # expect: listening on 0.0.0.0:31090
```

`MB_BIND` pins the mlat port to the tailnet interface — never the public one.

## On the receiver (station #1)

`receiver/docker-compose.yml` already carries the disarmed line (monorepo
commit 9e899eb):

```
# ;mlat,100.114.32.128,31090,uuid=${UUID_FLIGHTPORTRAIT}
```

Uncomment it (mind the `;` continuation on the line above), then
`docker compose up -d`. Non-exclusive: adsb.lol / airplanes.live / adsb.fi
keep receiving everything, same as always.

## What success looks like (first hour)

- mlatd log: `<station> connected (dump1090, zlib2)` and rate_report-driven
  `start_sending` — the real client sends only after being asked.
- `work/sync.json` appears (one receiver: peers empty — sync needs two).
- No positions with one receiver, by physics. The trial proves protocol
  survival: staying connected for days, tolerating ssync/rate_report,
  surviving readsb restarts on the input side.

## What was already proven without the roof

Five REAL wiedehopf mlat-clients (fed by `beast-serve`, no SDR) ran the full
pipeline end to end: p50 31 m vs capture truth. The trial adds the two things
the lab can't: this specific station's hardware quirks, and days-long
uptime.
