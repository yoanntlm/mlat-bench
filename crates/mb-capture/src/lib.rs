//! MBC capture container format. Built out in M2, spec frozen at M5.
//!
//! A capture is a directory: manifest.json, scenario.resolved.json,
//! truth.jsonl.zst, audibility.jsonl.zst, clients/rx-NNN.mbc.zst.
//! Record stream: magic "MBC1" + one JSON header line, then
//! `u64 t_nanos | u8 type | u32 len | payload` (LE).
//! Types: 0x01 client→server bytes, 0x02 server→client bytes (recorder only),
//! 0x03 connect, 0x04 disconnect.

/// Stream magic, first 4 bytes of every .mbc file (before zstd).
pub const MBC_MAGIC: &[u8; 4] = b"MBC1";

pub fn placeholder() {}
