# The oracle: wiedehopf/mlat-server, built from upstream source at a pinned
# commit. This container is the ONLY place AGPL code lives in this project —
# the harness never vendors or links it (see README, Licensing boundary).
#
# Pin bumps are deliberate, single-commit changes: the whole point of the
# harness is comparing runs, and a silent oracle upgrade invalidates every
# comparison across the bump.

FROM python:3.11-slim-trixie AS build

# 9b27a6d = "improve force_mlat logic", 2026-04-03 — master as of 2026-08-31.
ARG MLAT_SERVER_COMMIT=9b27a6d609c5bb47549016412222a79ecb0f48e4

RUN apt-get update && apt-get install -y --no-install-recommends \
        git gcc g++ libc6-dev \
    && rm -rf /var/lib/apt/lists/*

RUN git clone https://github.com/wiedehopf/mlat-server /opt/mlat-server \
    && cd /opt/mlat-server \
    && git checkout ${MLAT_SERVER_COMMIT}

# Dependency set per upstream README. Cython modules (clocktrack, geodesy)
# compile at install time — that's what the compilers above are for.
# setuptools <81: python-graph-core still imports pkg_resources, which
# setuptools removed in 2025 (observed failing 2026-08-31 with 82.x).
RUN python -m venv /opt/venv \
    && /opt/venv/bin/pip install --no-cache-dir -U pip "setuptools<81" wheel \
    && /opt/venv/bin/pip install --no-cache-dir \
        numpy scipy pykalman uvloop ujson Cython python-graph-core \
    && cd /opt/mlat-server \
    && /opt/venv/bin/python setup.py build_ext --inplace

FROM python:3.11-slim-trixie

# libfaketime enables faster-than-real-time replay: the server's grouping
# window (MLAT_DELAY, 0.9 s of ITS wall clock) is why real-time replay is
# otherwise mandatory. Speeding the container clock by the same factor as the
# replay keeps every internal ratio intact. Verified against a real-time run
# of the same capture — see docs/protocol-notes.md.
RUN apt-get update && apt-get install -y --no-install-recommends libfaketime \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /opt/venv /opt/venv
COPY --from=build /opt/mlat-server /opt/mlat-server
COPY entrypoint.sh /entrypoint.sh
COPY healthcheck.py /healthcheck.py

# Upstream README: keep the numeric libraries single-threaded — the server is
# asyncio-structured and thread pools only add jitter to what we measure.
ENV MKL_NUM_THREADS=1 NUMEXPR_NUM_THREADS=1 OMP_NUM_THREADS=1

WORKDIR /opt/mlat-server
ENTRYPOINT ["/entrypoint.sh"]
