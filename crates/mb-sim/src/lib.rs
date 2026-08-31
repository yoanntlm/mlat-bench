//! Scenario model and deterministic event generation. Built out in M2.
//!
//! Pipeline (offline, pure — see plan §mb-sim):
//! trajectories → truth log → emission schedule → per-receiver audibility,
//! loss, propagation delay, clock mapping → sync/mlat message stream →
//! framed timestamped byte records for mb-capture.

pub fn placeholder() {}
