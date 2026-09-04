#!/usr/bin/env bash
# Publish the client crates into the standalone mlatc repo (../mlatc).
#
# This workspace is the dev home for mlatc; the bench-driven loop (edit,
# replay, score) is what proves the client. The mlatc repo is the
# feeder-facing product view, regenerated from here by this script;
# everything NOT under crates/ (README, CI, release workflow) is authored
# in the mlatc repo itself and never touched here. Same pattern as
# tools/publish_mlatd.sh.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
dest="${1:-$here/../mlatc}"
[ -d "$dest/.git" ] || { echo "no git repo at $dest" >&2; exit 1; }

for c in mb-core mb-modes mb-proto mlatc; do
    rsync -a --delete "$here/crates/$c/" "$dest/crates/$c/"
done
rsync -a "$here/rust-toolchain.toml" "$dest/rust-toolchain.toml"

echo "synced -> $dest"
git -C "$dest" status --short
echo "review, then commit in $dest (plain authorship, no AI trailers)"
