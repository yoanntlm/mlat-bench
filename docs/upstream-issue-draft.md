Title: NaN var_est raises ValueError in mlattrack._resolve, dropping all
pending groups in the processing cycle

While replaying a recorded 316-receiver session (LocaRDS-derived) against
mlat-server @ 9b27a6d, we hit a repeatable crash-loop:

    File "mlat/mlattrack.py", line 324, in _resolve
      error = int(math.sqrt(abs(var_est)))
    ValueError: cannot convert float NaN to integer

var_est = numpy.trace(ecef_cov) can be NaN for a degenerate solve. Because
_process handles groups via a list comprehension
([group.handle(group) for group in self.groups], mlattrack.py:58), the
exception discards every remaining group in that cycle, not just the
degenerate one — under sustained load this collapsed our run's output
almost entirely (server otherwise healthy: all clients synced, ~1100
aircraft tracked).

Suggested minimal fix, mirroring the existing "result is suspect" path:

    if not numpy.isfinite(var_est):
        continue
    error = int(math.sqrt(abs(var_est)))

Happy to share the replay harness + capture that reproduces this.
