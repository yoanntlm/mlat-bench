//! Scenario model and deterministic event generation.
//!
//! Pipeline (offline, pure): trajectories → truth log → emission schedule →
//! per-receiver audibility, loss, propagation delay, clock mapping →
//! sync/mlat message stream → framed timestamped byte records.

pub mod clock;
pub mod gen;
pub mod scenario;
pub mod traj;

pub use gen::{generate, AudibilityRow, ClientStream, GeneratedCapture, SendRecord};
pub use scenario::Scenario;

#[cfg(test)]
mod tests {
    use super::*;

    fn smoke_toml() -> &'static str {
        r#"
[meta]
name = "test"
seed = 7
duration_s = 60

[[receivers]]
id = "rx-000"
lat = 47.21
lon = -1.55
alt_m = 40
clock = { type = "dump1090", offset_ppm = 3.2, jitter_ns = 40 }

[[receivers]]
id = "rx-001"
lat = 47.35
lon = -1.20
alt_m = 90
clock = { type = "radarcape_gps" }
compress = "zlib2"

[[aircraft]]
icao = "3C6444"
kind = "adsb"
traj = { type = "great_circle", from = [46.9, -2.1, 35000], to = [47.6, -0.9, 35000], gs_kts = 450 }

[[aircraft]]
icao = "3944F1"
kind = "modes_only"
traj = { type = "great_circle", from = [47.5, -1.9, 20000], to = [47.0, -1.0, 20000], gs_kts = 380 }
"#
    }

    #[test]
    fn parses_and_generates() {
        let sc = Scenario::from_toml(smoke_toml()).unwrap();
        let cap = generate(&sc).unwrap();
        assert_eq!(cap.clients.len(), 2);
        assert_eq!(cap.truth.len(), 2 * 60);
        // Both aircraft overfly the receivers at altitude: everything audible.
        let c0 = &cap.clients[0];
        assert!(c0.sync_count > 20, "sync pairs: {}", c0.sync_count);
        assert!(c0.mlat_count > 100, "mlat msgs: {}", c0.mlat_count);
        assert!(!c0.records.is_empty());
        // Records are time-ordered.
        for w in c0.records.windows(2) {
            assert!(w[0].t <= w[1].t);
        }
    }

    #[test]
    fn deterministic_across_runs() {
        let sc = Scenario::from_toml(smoke_toml()).unwrap();
        let a = generate(&sc).unwrap();
        let b = generate(&sc).unwrap();
        for (ca, cb) in a.clients.iter().zip(&b.clients) {
            assert_eq!(ca.records.len(), cb.records.len());
            for (ra, rb) in ca.records.iter().zip(&cb.records) {
                assert_eq!(ra.t, rb.t);
                assert_eq!(ra.bytes, rb.bytes, "client {}", ca.id);
            }
        }
    }

    #[test]
    fn adding_aircraft_does_not_perturb_existing_streams() {
        // The domain-separated RNG promise, end to end: rx-000's sync lines
        // for 3C6444 must be identical with and without a third aircraft.
        let sc1 = Scenario::from_toml(smoke_toml()).unwrap();
        let extra = smoke_toml().to_string()
            + r#"
[[aircraft]]
icao = "AAAAAA"
kind = "modes_only"
traj = { type = "great_circle", from = [47.4, -1.6, 10000], to = [47.2, -1.2, 10000], gs_kts = 300 }
"#;
        let sc2 = Scenario::from_toml(&extra).unwrap();
        let a = generate(&sc1).unwrap();
        let b = generate(&sc2).unwrap();

        let sync_lines = |cap: &GeneratedCapture| -> Vec<Vec<u8>> {
            // Client 0 uses compress=none: records are raw JSON lines.
            cap.clients[0]
                .records
                .iter()
                .filter(|r| r.bytes.starts_with(b"{\"sync\""))
                .map(|r| r.bytes.clone())
                .collect()
        };
        assert_eq!(sync_lines(&a), sync_lines(&b));
    }
}
