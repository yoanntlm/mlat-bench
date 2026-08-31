#!/bin/sh
# Fetch a LocaRDS subset (real OpenSky receivers + published truth, CC BY-SA)
# and unpack it. ~450 MB per subset; Zenodo can be slow.
#
#   sh tools/fetch_locards.sh [subset_number] [dest_dir]
#
# Then: mlat-bench import-locards <dest>/subset_N/set_N.csv \
#           <dest>/subset_N/set_N_sensors.csv -o capture/ --duration-s 600
set -eu

N="${1:-1}"
DEST="${2:-locards}"
mkdir -p "$DEST"
ZIP="$DEST/subset_${N}.zip"
[ -f "$ZIP" ] || curl -L -o "$ZIP" \
    "https://zenodo.org/records/4739276/files/subset_${N}.zip?download=1"
unzip -o -q "$ZIP" -d "$DEST"
echo "ready: $DEST/subset_${N}/"
