#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib"]
# ///
"""Plots for a scored mlat-bench run.

    uv run plots/plot.py runs/<run-dir>

Reads metrics.json + oracle-work/results.csv + capture truth, writes
plots/*.png inside the run directory:
  error_cdf.png   — horizontal error CDF
  map.png         — truth tracks vs oracle positions
  resources.png   — oracle CPU% over the run
"""

from __future__ import annotations

import json
import math
import subprocess
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402


def hav_m(lat1, lon1, lat2, lon2):
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dl, dn = math.radians(lat2 - lat1), math.radians(lon2 - lon1)
    a = math.sin(dl / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dn / 2) ** 2
    return 2 * 6371000.8 * math.asin(math.sqrt(a))


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    run = Path(sys.argv[1])
    out = run / "plots"
    out.mkdir(exist_ok=True)

    metrics = json.loads((run / "metrics.json").read_text())
    run_json = json.loads((run / "run.json").read_text())
    wall_t0 = run_json["wall_t0"]
    capture = Path(run_json.get("capture", run / "capture"))
    if not capture.exists():
        capture = run / "capture"

    # Truth via system zstd (keeps the script dependency-light).
    truth_lines = subprocess.run(
        ["zstd", "-dc", str(capture / "truth.jsonl.zst")],
        capture_output=True,
        check=True,
        text=True,
    ).stdout.splitlines()
    truth: dict[int, list[tuple[float, float, float]]] = {}
    for line in truth_lines:
        p = json.loads(line)
        truth.setdefault(p["icao"], []).append(
            (p["t"] / 1e9, p["pos"]["lat_deg"], p["pos"]["lon_deg"])
        )

    rows = []
    for line in (run / "oracle-work/results.csv").read_text().splitlines():
        f = line.split(",")
        if len(f) < 10:
            continue
        try:
            rows.append((float(f[0]), int(f[1], 16), float(f[4]), float(f[5])))
        except ValueError:
            continue

    # ---- error CDF -------------------------------------------------------
    def truth_at(icao, t):
        pts = truth.get(icao)
        if not pts or not pts[0][0] <= t <= pts[-1][0]:
            return None
        i = min(int(t - pts[0][0]), len(pts) - 2)
        (t0, la0, lo0), (t1, la1, lo1) = pts[i], pts[i + 1]
        f = (t - t0) / (t1 - t0)
        return la0 + f * (la1 - la0), lo0 + f * (lo1 - lo0)

    errs = []
    for t, icao, lat, lon in rows:
        tp = truth_at(icao, t - wall_t0)
        if tp:
            e = hav_m(lat, lon, tp[0], tp[1])
            if e < 10_000:
                errs.append(e)
    errs.sort()
    if errs:
        fig, ax = plt.subplots(figsize=(7, 4.5))
        ax.plot(errs, [i / len(errs) for i in range(1, len(errs) + 1)])
        ax.set_xscale("log")
        ax.set_xlabel("horizontal error (m)")
        ax.set_ylabel("CDF")
        h = metrics["horizontal_error_m"]
        ax.set_title(f"p50 {h['p50']:.0f} m · p90 {h['p90']:.0f} m · n={h['n']}")
        ax.grid(True, which="both", alpha=0.3)
        fig.tight_layout()
        fig.savefig(out / "error_cdf.png", dpi=130)

    # ---- map -------------------------------------------------------------
    fig, ax = plt.subplots(figsize=(7, 7))
    for icao, pts in truth.items():
        ax.plot([p[2] for p in pts], [p[1] for p in pts], "-", lw=0.7, alpha=0.5, color="gray")
    mlat_icaos = {r[1] for r in rows}
    for icao in mlat_icaos:
        pts = [(lat, lon) for (t, i, lat, lon) in rows if i == icao]
        ax.plot(
            [p[1] for p in pts],
            [p[0] for p in pts],
            ".",
            ms=3,
            label=f"{icao:06x} ({len(pts)})",
        )
    ax.set_xlabel("lon")
    ax.set_ylabel("lat")
    ax.set_title("truth tracks (gray) vs oracle MLAT positions")
    ax.legend(fontsize=8)
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out / "map.png", dpi=130)

    # ---- resources -------------------------------------------------------
    samples = []
    res_path = run / "resources.jsonl"
    if res_path.exists():
        for line in res_path.read_text().splitlines():
            v = json.loads(line)
            if v.get("cpu_usec") is not None:
                samples.append((v["t"], v["cpu_usec"]))
    if len(samples) > 2:
        ts = [s[0] - samples[0][0] for s in samples[1:]]
        cpu = [
            (b[1] - a[1]) / ((b[0] - a[0]) * 1e6) * 100
            for a, b in zip(samples, samples[1:])
        ]
        fig, ax = plt.subplots(figsize=(7, 3.5))
        ax.plot(ts, cpu)
        ax.set_xlabel("run time (s)")
        ax.set_ylabel("oracle CPU %")
        ax.grid(True, alpha=0.3)
        fig.tight_layout()
        fig.savefig(out / "resources.png", dpi=130)

    print(f"plots in {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
