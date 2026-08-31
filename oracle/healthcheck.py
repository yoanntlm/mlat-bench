#!/usr/bin/env python3
"""Healthcheck: the client port accepts TCP. Deliberately does NOT handshake —
a health probe that registers as a receiver would pollute the server's state
and the run's metrics. The orchestrator does one real probe handshake at
startup instead (and its user string marks it as a probe)."""
import os
import socket
import sys

port = int(os.environ.get("ORACLE_CLIENT_PORT", "40147"))
try:
    with socket.create_connection(("127.0.0.1", port), timeout=3):
        sys.exit(0)
except OSError:
    sys.exit(1)
