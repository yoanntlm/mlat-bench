#!/bin/sh
# Oracle entrypoint. All ports/paths come from the environment so the compose
# file is the single place they're defined.
set -eu

WORK_DIR="${ORACLE_WORK_DIR:-/work}"
CLIENT_PORT="${ORACLE_CLIENT_PORT:-40147}"
SBS_PORT="${ORACLE_SBS_PORT:-40148}"
SBS_FILTERED_PORT="${ORACLE_SBS_FILTERED_PORT:-40149}"
SPEED="${ORACLE_SPEED:-1}"

if [ "$SPEED" != "1" ]; then
    # Speed multiplier mode: clock starts at real now, ticks SPEED× faster.
    # Monotonic must be faked too — asyncio/uvloop schedule on it.
    export LD_PRELOAD=/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1
    export FAKETIME="+0 x${SPEED}"
    export DONT_FAKE_MONOTONIC=0
    export FAKETIME_DONT_FAKE_MONOTONIC=0
    echo "entrypoint: faketime x${SPEED} enabled"
fi

mkdir -p "$WORK_DIR"

exec /opt/venv/bin/python /opt/mlat-server/mlat-server \
    --work-dir "$WORK_DIR" \
    --client-listen "0.0.0.0:${CLIENT_PORT}" \
    --write-csv "${WORK_DIR}/results.csv" \
    --basestation-listen "0.0.0.0:${SBS_PORT}" \
    --filtered-basestation-listen "0.0.0.0:${SBS_FILTERED_PORT}" \
    --motd "mlat-bench oracle"
