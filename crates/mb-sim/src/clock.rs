//! Receiver clock models: true reception time → wire counter value.

use crate::scenario::ClockSpec;
use rand::Rng;
use rand_chacha::ChaCha12Rng;

/// 48-bit wrap, like the Beast/dump1090 counter (protocol-notes: wire
/// timestamps are raw counts; the server treats backwards jumps as resets,
/// so we also enforce per-connection monotonicity below).
const WRAP_48: u64 = 1 << 48;

pub struct ClockModel {
    freq_hz: f64,
    offset: f64,      // fractional frequency offset (ppm * 1e-6)
    drift_per_s: f64, // fractional frequency change per second
    jitter_s: f64,    // 1σ gaussian, seconds
    start_count: u64,
    last_count: Option<u64>,
    wraps: bool,
    // Hostility: frequency random walk (thermal wander) and counter jumps.
    // Draw from a dedicated per-clock stream so the polite-lab determinism
    // property (wander == 0, jumps == 0) is untouched.
    wander_per_sqrt_s: f64, // fractional, per sqrt(second)
    jump_prob_per_s: f64,
    hostile_rng: Option<ChaCha12Rng>,
    wander_accum: f64,   // accumulated fractional offset from the walk
    wander_phase_s: f64, // integrated extra phase from the walk
    last_t: f64,
}

impl ClockModel {
    pub fn new(spec: &ClockSpec) -> Self {
        match *spec {
            ClockSpec::Dump1090 {
                offset_ppm,
                drift_ppm_per_hr,
                jitter_ns,
                start_count,
                wander_ppm_sqrt_hr,
                jump_prob_per_min,
            } => ClockModel {
                freq_hz: 12e6,
                offset: offset_ppm * 1e-6,
                drift_per_s: drift_ppm_per_hr * 1e-6 / 3600.0,
                jitter_s: jitter_ns * 1e-9,
                start_count,
                last_count: None,
                wraps: true,
                wander_per_sqrt_s: wander_ppm_sqrt_hr * 1e-6 / 60.0,
                jump_prob_per_s: jump_prob_per_min / 60.0,
                hostile_rng: None,
                wander_accum: 0.0,
                wander_phase_s: 0.0,
                last_t: 0.0,
            },
            ClockSpec::RadarcapeGps { jitter_ns } => ClockModel {
                freq_hz: 1e9,
                offset: 0.0,
                drift_per_s: 0.0,
                jitter_s: jitter_ns * 1e-9,
                start_count: 0,
                last_count: None,
                wraps: false,
                wander_per_sqrt_s: 0.0,
                jump_prob_per_s: 0.0,
                hostile_rng: None,
                wander_accum: 0.0,
                wander_phase_s: 0.0,
                last_t: 0.0,
            },
        }
    }

    /// Arm the hostility stream (only clocks with wander/jumps need one).
    pub fn set_hostile_rng(&mut self, rng: rand_chacha::ChaCha12Rng) {
        self.hostile_rng = Some(rng);
    }

    pub fn is_hostile(&self) -> bool {
        self.wander_per_sqrt_s > 0.0 || self.jump_prob_per_s > 0.0
    }

    pub const fn wire_clock_type(spec: &ClockSpec) -> &'static str {
        match spec {
            ClockSpec::Dump1090 { .. } => "dump1090",
            ClockSpec::RadarcapeGps { .. } => "radarcape_gps",
        }
    }

    /// Counter value for a reception at true time t (seconds since T0).
    /// Integrated phase: freq · (t + offset·t + ½·drift·t²) + jitter.
    /// Calls must be in nondecreasing t order per clock (receptions are
    /// processed sorted); monotonicity is clamped so ns-scale jitter can
    /// never make the wire counter step backwards.
    pub fn count_at(&mut self, t_s: f64, rng: &mut ChaCha12Rng) -> u64 {
        self.count_at_hostile(t_s, rng).0
    }

    /// Like count_at, also reporting whether a counter jump happened at this
    /// reception (the client then sends clock_jump, as real mlat-client does).
    pub fn count_at_hostile(&mut self, t_s: f64, rng: &mut ChaCha12Rng) -> (u64, bool) {
        let mut jumped = false;
        let dt = (t_s - self.last_t).max(0.0);
        self.last_t = t_s;
        if let Some(h) = self.hostile_rng.as_mut() {
            if self.wander_per_sqrt_s > 0.0 && dt > 0.0 {
                // Integrate the random walk: offset does a gaussian step
                // scaled by sqrt(dt); phase accumulates the current offset.
                self.wander_phase_s += self.wander_accum * dt;
                self.wander_accum += gaussian(h) * self.wander_per_sqrt_s * dt.sqrt();
            }
            if self.jump_prob_per_s > 0.0 && h.gen_range(0.0f64..1.0) < self.jump_prob_per_s * dt {
                // ±1..100 ms worth of counts, either direction.
                let ms = h.gen_range(1.0f64..100.0)
                    * if h.gen_range(0.0f64..1.0) < 0.5 {
                        -1.0
                    } else {
                        1.0
                    };
                self.wander_phase_s += ms / 1000.0;
                self.last_count = None; // jump breaks monotonic continuity
                jumped = true;
            }
        }
        let jitter = gaussian(rng) * self.jitter_s;
        let phase_s = t_s * (1.0 + self.offset)
            + 0.5 * self.drift_per_s * t_s * t_s
            + self.wander_phase_s
            + jitter;
        let raw = self.start_count as f64 + phase_s * self.freq_hz;
        let mut count = if self.wraps {
            (raw.round() as i128).rem_euclid(WRAP_48 as i128) as u64
        } else {
            raw.round().max(0.0) as u64
        };
        if let Some(prev) = self.last_count {
            // Monotonic clamp — but never across a legitimate 48-bit wrap.
            let wrapped = self.wraps && prev > WRAP_48 - (self.freq_hz as u64) && count < prev;
            if !wrapped && count <= prev {
                count = prev + 1;
            }
        }
        self.last_count = Some(count);
        (count, jumped)
    }
}

/// Box-Muller from two uniform draws; avoids a rand_distr dependency for
/// one gaussian.
fn gaussian(rng: &mut ChaCha12Rng) -> f64 {
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_core::rng_for;

    fn spec(offset_ppm: f64) -> ClockSpec {
        ClockSpec::Dump1090 {
            offset_ppm,
            drift_ppm_per_hr: 0.0,
            jitter_ns: 0.0,
            start_count: 0,
            wander_ppm_sqrt_hr: 0.0,
            jump_prob_per_min: 0.0,
        }
    }

    #[test]
    fn nominal_frequency() {
        let mut c = ClockModel::new(&spec(0.0));
        let mut rng = rng_for(1, "t");
        assert_eq!(c.count_at(1.0, &mut rng), 12_000_000);
        assert_eq!(c.count_at(10.0, &mut rng), 120_000_000);
    }

    #[test]
    fn offset_accumulates() {
        // +100 ppm: after 100 s the clock is 10 ms (120000 counts) ahead.
        let mut c = ClockModel::new(&spec(100.0));
        let mut rng = rng_for(1, "t");
        let n = c.count_at(100.0, &mut rng);
        assert_eq!(n, 1_200_120_000);
    }

    #[test]
    fn drift_is_quadratic() {
        let mut c = ClockModel::new(&ClockSpec::Dump1090 {
            offset_ppm: 0.0,
            drift_ppm_per_hr: 3600.0, // 1 ppm per second — huge, for arithmetic clarity
            jitter_ns: 0.0,
            start_count: 0,
            wander_ppm_sqrt_hr: 0.0,
            jump_prob_per_min: 0.0,
        });
        let mut rng = rng_for(1, "t");
        // phase = t + 0.5 * 1e-6 * t^2 ; t=100 → +0.005 s → +60000 counts
        assert_eq!(c.count_at(100.0, &mut rng), 1_200_060_000);
    }

    #[test]
    fn wraps_at_48_bits() {
        let mut c = ClockModel::new(&ClockSpec::Dump1090 {
            offset_ppm: 0.0,
            drift_ppm_per_hr: 0.0,
            jitter_ns: 0.0,
            start_count: (1u64 << 48) - 6_000_000, // 0.5 s before wrap
            wander_ppm_sqrt_hr: 0.0,
            jump_prob_per_min: 0.0,
        });
        let mut rng = rng_for(1, "t");
        let before = c.count_at(0.25, &mut rng);
        let after = c.count_at(1.0, &mut rng);
        assert!(before > after, "counter must wrap: {before} then {after}");
        assert_eq!(after, 6_000_000);
    }

    #[test]
    fn monotonic_under_jitter() {
        let mut c = ClockModel::new(&ClockSpec::Dump1090 {
            offset_ppm: 0.0,
            drift_ppm_per_hr: 0.0,
            jitter_ns: 500.0,
            start_count: 0,
            wander_ppm_sqrt_hr: 0.0,
            jump_prob_per_min: 0.0,
        });
        let mut rng = rng_for(1, "t");
        let mut prev = 0;
        // Receptions 1 µs apart — jitter (500 ns σ) would reorder freely
        // without the clamp.
        for i in 1..2000 {
            let n = c.count_at(i as f64 * 1e-6, &mut rng);
            assert!(n > prev, "step {i}: {n} <= {prev}");
            prev = n;
        }
    }
}
