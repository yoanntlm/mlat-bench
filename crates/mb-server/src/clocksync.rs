//! Pairwise receiver clock synchronization.
//!
//! v0 keeps the oracle's overall approach (pairwise models from shared DF17
//! sync pairs) in its simplest defensible form: a windowed linear fit
//! t_to = α + β·t_from per directed receiver pair. The known weakness — no
//! sync graph traversal, pairs must sync directly with the reference — is
//! deliberate scope, recorded in the README. The bench decides whether it's
//! good enough; opinions don't.

/// One directed pair's model, fit over a sliding window of shared
/// transmit-time observations.
#[derive(Default)]
pub struct PairModel {
    /// (t_from_s, t_to_s) — same physical transmission expressed in each
    /// receiver's (propagation-corrected) clock.
    obs: Vec<(f64, f64)>,
}

/// Model quality gates: below these, conversions are refused rather than
/// guessed. Tuned on the bench, not by feel.
const MIN_OBS: usize = 8;
const MIN_SPAN_S: f64 = 5.0;
const WINDOW_S: f64 = 180.0;
const MAX_OBS: usize = 400;
/// Refuse conversions whose prediction sigma exceeds this — a poisoned
/// observation is worse than a missing one (bench: minute-1 and minute-9
/// tails, seeds 42 and 1337).
const MAX_PRED_SIGMA_S: f64 = 2e-6;

impl PairModel {
    pub fn push(&mut self, t_from: f64, t_to: f64) {
        self.obs.push((t_from, t_to));
        // Expire by from-clock age; cap for memory.
        let cutoff = t_from - WINDOW_S;
        self.obs.retain(|(a, _)| *a >= cutoff);
        if self.obs.len() > MAX_OBS {
            let excess = self.obs.len() - MAX_OBS;
            self.obs.drain(..excess);
        }
    }

    /// Convert a from-clock reading to the to-clock, if the model is sound.
    /// Returns the converted time AND the fit's own residual RMS — the honest
    /// per-pair timing error, which downstream weighting and gating need far
    /// more than any per-clock-type constant (bench evidence: the est column
    /// underestimated ~2-3x during sync-noise bursts with constant errors).
    pub fn convert(&self, t_from: f64) -> Option<(f64, f64)> {
        let n = self.obs.len();
        if n < MIN_OBS {
            return None;
        }
        let span = self.obs.last()?.0 - self.obs.first()?.0;
        if span < MIN_SPAN_S {
            return None;
        }
        let mean_a: f64 = self.obs.iter().map(|(a, _)| a).sum::<f64>() / n as f64;
        let mean_b: f64 = self.obs.iter().map(|(_, b)| b).sum::<f64>() / n as f64;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for (a, b) in &self.obs {
            let da = a - mean_a;
            sxx += da * da;
            sxy += da * (b - mean_b);
        }
        if sxx <= 0.0 {
            return None;
        }
        let beta = sxy / sxx;
        let alpha = mean_b - beta * mean_a;
        let sigma_fit = (self
            .obs
            .iter()
            .map(|(a, b)| {
                let e = b - (alpha + beta * a);
                e * e
            })
            .sum::<f64>()
            / n as f64)
            .sqrt();
        // PREDICTION interval, not fit residual: the (t−t̄)²/Sxx term blows
        // up exactly when the model is young or extrapolating past its
        // window — the two phases where the bench measured km-scale errors
        // hiding behind a 24 m fit sigma. Honest sigma lets the covariance
        // gate and accuracy throttle downstream do their jobs.
        let da = t_from - mean_a;
        let infl = (1.0 + 1.0 / n as f64 + da * da / sxx).sqrt();
        let sigma_pred = sigma_fit * infl;
        if sigma_pred > MAX_PRED_SIGMA_S {
            return None; // conversion too uncertain to contribute at all
        }
        Some((alpha + beta * t_from, sigma_pred))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_offset_and_drift() {
        // to-clock runs 10 ppm fast with 0.5 ms offset.
        let mut m = PairModel::default();
        for i in 0..40 {
            let t = i as f64;
            m.push(t, 0.0005 + t * (1.0 + 10e-6));
        }
        let (got, sigma) = m.convert(100.0).unwrap();
        let want = 0.0005 + 100.0 * (1.0 + 10e-6);
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        assert!(sigma < 1e-9, "perfect data must fit tightly: {sigma}");
    }

    #[test]
    fn refuses_thin_models() {
        let mut m = PairModel::default();
        m.push(0.0, 0.0);
        m.push(0.1, 0.1);
        assert!(
            m.convert(5.0).is_none(),
            "2 points over 0.1 s is not a model"
        );
    }
}
