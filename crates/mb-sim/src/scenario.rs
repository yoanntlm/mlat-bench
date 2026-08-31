//! Scenario file model (TOML, human-authored) and its resolved form.

use mb_core::Geodetic;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub meta: Meta,
    #[serde(default)]
    pub network: Network,
    pub receivers: Vec<ReceiverSpec>,
    pub aircraft: Vec<AircraftSpec>,
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
    /// generation time (stream "net/<rx>"). Models the client's route to the
    /// server; deliberately constant per client — network jitter belongs to
    /// a later stress scenario, not the baseline.
    pub latency_ms_min: f64,
    pub latency_ms_max: f64,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            latency_ms_min: 5.0,
            latency_ms_max: 40.0,
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
        toml::from_str(s)
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
