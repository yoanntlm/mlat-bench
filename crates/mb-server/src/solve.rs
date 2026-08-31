//! TDOA position solve: Gauss-Newton on (lat, lon, t_tx) with altitude fixed
//! from the aircraft's own Mode S altitude replies. Fixing altitude turns a
//! marginal 4-receiver geometry into a well-determined solve — the same
//! trick the production servers lean on.

use mb_core::{Ecef, Geodetic, C_MPS};

pub struct Observation {
    pub rx: Ecef,
    /// Arrival time in the common (reference) timebase, seconds.
    pub t_s: f64,
}

pub struct Solution {
    pub pos: Geodetic,
    /// RMS residual of the fit, seconds.
    pub rms_s: f64,
    /// Kept for logging/tests; not part of the CSV contract.
    #[allow(dead_code)]
    pub iterations: u32,
}

const MAX_ITER: u32 = 15;
/// Accept only fits whose residual is physically credible: 3 µs ≈ 900 m of
/// pseudorange scatter. Anything worse is a bad group or broken sync — a
/// ghost waiting to happen — so it is dropped, not published.
pub const MAX_RMS_S: f64 = 3e-6;

pub fn solve(obs: &[Observation], alt_m: f64, init: Geodetic) -> Option<Solution> {
    if obs.len() < 4 {
        return None;
    }
    let mut lat = init.lat_deg;
    let mut lon = init.lon_deg;
    // t_tx initial: earliest arrival minus a plausible propagation time.
    let t_min = obs.iter().map(|o| o.t_s).fold(f64::INFINITY, f64::min);
    let mut t_tx = t_min - 200e3 / C_MPS;

    let mut rms = f64::INFINITY;
    let mut iters = 0;
    for it in 0..MAX_ITER {
        iters = it + 1;
        let r = residuals(obs, lat, lon, alt_m, t_tx);
        rms = (r.iter().map(|x| x * x).sum::<f64>() / r.len() as f64).sqrt();

        // Numeric Jacobian. Step sizes: ~1 m in position, 0.1 µs in time.
        let dlat = 1e-5;
        let dlon = 1e-5 / lat.to_radians().cos().max(0.2);
        let dt = 1e-7;
        let jl = residuals(obs, lat + dlat, lon, alt_m, t_tx);
        let jo = residuals(obs, lat, lon + dlon, alt_m, t_tx);
        let jt = residuals(obs, lat, lon, alt_m, t_tx + dt);

        // Normal equations for the 3-parameter step (JᵀJ)Δ = −Jᵀr.
        let n = obs.len();
        let mut jtj = [[0.0f64; 3]; 3];
        let mut jtr = [0.0f64; 3];
        for i in 0..n {
            let ji = [
                (jl[i] - r[i]) / dlat,
                (jo[i] - r[i]) / dlon,
                (jt[i] - r[i]) / dt,
            ];
            for a in 0..3 {
                for b in 0..3 {
                    jtj[a][b] += ji[a] * ji[b];
                }
                jtr[a] -= ji[a] * r[i];
            }
        }
        let step = solve3(&jtj, &jtr)?;
        // Clamp: no step larger than ~1 degree / 1 ms — divergence guard.
        let (sl, so, st) = (
            step[0].clamp(-1.0, 1.0),
            step[1].clamp(-1.0, 1.0),
            step[2].clamp(-1e-3, 1e-3),
        );
        lat += sl;
        lon += so;
        t_tx += st;
        if sl.abs() < 1e-9 && so.abs() < 1e-9 && st.abs() < 1e-12 {
            break;
        }
    }
    if !lat.is_finite() || !lon.is_finite() || rms > MAX_RMS_S {
        return None;
    }
    Some(Solution {
        pos: Geodetic {
            lat_deg: lat,
            lon_deg: lon,
            alt_m,
        },
        rms_s: rms,
        iterations: iters,
    })
}

fn residuals(obs: &[Observation], lat: f64, lon: f64, alt_m: f64, t_tx: f64) -> Vec<f64> {
    let p = Geodetic {
        lat_deg: lat,
        lon_deg: lon,
        alt_m,
    }
    .to_ecef();
    obs.iter()
        .map(|o| {
            let d =
                ((p.x - o.rx.x).powi(2) + (p.y - o.rx.y).powi(2) + (p.z - o.rx.z).powi(2)).sqrt();
            (t_tx + d / C_MPS) - o.t_s
        })
        .collect()
}

/// 3×3 linear solve, Cramer's rule (conditioning is fine at this size after
/// the parameter scaling above).
fn solve3(a: &[[f64; 3]; 3], b: &[f64; 3]) -> Option<[f64; 3]> {
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-30 {
        return None;
    }
    let mut out = [0.0; 3];
    for k in 0..3 {
        let mut m = *a;
        for row in 0..3 {
            m[row][k] = b[row];
        }
        let dk = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        out[k] = dk / det;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic exactness: place an aircraft, compute exact arrival times at
    /// 5 receivers, solve, expect meters.
    #[test]
    fn recovers_position() {
        let truth = Geodetic {
            lat_deg: 47.25,
            lon_deg: -1.40,
            alt_m: 6400.0,
        };
        let te = truth.to_ecef();
        let rxs = [
            (47.2181, -1.5528, 40.0),
            (47.4802, -1.0511, 85.0),
            (46.9433, -1.1002, 60.0),
            (47.0821, -2.0107, 25.0),
            (47.5934, -1.7523, 110.0),
        ];
        let t_tx = 123.456;
        let obs: Vec<Observation> = rxs
            .iter()
            .map(|&(la, lo, al)| {
                let r = Geodetic {
                    lat_deg: la,
                    lon_deg: lo,
                    alt_m: al,
                }
                .to_ecef();
                let d = ((te.x - r.x).powi(2) + (te.y - r.y).powi(2) + (te.z - r.z).powi(2)).sqrt();
                Observation {
                    rx: r,
                    t_s: t_tx + d / C_MPS,
                }
            })
            .collect();
        let init = Geodetic {
            lat_deg: 47.2,
            lon_deg: -1.5,
            alt_m: truth.alt_m,
        };
        let s = solve(&obs, truth.alt_m, init).expect("solves");
        let err = s.pos.haversine_m(&truth);
        assert!(err < 1.0, "err {err} m after {} iters", s.iterations);
        assert!(s.rms_s < 1e-9);
    }

    /// With 100 ns timing noise the solve should land within ~100 m and
    /// report a credible rms.
    #[test]
    fn tolerates_timing_noise() {
        let truth = Geodetic {
            lat_deg: 47.25,
            lon_deg: -1.40,
            alt_m: 6400.0,
        };
        let te = truth.to_ecef();
        let rxs = [
            (47.2181, -1.5528, 40.0),
            (47.4802, -1.0511, 85.0),
            (46.9433, -1.1002, 60.0),
            (47.0821, -2.0107, 25.0),
            (47.5934, -1.7523, 110.0),
        ];
        // Fixed pseudo-noise, ±100 ns.
        let noise = [70e-9, -90e-9, 40e-9, -20e-9, 85e-9];
        let obs: Vec<Observation> = rxs
            .iter()
            .zip(noise)
            .map(|(&(la, lo, al), dn)| {
                let r = Geodetic {
                    lat_deg: la,
                    lon_deg: lo,
                    alt_m: al,
                }
                .to_ecef();
                let d = ((te.x - r.x).powi(2) + (te.y - r.y).powi(2) + (te.z - r.z).powi(2)).sqrt();
                Observation {
                    rx: r,
                    t_s: 5.0 + d / C_MPS + dn,
                }
            })
            .collect();
        let init = Geodetic {
            lat_deg: 47.3,
            lon_deg: -1.3,
            alt_m: truth.alt_m,
        };
        let s = solve(&obs, truth.alt_m, init).expect("solves");
        let err = s.pos.haversine_m(&truth);
        assert!(err < 200.0, "err {err} m");
    }
}
