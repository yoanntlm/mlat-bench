//! MBC capture container format (v1; spec frozen at M5, see
//! docs/capture-format.md).
//!
//! A capture is a directory:
//! ```text
//! capture/
//! ├── manifest.json           format version, scenario hash, client table
//! ├── scenario.toml           verbatim scenario (synthetic captures)
//! ├── truth.jsonl.zst         TruthPoint per line (synthetic only)
//! ├── audibility.jsonl.zst    per-second audibility rows (synthetic only)
//! └── clients/<id>.mbc.zst    one record stream per client
//! ```
//! Record stream (after zstd): magic `MBC1`, one JSON header line, then
//! records of `u64 t_nanos | u8 type | u32 len | payload`, little-endian.
//! Types: 0x01 client→server bytes, 0x02 server→client bytes (recorder only),
//! 0x03 connect (payload = handshake line), 0x04 disconnect.

use mb_core::TruthPoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Stream magic, first 4 bytes of every .mbc file (before zstd).
pub const MBC_MAGIC: &[u8; 4] = b"MBC1";

/// Pinned zstd level: byte-deterministic output for identical input matters
/// more than ratio here.
const ZSTD_LEVEL: i32 = 3;

pub const REC_C2S: u8 = 0x01;
pub const REC_S2C: u8 = 0x02;
pub const REC_CONNECT: u8 = 0x03;
pub const REC_DISCONNECT: u8 = 0x04;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("format: {0}")]
    Format(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String, // "mbc"
    pub version: u32,   // 1
    pub name: String,
    pub seed: u64,
    pub duration_s: u64,
    /// SHA-256 of the scenario TOML text (empty for real recordings).
    pub scenario_sha256: String,
    pub clients: Vec<ClientEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientEntry {
    pub id: String,
    pub file: String, // relative: clients/<id>.mbc.zst
    pub clock_type: String,
    pub compress: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub t_nanos: u64,
    pub kind: u8,
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------- writing

pub struct CaptureWriter {
    dir: PathBuf,
}

impl CaptureWriter {
    /// Creates the capture directory (must not already exist — captures are
    /// immutable artifacts, never overwritten in place).
    pub fn create(dir: &Path) -> Result<Self, CaptureError> {
        if dir.exists() {
            return Err(CaptureError::Format(format!(
                "{} already exists — captures are immutable, pick a fresh path",
                dir.display()
            )));
        }
        std::fs::create_dir_all(dir.join("clients"))?;
        Ok(CaptureWriter { dir: dir.into() })
    }

    pub fn write_scenario_toml(&self, toml_text: &str) -> Result<String, CaptureError> {
        std::fs::write(self.dir.join("scenario.toml"), toml_text)?;
        Ok(hex::encode(Sha256::digest(toml_text.as_bytes())))
    }

    pub fn write_truth<'a>(
        &self,
        points: impl Iterator<Item = &'a TruthPoint>,
    ) -> Result<(), CaptureError> {
        let f = std::fs::File::create(self.dir.join("truth.jsonl.zst"))?;
        let mut z = zstd::Encoder::new(BufWriter::new(f), ZSTD_LEVEL)?.auto_finish();
        for p in points {
            serde_json::to_writer(&mut z, p)?;
            z.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn write_audibility<T: Serialize>(
        &self,
        rows: impl Iterator<Item = T>,
    ) -> Result<(), CaptureError> {
        let f = std::fs::File::create(self.dir.join("audibility.jsonl.zst"))?;
        let mut z = zstd::Encoder::new(BufWriter::new(f), ZSTD_LEVEL)?.auto_finish();
        for r in rows {
            serde_json::to_writer(&mut z, &r)?;
            z.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn client_writer(&self, id: &str) -> Result<ClientWriter, CaptureError> {
        let path = self.dir.join("clients").join(format!("{id}.mbc.zst"));
        let f = std::fs::File::create(path)?;
        let mut z = zstd::Encoder::new(BufWriter::new(f), ZSTD_LEVEL)?.auto_finish();
        z.write_all(MBC_MAGIC)?;
        let header = serde_json::json!({"id": id});
        serde_json::to_writer(&mut z, &header)?;
        z.write_all(b"\n")?;
        Ok(ClientWriter { z })
    }

    pub fn write_manifest(&self, m: &Manifest) -> Result<(), CaptureError> {
        let f = std::fs::File::create(self.dir.join("manifest.json"))?;
        serde_json::to_writer_pretty(BufWriter::new(f), m)?;
        Ok(())
    }
}

pub struct ClientWriter {
    z: zstd::stream::AutoFinishEncoder<'static, BufWriter<std::fs::File>>,
}

impl ClientWriter {
    pub fn record(&mut self, t_nanos: u64, kind: u8, payload: &[u8]) -> Result<(), CaptureError> {
        self.z.write_all(&t_nanos.to_le_bytes())?;
        self.z.write_all(&[kind])?;
        self.z.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.z.write_all(payload)?;
        Ok(())
    }

    /// Explicit finish so write errors surface instead of vanishing in Drop.
    pub fn finish(mut self) -> Result<(), CaptureError> {
        self.z.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------- reading

pub struct CaptureReader {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl CaptureReader {
    pub fn open(dir: &Path) -> Result<Self, CaptureError> {
        let m: Manifest = serde_json::from_reader(std::fs::File::open(dir.join("manifest.json"))?)?;
        if m.format != "mbc" || m.version != 1 {
            return Err(CaptureError::Format(format!(
                "unsupported capture format {}v{}",
                m.format, m.version
            )));
        }
        Ok(CaptureReader {
            dir: dir.into(),
            manifest: m,
        })
    }

    pub fn scenario_toml(&self) -> Result<String, CaptureError> {
        Ok(std::fs::read_to_string(self.dir.join("scenario.toml"))?)
    }

    pub fn truth(&self) -> Result<Vec<TruthPoint>, CaptureError> {
        let f = std::fs::File::open(self.dir.join("truth.jsonl.zst"))?;
        let z = BufReader::new(zstd::Decoder::new(f)?);
        let mut out = Vec::new();
        for line in z.lines() {
            out.push(serde_json::from_str(&line?)?);
        }
        Ok(out)
    }

    pub fn client_records(&self, entry: &ClientEntry) -> Result<RecordIter, CaptureError> {
        let f = std::fs::File::open(self.dir.join(&entry.file))?;
        let mut z = BufReader::new(zstd::Decoder::new(f)?);
        let mut magic = [0u8; 4];
        z.read_exact(&mut magic)?;
        if &magic != MBC_MAGIC {
            return Err(CaptureError::Format("bad MBC magic".into()));
        }
        let mut header = String::new();
        z.read_line(&mut header)?;
        Ok(RecordIter { z })
    }
}

pub struct RecordIter {
    z: BufReader<zstd::Decoder<'static, BufReader<std::fs::File>>>,
}

impl Iterator for RecordIter {
    type Item = Result<Record, CaptureError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut t = [0u8; 8];
        match self.z.read_exact(&mut t) {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
            Err(e) => return Some(Err(e.into())),
            Ok(()) => {}
        }
        let mut kind = [0u8; 1];
        let mut len = [0u8; 4];
        if let Err(e) = self
            .z
            .read_exact(&mut kind)
            .and_then(|_| self.z.read_exact(&mut len))
        {
            return Some(Err(e.into()));
        }
        let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
        if let Err(e) = self.z.read_exact(&mut payload) {
            return Some(Err(e.into()));
        }
        Some(Ok(Record {
            t_nanos: u64::from_le_bytes(t),
            kind: kind[0],
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("mbc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let w = CaptureWriter::create(&dir).unwrap();
        let sha = w.write_scenario_toml("[meta]\nname='x'\n").unwrap();
        let truth = vec![TruthPoint {
            t: mb_core::SimNanos(1_000_000_000),
            icao: mb_core::Icao(0x3C6444),
            pos: mb_core::Geodetic {
                lat_deg: 47.0,
                lon_deg: -1.5,
                alt_m: 10000.0,
            },
            gs_mps: 230.0,
            vrate_mps: 0.0,
        }];
        w.write_truth(truth.iter()).unwrap();
        w.write_audibility(std::iter::once(serde_json::json!({"t_s": 1})))
            .unwrap();

        let mut cw = w.client_writer("rx-000").unwrap();
        cw.record(0, REC_CONNECT, b"{\"version\":3}\n").unwrap();
        cw.record(1_500_000_000, REC_C2S, b"{\"heartbeat\":{}}\n")
            .unwrap();
        cw.finish().unwrap();

        w.write_manifest(&Manifest {
            format: "mbc".into(),
            version: 1,
            name: "x".into(),
            seed: 7,
            duration_s: 60,
            scenario_sha256: sha,
            clients: vec![ClientEntry {
                id: "rx-000".into(),
                file: "clients/rx-000.mbc.zst".into(),
                clock_type: "dump1090".into(),
                compress: "none".into(),
                lat: 47.21,
                lon: -1.55,
                alt_m: 40.0,
            }],
        })
        .unwrap();

        let r = CaptureReader::open(&dir).unwrap();
        assert_eq!(r.manifest.clients.len(), 1);
        assert_eq!(r.truth().unwrap().len(), 1);
        let recs: Vec<Record> = r
            .client_records(&r.manifest.clients[0])
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].kind, REC_CONNECT);
        assert_eq!(recs[1].t_nanos, 1_500_000_000);
        assert_eq!(recs[1].payload, b"{\"heartbeat\":{}}\n");

        // Immutability: create() refuses an existing dir.
        assert!(CaptureWriter::create(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
