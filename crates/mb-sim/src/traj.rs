//! Trajectory sampling: where an aircraft truly is at time t.

use crate::scenario::Trajectory;
use mb_core::{Ecef, Geodetic};

const KTS_TO_MPS: f64 = 0.514444;
const FT_TO_M: f64 = 0.3048;

impl Trajectory {
    /// True position at t seconds after scenario T0.
    pub fn position_at(&self, t_s: f64) -> Geodetic {
        match self {
            Trajectory::GreatCircle { from, to, gs_kts } => {
                let a = Geodetic {
                    lat_deg: from[0],
                    lon_deg: from[1],
                    alt_m: from[2] * FT_TO_M,
                };
                let b = Geodetic {
                    lat_deg: to[0],
                    lon_deg: to[1],
                    alt_m: to[2] * FT_TO_M,
                };
                // Slerp on the unit sphere; fraction may exceed 1 (the leg
                // continues) — slerp extrapolates cleanly.
                let dist_m = a.haversine_m(&b).max(1.0);
                let f = (gs_kts * KTS_TO_MPS * t_s) / dist_m;
                let p = slerp(&a, &b, f);
                Geodetic {
                    alt_m: a.alt_m + (b.alt_m - a.alt_m) * f.clamp(0.0, 1.0),
                    ..p
                }
            }
            Trajectory::Orbit {
                center,
                radius_m,
                gs_kts,
                clockwise,
            } => {
                // Local tangent plane about the centre: an orbit is a few
                // km across, where the plane and the sphere agree to
                // centimetres.
                let omega = gs_kts * KTS_TO_MPS / radius_m.max(1.0);
                let theta = if *clockwise {
                    -omega * t_s
                } else {
                    omega * t_s
                };
                let (east, north) = (radius_m * theta.cos(), radius_m * theta.sin());
                let m_lat = 111_320.0;
                let m_lon = m_lat * center[0].to_radians().cos().max(0.05);
                Geodetic {
                    lat_deg: center[0] + north / m_lat,
                    lon_deg: center[1] + east / m_lon,
                    alt_m: center[2] * FT_TO_M,
                }
            }
        }
    }

    /// Ground speed in m/s (constant for great-circle legs).
    pub fn gs_mps(&self) -> f64 {
        match self {
            Trajectory::GreatCircle { gs_kts, .. } | Trajectory::Orbit { gs_kts, .. } => {
                gs_kts * KTS_TO_MPS
            }
        }
    }

    /// Vertical rate in m/s at time t (constant along the leg, zero past it).
    pub fn vrate_mps(&self, t_s: f64) -> f64 {
        match self {
            Trajectory::GreatCircle { from, to, gs_kts } => {
                let a = Geodetic {
                    lat_deg: from[0],
                    lon_deg: from[1],
                    alt_m: 0.0,
                };
                let b = Geodetic {
                    lat_deg: to[0],
                    lon_deg: to[1],
                    alt_m: 0.0,
                };
                let dist_m = a.haversine_m(&b).max(1.0);
                let leg_s = dist_m / (gs_kts * KTS_TO_MPS);
                if t_s < leg_s {
                    (to[2] - from[2]) * FT_TO_M / leg_s
                } else {
                    0.0
                }
            }
            Trajectory::Orbit { .. } => 0.0,
        }
    }
}

/// Spherical linear interpolation between two geodetic points (ignoring
/// altitude), by fraction f (may exceed [0,1]).
fn slerp(a: &Geodetic, b: &Geodetic, f: f64) -> Geodetic {
    // Work on the unit sphere: direction vectors from the earth's center.
    let va = unit(a);
    let vb = unit(b);
    let dot = (va.x * vb.x + va.y * vb.y + va.z * vb.z).clamp(-1.0, 1.0);
    let omega = dot.acos();
    if omega < 1e-12 {
        return *a;
    }
    let so = omega.sin();
    let ka = ((1.0 - f) * omega).sin() / so;
    let kb = (f * omega).sin() / so;
    let v = Ecef {
        x: ka * va.x + kb * vb.x,
        y: ka * va.y + kb * vb.y,
        z: ka * va.z + kb * vb.z,
    };
    let lat = v.z.atan2((v.x * v.x + v.y * v.y).sqrt()).to_degrees();
    let lon = v.y.atan2(v.x).to_degrees();
    Geodetic {
        lat_deg: lat,
        lon_deg: lon,
        alt_m: 0.0,
    }
}

fn unit(g: &Geodetic) -> Ecef {
    let lat = g.lat_deg.to_radians();
    let lon = g.lon_deg.to_radians();
    Ecef {
        x: lat.cos() * lon.cos(),
        y: lat.cos() * lon.sin(),
        z: lat.sin(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg() -> Trajectory {
        Trajectory::GreatCircle {
            from: [47.0, -2.0, 30000.0],
            to: [47.5, -1.0, 34000.0],
            gs_kts: 450.0,
        }
    }

    #[test]
    fn starts_at_from_ends_at_to() {
        let t = leg();
        let p0 = t.position_at(0.0);
        assert!((p0.lat_deg - 47.0).abs() < 1e-9);
        assert!((p0.lon_deg + 2.0).abs() < 1e-9);
        assert!((p0.alt_m - 30000.0 * 0.3048).abs() < 0.1);

        // Leg length / speed = arrival time; position there ≈ `to`.
        let a = Geodetic {
            lat_deg: 47.0,
            lon_deg: -2.0,
            alt_m: 0.0,
        };
        let b = Geodetic {
            lat_deg: 47.5,
            lon_deg: -1.0,
            alt_m: 0.0,
        };
        let leg_s = a.haversine_m(&b) / (450.0 * 0.514444);
        let p1 = t.position_at(leg_s);
        assert!((p1.lat_deg - 47.5).abs() < 1e-6, "{}", p1.lat_deg);
        assert!((p1.lon_deg + 1.0).abs() < 1e-6, "{}", p1.lon_deg);
    }

    #[test]
    fn speed_is_constant() {
        let t = leg();
        let p1 = t.position_at(100.0);
        let p2 = t.position_at(101.0);
        let d = Geodetic { alt_m: 0.0, ..p1 }.haversine_m(&Geodetic { alt_m: 0.0, ..p2 });
        assert!((d - 450.0 * 0.514444).abs() < 1.0, "1s step moved {d} m");
    }

    #[test]
    fn an_orbit_keeps_its_radius_and_speed() {
        let t = Trajectory::Orbit {
            center: [47.2, -1.5, 1500.0],
            radius_m: 1500.0,
            gs_kts: 70.0,
            clockwise: false,
        };
        let c = Geodetic {
            lat_deg: 47.2,
            lon_deg: -1.5,
            alt_m: 0.0,
        };
        for &s in &[0.0, 37.0, 150.0, 400.0] {
            let p = Geodetic {
                alt_m: 0.0,
                ..t.position_at(s)
            };
            assert!((p.haversine_m(&c) - 1500.0).abs() < 2.0, "r at {s}");
            let q = Geodetic {
                alt_m: 0.0,
                ..t.position_at(s + 1.0)
            };
            assert!(
                (p.haversine_m(&q) - 70.0 * KTS_TO_MPS).abs() < 0.5,
                "v at {s}"
            );
        }
        // Starts due east, turns left: north of the centre a quarter in.
        let quarter = std::f64::consts::FRAC_PI_2 * 1500.0 / (70.0 * KTS_TO_MPS);
        let p = t.position_at(quarter);
        assert!(p.lat_deg > 47.2 && (p.lon_deg + 1.5).abs() < 1e-4, "{p:?}");
    }

    #[test]
    fn extrapolates_past_leg_end() {
        let t = leg();
        let p = t.position_at(1e5); // far beyond the leg
        assert!(p.lat_deg.is_finite() && p.lon_deg.is_finite());
    }
}
