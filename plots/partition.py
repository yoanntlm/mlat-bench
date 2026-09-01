#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib"]
# ///
"""Draw the shard partition: cells colored by owning shard.

    uv run plots/partition.py <work-dir or partition.json> [out.png]

Reads the partition.json that mlatd writes into its work dir every 15 s.
Each rectangle is a cell; the color is its shard; the label is the number
of receivers assigned through it. Split cells appear as smaller
rectangles inside their parent's square.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.patches import Rectangle  # noqa: E402


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    src = Path(sys.argv[1])
    if src.is_dir():
        src = src / "partition.json"
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else src.with_suffix(".png")
    cells = json.loads(src.read_text())
    if not cells:
        sys.exit("empty partition")

    fig, ax = plt.subplots(figsize=(12, 8))
    cmap = plt.get_cmap("tab20")
    # Draw coarse cells first so split children paint on top.
    for c in sorted(cells, key=lambda c: c["level"]):
        color = cmap(c["shard"] % 20)
        ax.add_patch(
            Rectangle(
                (c["lon0"], c["lat0"]),
                c["lon1"] - c["lon0"],
                c["lat1"] - c["lat0"],
                facecolor=color,
                alpha=0.55,
                edgecolor="black",
                linewidth=0.6 + 0.4 * c["level"],
            )
        )
        ax.annotate(
            f"s{c['shard']}\n{c['rx']}rx",
            ((c["lon0"] + c["lon1"]) / 2, (c["lat0"] + c["lat1"]) / 2),
            ha="center",
            va="center",
            fontsize=6,
        )
    ax.set_xlim(min(c["lon0"] for c in cells) - 2, max(c["lon1"] for c in cells) + 2)
    ax.set_ylim(min(c["lat0"] for c in cells) - 2, max(c["lat1"] for c in cells) + 2)
    ax.set_xlabel("longitude")
    ax.set_ylabel("latitude")
    n_shards = len({c["shard"] for c in cells})
    ax.set_title(f"shard partition: {len(cells)} cells, {n_shards} shards")
    ax.set_aspect("equal")
    fig.tight_layout()
    fig.savefig(out, dpi=140)
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
