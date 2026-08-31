# Real-feeder trial runbook

How to put mlatd in front of one real feeder, non-exclusively, as a
protocol-survival trial. One command on each host.

## On the server host

```sh
git clone <this repo> && cd mlat-bench/candidate
MB_BIND=<private-interface-ip> docker compose up -d --build
docker logs -f mlatd        # expect: listening on 0.0.0.0:31090
```

`MB_BIND` binds the mlat port to a private interface (a VPN or tailnet
address), never the public one: the port receives receiver coordinates.

## On the receiver

Add one line to the feeder configuration (readsb/ultrafeeder
`ULTRAFEEDER_CONFIG`):

```
mlat,<server-host>,31090,uuid=<station-uuid>
```

This feed is not exclusive; every aggregator the station already feeds
continues to receive everything.

## Expected result, first hour

- mlatd log: `<station> connected (dump1090, zlib2)`, then rate_report and
  `start_sending`. The real client sends traffic only after the server
  requests it.
- `work/sync.json` appears. With one receiver, `peers` is empty; sync
  needs two.
- No positions. One receiver cannot multilaterate; a fix needs four. The
  trial tests protocol survival: a connection that stays up for days,
  tolerance of ssync and rate_report, and readsb restarts on the input
  side.

## Already tested without a live station

Five real wiedehopf mlat-clients, fed by `beast-serve` without an SDR, ran
the full pipeline end to end: p50 31 m against capture truth. A live trial
adds what the lab cannot: one specific station's hardware, and uptime
measured in days.
