//! Streaming decoder for the Mode-S Beast binary format.
//!
//! Message: 0x1a, type (0x31 Mode A/C, 0x32 short Mode S, 0x33 long
//! Mode S), 6-byte big-endian timestamp (12 MHz counts), signal byte,
//! frame. Every 0x1a inside the payload is doubled. Mode A/C messages are
//! consumed and dropped; MLAT has no use for them.

pub struct Reception {
    pub counts_12mhz: u64,
    pub frame: Vec<u8>,
}

#[derive(Default)]
pub struct BeastDecoder {
    buf: Vec<u8>,
}

enum Step {
    Consumed(usize, Option<Reception>),
    NeedMore,
}

impl BeastDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Reception> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < self.buf.len() {
            if self.buf[pos] != 0x1a {
                pos += 1; // resync noise
                continue;
            }
            match Self::parse_at(&self.buf[pos..]) {
                Step::Consumed(n, r) => {
                    if let Some(r) = r {
                        out.push(r);
                    }
                    pos += n;
                }
                Step::NeedMore => break,
            }
        }
        self.buf.drain(..pos);
        out
    }

    fn parse_at(b: &[u8]) -> Step {
        if b.len() < 2 {
            return Step::NeedMore;
        }
        let frame_len = match b[1] {
            0x31 => 2usize,
            0x32 => 7,
            0x33 => 14,
            // 0x1a 0x1a here is a stray escape outside a message; anything
            // else is noise. Skip one byte and resync.
            _ => return Step::Consumed(1, None),
        };
        let want = 6 + 1 + frame_len;
        let mut payload = Vec::with_capacity(want);
        let mut i = 2usize;
        while payload.len() < want {
            if i >= b.len() {
                return Step::NeedMore;
            }
            let c = b[i];
            if c == 0x1a {
                if i + 1 >= b.len() {
                    return Step::NeedMore;
                }
                if b[i + 1] == 0x1a {
                    payload.push(0x1a);
                    i += 2;
                    continue;
                }
                // Unescaped 0x1a inside a payload: truncated message.
                // Resync at it.
                return Step::Consumed(i, None);
            }
            payload.push(c);
            i += 1;
        }
        if frame_len == 2 {
            return Step::Consumed(i, None); // Mode A/C
        }
        let counts = u64::from_be_bytes([
            0, 0, payload[0], payload[1], payload[2], payload[3], payload[4], payload[5],
        ]);
        Step::Consumed(
            i,
            Some(Reception {
                counts_12mhz: counts,
                frame: payload[7..].to_vec(),
            }),
        )
    }
}

/// Encode one reception, the exact inverse of the decoder.
#[cfg(test)]
pub fn encode(counts_12mhz: u64, signal: u8, frame: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + 7 + frame.len() * 2);
    buf.push(0x1a);
    buf.push(if frame.len() == 7 { 0x32 } else { 0x33 });
    let ts = counts_12mhz.to_be_bytes();
    let mut payload = Vec::with_capacity(7 + frame.len());
    payload.extend_from_slice(&ts[2..8]);
    payload.push(signal);
    payload.extend_from_slice(frame);
    for b in payload {
        buf.push(b);
        if b == 0x1a {
            buf.push(0x1a);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<(u64, Vec<u8>)> {
        vec![
            // Timestamps and frames that contain 0x1a bytes exercise the
            // escaping.
            (0x1a_1a_1a_1a_1a_1a, vec![0x1a; 7]),
            (
                12_000_000,
                hex::decode("8d40621d58c382d690c8ac2863a7").unwrap(),
            ),
            (
                0xFFFF_FFFF_FFFF,
                vec![0x20, 0x1a, 0x00, 0x1a, 0x1a, 0x30, 0x99],
            ),
        ]
    }

    #[test]
    fn roundtrip_single_feed() {
        let mut wire = Vec::new();
        for (t, f) in frames() {
            wire.extend(encode(t, 0x1a, &f));
        }
        let mut d = BeastDecoder::default();
        let got = d.feed(&wire);
        let want = frames();
        assert_eq!(got.len(), want.len());
        for (g, (t, f)) in got.iter().zip(&want) {
            assert_eq!(g.counts_12mhz, *t);
            assert_eq!(&g.frame, f);
        }
    }

    #[test]
    fn roundtrip_byte_by_byte() {
        let mut wire = Vec::new();
        for (t, f) in frames() {
            wire.extend(encode(t, 0xA0, &f));
        }
        let mut d = BeastDecoder::default();
        let mut got = Vec::new();
        for b in wire {
            got.extend(d.feed(&[b]));
        }
        assert_eq!(got.len(), frames().len());
    }

    #[test]
    fn resyncs_after_noise() {
        let mut wire = vec![0x00, 0x1a, 0x99, 0x42]; // garbage + bad type
        wire.extend(encode(5, 0xA0, &[0x5d; 7]));
        let mut d = BeastDecoder::default();
        let got = d.feed(&wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].counts_12mhz, 5);
    }

    #[test]
    fn mode_ac_dropped() {
        let mut d = BeastDecoder::default();
        let mut wire = vec![0x1a, 0x31, 0, 0, 0, 0, 0, 1, 0xA0, 0x12, 0x34];
        wire.extend(encode(7, 0xA0, &[0x5d; 7]));
        let got = d.feed(&wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].counts_12mhz, 7);
    }
}
