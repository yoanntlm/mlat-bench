//! Scenario file model (TOML, human-authored) and its resolved form.

use mb_core::Geodetic;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub meta: Meta,
    #[serde(default)]
    pub network: Network,
    /// Procedural scale block: expands into concrete receivers/aircraft at
    /// load time, deterministically from the seed. Explicit [[receivers]] /
    /// [[aircraft]] entries are kept and come first.
    #[serde(default)]
    pub generate: Option<GenerateSpec>,
    #[serde(default)]
    pub receivers: Vec<ReceiverSpec>,
    #[serde(default)]
    pub aircraft: Vec<AircraftSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateSpec {
    pub receivers: u32,
    pub aircraft_adsb: u32,
    pub aircraft_modes: u32,
    /// [lat, lon] of the region center.
    pub center: [f64; 2],
    pub radius_km: f64,
    /// Fraction of generated receivers with GPS clocks (default 0.1).
    #[serde(default = "default_gps_fraction")]
    pub gps_fraction: f64,
}

fn default_gps_fraction() -> f64 {
    0.1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub seed: u64,
    pub duration_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    /// Fixed per-client one-way latency, drawn uniformly in this range at
    /// generation time (stream "net/<rx>").
    pub latency_ms_min: f64,
    pub latency_ms_max: f64,
    /// Per-message gaussian latency jitter (1σ, ms). Zero = the polite lab.
    #[serde(default)]
    pub jitter_ms: f64,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            latency_ms_min: 5.0,
            latency_ms_max: 40.0,
            jitter_ms: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverSpec {
    pub id: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
    pub clock: ClockSpec,
    /// Per-message loss probability (independent draws).
    #[serde(default = "default_loss")]
    pub loss_prob: f64,
    /// Range cap in km on top of the radio horizon (antenna quality proxy).
    #[serde(default)]
    pub max_range_km: Option<f64>,
    /// Error in the receiver's reported (handshake) position vs its true
    /// antenna location, meters; operators mistype coordinates. Drawn as a
    /// fixed offset in a seeded direction at gen time.
    #[serde(default)]
    pub reported_pos_error_m: f64,
    /// Per-reception probability of a multipath delay spike
    /// (+0.5..3 µs on the timestamp — a reflection's longer path).
    #[serde(default)]
    pub multipath_prob: f64,
    /// Wire compression this client pins in its handshake.
    #[serde(default = "default_compress")]
    pub compress: String,
}

fn default_loss() -> f64 {
    0.02
}
fn default_compress() -> String {
    "none".into()
}

impl ReceiverSpec {
    pub fn pos(&self) -> Geodetic {
        Geodetic {
            lat_deg: self.lat,
            lon_deg: self.lon,
            alt_m: self.alt_m,
        }
    }
}

/// Receiver clock model. Bounds context: the oracle assumes dump1090 clocks
/// are within 100 ppm frequency error with 500 ns jitter (clocktrack.pyx);
/// a realistic scenario stays well inside that.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClockSpec {
    /// 12 MHz free-running counter (dump1090 and friends).
    #[serde(rename = "dump1090")]
    Dump1090 {
        /// Constant frequency offset, ppm.
        #[serde(default)]
        offset_ppm: f64,
        /// Linear frequency drift, ppm per hour (thermal).
        #[serde(default)]
        drift_ppm_per_hr: f64,
        /// Gaussian per-measurement jitter, ns (1σ).
        #[serde(default = "default_jitter_dump1090")]
        jitter_ns: f64,
        /// Counter value at scenario T0 (wraps at 2^48).
        #[serde(default)]
        start_count: u64,
        /// Frequency random-walk intensity, ppm per √hour — thermal wander
        /// beyond the linear drift term. Breaks any purely linear sync model.
        #[serde(default)]
        wander_ppm_sqrt_hr: f64,
        /// Probability per minute of a counter jump (±1..100 ms), after
        /// which the client sends clock_jump like real mlat-client does.
        #[serde(default)]
        jump_prob_per_min: f64,
    },
    /// GPS-disciplined 1 GHz (radarcape): no offset/drift by construction.
    #[serde(rename = "radarcape_gps")]
    RadarcapeGps {
        #[serde(default = "default_jitter_gps")]
        jitter_ns: f64,
    },
}

fn default_jitter_dump1090() -> f64 {
    40.0
}
fn default_jitter_gps() -> f64 {
    10.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AircraftSpec {
    /// 6 hex digits.
    pub icao: String,
    pub kind: AircraftKind,
    pub traj: Trajectory,
    /// DF17 position broadcast rate (even/odd alternating), Hz. ADS-B only.
    #[serde(default = "default_pos_rate")]
    pub pos_rate_hz: f64,
    /// DF11 acquisition squitter rate, Hz. Mode-S-only.
    #[serde(default = "default_df11_rate")]
    pub df11_rate_hz: f64,
    /// DF4 altitude reply rate, Hz (models interrogation). Mode-S-only.
    #[serde(default = "default_df4_rate")]
    pub df4_rate_hz: f64,
    /// ADS-B navigation error: broadcast DF17 positions are offset from the
    /// true position by this many meters in a fixed seeded direction — the
    /// "liar" sync source (GPS-degraded or spoofed traffic) that poisons
    /// clock sync if a server trusts it blindly.
    #[serde(default)]
    pub nav_error_m: f64,
}

fn default_pos_rate() -> f64 {
    2.0
}
fn default_df11_rate() -> f64 {
    1.0
}
fn default_df4_rate() -> f64 {
    4.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AircraftKind {
    /// Emits DF17 positions: a sync source AND directly trackable.
    Adsb,
    /// Emits only DF11/DF4: exists so the server must multilaterate it.
    ModesOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trajectory {
    /// Great-circle leg from -> to at constant ground speed; altitude
    /// interpolates linearly along the leg. Continues past `to` on the same
    /// great circle if the scenario outlasts the leg.
    GreatCircle {
        /// [lat_deg, lon_deg, alt_ft]
        from: [f64; 3],
        to: [f64; 3],
        gs_kts: f64,
    },
}

impl Scenario {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        let mut sc: Scenario = toml::from_str(s)?;
        sc.expand_generate();
        Ok(sc)
    }

    /// Expand the [generate] block into concrete entries, deterministically.
    fn expand_generate(&mut self) {
        use rand::Rng;
        let Some(g) = self.generate.take() else {
            return;
        };
        let seed = self.meta.seed;
        let mut rng = mb_core::rng_for(seed, "generate");
        let (clat, clon) = (g.center[0], g.center[1]);
        let deg_per_km_lat = 1.0 / 111.32;
        let deg_per_km_lon = 1.0 / (111.32 * clat.to_radians().cos());
        let disk = |r_km: f64, rng: &mut rand_chacha::ChaCha12Rng| {
            // Uniform in a disk: sqrt on the radial draw.
            let r = r_km * rng.gen_range(0.0f64..1.0).sqrt();
            let th = rng.gen_range(0.0..std::f64::consts::TAU);
            (
                clat + r * th.sin() * deg_per_km_lat,
                clon + r * th.cos() * deg_per_km_lon,
            )
        };
        for i in 0..g.receivers {
            let (lat, lon) = disk(g.radius_km, &mut rng);
            let gps = rng.gen_range(0.0f64..1.0) < g.gps_fraction;
            let clock = if gps {
                ClockSpec::RadarcapeGps { jitter_ns: 15.0 }
            } else {
                ClockSpec::Dump1090 {
                    offset_ppm: rng.gen_range(-40.0..40.0),
                    drift_ppm_per_hr: rng.gen_range(0.0..0.3),
                    jitter_ns: rng.gen_range(40.0..90.0),
                    start_count: 0,
                    wander_ppm_sqrt_hr: rng.gen_range(0.0..0.4),
                    jump_prob_per_min: 0.0,
                }
            };
            self.receivers.push(ReceiverSpec {
                id: format!("gen-{i:03}"),
                lat,
                lon,
                alt_m: rng.gen_range(10.0..200.0),
                clock,
                loss_prob: rng.gen_range(0.01..0.08),
                max_range_km: None,
                reported_pos_error_m: 0.0,
                multipath_prob: 0.0,
                compress: "none".into(),
            });
        }
        let mk_traj = |rng: &mut rand_chacha::ChaCha12Rng, alt_lo: f64, alt_hi: f64| {
            let (la1, lo1) = disk(g.radius_km * 1.3, rng);
            let (la2, lo2) = disk(g.radius_km * 1.3, rng);
            let alt = rng.gen_range(alt_lo..alt_hi);
            Trajectory::GreatCircle {
                from: [la1, lo1, alt],
                to: [la2, lo2, alt],
                gs_kts: rng.gen_range(320.0..500.0),
            }
        };
        for i in 0..g.aircraft_adsb {
            let traj = mk_traj(&mut rng, 26000.0, 41000.0);
            self.aircraft.push(AircraftSpec {
                icao: format!("A{:05X}", 0x10000 + i),
                kind: AircraftKind::Adsb,
                traj,
                pos_rate_hz: 2.0,
                df11_rate_hz: 1.0,
                df4_rate_hz: 4.0,
                nav_error_m: 0.0,
            });
        }
        for i in 0..g.aircraft_modes {
            let traj = mk_traj(&mut rng, 8000.0, 26000.0);
            self.aircraft.push(AircraftSpec {
                icao: format!("B{:05X}", 0x20000 + i),
                kind: AircraftKind::ModesOnly,
                traj,
                pos_rate_hz: 2.0,
                df11_rate_hz: 1.0,
                df4_rate_hz: rng.gen_range(2.0..6.0),
                nav_error_m: 0.0,
            });
        }
    }

    /// Sanity checks that catch scenario-authoring mistakes before an
    /// expensive run does. Errors are for humans.
    pub fn validate(&self) -> Result<(), String> {
        if self.receivers.is_empty() {
            return Err("no receivers".into());
        }
        if self.aircraft.is_empty() {
            return Err("no aircraft".into());
        }
        let mut ids = std::collections::HashSet::new();
        for r in &self.receivers {
            if !ids.insert(&r.id) {
                return Err(format!("duplicate receiver id {}", r.id));
            }
            if !(-90.0..=90.0).contains(&r.lat) || !(-180.0..=180.0).contains(&r.lon) {
                return Err(format!("receiver {} position out of range", r.id));
            }
            if !(-1000.0..=10000.0).contains(&r.alt_m) {
                return Err(format!(
                    "receiver {} altitude {} outside the oracle's accepted -1000..10000 m",
                    r.id, r.alt_m
                ));
            }
            if !matches!(r.compress.as_str(), "none" | "zlib" | "zlib2") {
                return Err(format!(
                    "receiver {}: unknown compress {}",
                    r.id, r.compress
                ));
            }
        }
        let mut icaos = std::collections::HashSet::new();
        for a in &self.aircraft {
            if mb_core::Icao::from_hex(&a.icao).is_none() {
                return Err(format!("aircraft icao {} not 6 hex digits", a.icao));
            }
            if !icaos.insert(&a.icao) {
                return Err(format!("duplicate aircraft icao {}", a.icao));
            }
        }
        Ok(())
    }
}
