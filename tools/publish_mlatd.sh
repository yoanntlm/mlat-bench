#!/usr/bin/env bash
# Publish the server crates into the standalone mlatd repo (../mlatd).
#
# Until the public launch this workspace is the dev home for mlatd — the
# bench-driven loop (edit, replay, score) is what made the server good, and
# splitting the crates out would break it. The mlatd repo is the operator-
# facing product view, regenerated from here by this script; everything NOT
# under crates/ (README, Dockerfile, docs, CI) is authored in the mlatd repo
# itself and never touched here. Same canonical-vs-generated pattern as the
# monorepo's network/web -> site/network.
#
# At launch the direction flips: crates move out for good, this repo gains a
# git dependency, and this script is deleted.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
dest="${1:-$here/../mlatd}"
[ -d "$dest/.git" ] || { echo "no git repo at $dest" >&2; exit 1; }

for c in mb-core mb-modes mb-proto mlatd; do
    rsync -a --delete "$here/crates/$c/" "$dest/crates/$c/"
done
rsync -a "$here/rust-toolchain.toml" "$dest/rust-toolchain.toml"

echo "synced -> $dest"
git -C "$dest" status --short
echo "review, then commit in $dest (plain authorship, no AI trailers)"
