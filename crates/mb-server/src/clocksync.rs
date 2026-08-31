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
    /// receiver's (propagation-corrected) clock. Deque: expiry pops the
    /// front, amortized O(1) — the old retain() scan per push was ~40% of a
    /// core at metro scale.
    obs: std::collections::VecDeque<(f64, f64)>,
    /// Cached trimmed fit + staleness counter. At metro scale convert() is
    /// called once per observation per solve; refitting 400 points each time
    /// saturated a core (bench: 104% CPU at 5×). A fit a few observations
    /// stale is statistically identical.
    cached: Option<Fit>,
    stale: u32,
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
/// Reuse a cached fit until this many new observations arrive.
const REFIT_EVERY: u32 = 8;

impl PairModel {
    /// (observation count, estimated pairwise offset ppm) for status export
    /// (sync.json — existing monitoring tools read the oracle's shape).
    pub fn status(&self) -> (usize, f64) {
        let ppm = self
            .cached
            .as_ref()
            .map(|f| (f.beta - 1.0) * 1e6)
            .unwrap_or(0.0);
        (self.obs.len(), ppm)
    }

    pub fn push(&mut self, t_from: f64, t_to: f64) {
        self.obs.push_back((t_from, t_to));
        self.stale += 1;
        let cutoff = t_from - WINDOW_S;
        while self.obs.front().is_some_and(|(a, _)| *a < cutoff) {
            self.obs.pop_front();
        }
        while self.obs.len() > MAX_OBS {
            self.obs.pop_front();
        }
    }

    /// Convert a from-clock reading to the to-clock, if the model is sound.
    /// Returns the converted time AND the prediction-interval sigma.
    ///
    /// The fit is TRIMMED: fit once, drop observations whose residual exceeds
    /// max(3×RMS, 500 ns), refit with the survivors. A lying sync source
    /// (bad navigation → wrong propagation correction, ±µs correlated error)
    /// or a multipath-stamped reception poisons a democratic fit wholesale —
    /// the hostile bench measured 2× accuracy loss before trimming. The
    /// oracle's clocktrack rejects sync outliers for the same reason.
    pub fn convert(&mut self, t_from: f64) -> Option<(f64, f64)> {
        let n = self.obs.len();
        if n < MIN_OBS {
            return None;
        }
        let span = self.obs.back()?.0 - self.obs.front()?.0;
        if span < MIN_SPAN_S {
            return None;
        }
        if self.cached.is_none() || self.stale >= REFIT_EVERY {
            let all: Vec<(f64, f64)> = self.obs.iter().copied().collect();
            let first = fit(&all)?;
            let cut = (3.0 * first.sigma_fit).max(500e-9);
            let kept: Vec<(f64, f64)> = all
                .iter()
                .filter(|(a, b)| (b - (first.alpha + first.beta * a)).abs() <= cut)
                .copied()
                .collect();
            self.cached = Some(if kept.len() >= MIN_OBS && kept.len() < n {
                fit(&kept)?
            } else {
                first
            });
            self.stale = 0;
        }
        let f = self.cached.as_ref().expect("just set");
        let _ = f.n; // (fields also served via status())
                     // Prediction interval: inflates for young or extrapolating models —
                     // exactly the phases where km-scale errors hid behind tiny fit
                     // residuals (bench, seeds 42 + 1337).
        let da = t_from - f.mean_a;
        let infl = (1.0 + 1.0 / f.n as f64 + da * da / f.sxx).sqrt();
        let sigma_pred = f.sigma_fit * infl;
        if sigma_pred > MAX_PRED_SIGMA_S {
            return None; // too uncertain to contribute at all
        }
        Some((f.alpha + f.beta * t_from, sigma_pred))
    }
}

struct Fit {
    alpha: f64,
    beta: f64,
    sigma_fit: f64,
    mean_a: f64,
    sxx: f64,
    n: usize,
}

fn fit(obs: &[(f64, f64)]) -> Option<Fit> {
    let n = obs.len();
    if n < 2 {
        return None;
    }
    let mean_a: f64 = obs.iter().map(|(a, _)| a).sum::<f64>() / n as f64;
    let mean_b: f64 = obs.iter().map(|(_, b)| b).sum::<f64>() / n as f64;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (a, b) in obs {
        let da = a - mean_a;
        sxx += da * da;
        sxy += da * (b - mean_b);
    }
    if sxx <= 0.0 {
        return None;
    }
    let beta = sxy / sxx;
    let alpha = mean_b - beta * mean_a;
    let sigma_fit = (obs
        .iter()
        .map(|(a, b)| {
            let e = b - (alpha + beta * a);
            e * e
        })
        .sum::<f64>()
        / n as f64)
        .sqrt();
    Some(Fit {
        alpha,
        beta,
        sigma_fit,
        mean_a,
        sxx,
        n,
    })
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
    fn trims_poisoned_observations() {
        // Clean linear relation + 10% gross outliers (the liar / multipath).
        // Untrimmed, the fit is dragged µs off; trimmed, conversion stays ns.
        let mut m = PairModel::default();
        for i in 0..50 {
            let t = i as f64;
            let poison = if i % 10 == 3 { 2e-6 } else { 0.0 };
            m.push(t, 0.0005 + t * (1.0 + 10e-6) + poison);
        }
        let (got, sigma) = m.convert(50.0).unwrap();
        let want = 0.0005 + 50.0 * (1.0 + 10e-6);
        assert!(
            (got - want).abs() < 100e-9,
            "residual poison: {} ns",
            (got - want).abs() * 1e9
        );
        assert!(
            sigma < 500e-9,
            "sigma should reflect the clean fit: {sigma}"
        );
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
