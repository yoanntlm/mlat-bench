# Real-feeder trial runbook

Status 2026-08-31: staged. Waiting for the hub host (leserveur) to return
to the tailnet. Then the trial is one command on each host.

## On the hub (leserveur)

```sh
git clone git@github.com:yoanntlm/mlat-bench && cd mlat-bench/candidate
MB_BIND=100.114.32.128 docker compose up -d --build
docker logs -f mlatd        # expect: listening on 0.0.0.0:31090
```

`MB_BIND` binds the mlat port to the tailnet interface only, never the
public one.

## On the receiver (station #1)

`receiver/docker-compose.yml` carries the disarmed line (monorepo commit
9e899eb):

```
# ;mlat,100.114.32.128,31090,uuid=${UUID_FLIGHTPORTRAIT}
```

Uncomment it (the line above it ends with a `;` continuation), then
`docker compose up -d`. This feed is not exclusive: adsb.lol,
airplanes.live, and adsb.fi continue to receive everything.

## Expected result, first hour

- mlatd log: `<station> connected (dump1090, zlib2)`, then rate_report and
  `start_sending`. The real client sends traffic only after the server
  requests it.
- `work/sync.json` appears. With one receiver, `peers` is empty; sync
  needs two.
- No positions. One receiver cannot multilaterate; MLAT needs four
  receivers per fix. The trial tests protocol survival: a connection that
  stays up for days, tolerance of ssync and rate_report, and readsb
  restarts on the input side.

## Already tested without this station

Five real wiedehopf mlat-clients, fed by `beast-serve` without an SDR, ran
the full pipeline end to end: p50 31 m against capture truth. The trial
adds what the lab cannot: this station's hardware, and uptime measured in
days.
