//! Minimal 16-bit mono PCM WAV reading and writing at 48 kHz (SPEC 5.1), for
//! fixtures, captures and tools.

use std::io;
use std::path::Path;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Samples of a 16-bit mono 48 kHz PCM WAV file, scaled to +-1.
pub fn read(path: &Path) -> io::Result<Vec<f64>> {
    parse(&std::fs::read(path)?)
}

/// Parse WAV bytes (see [`read`]).
pub fn parse(b: &[u8]) -> io::Result<Vec<f64>> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut i = 12;
    let mut fmt_ok = false;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = b.get(i + 8..i + 8 + len).ok_or_else(|| bad("truncated chunk"))?;
        if id == b"fmt " {
            if len < 16 {
                return Err(bad("short fmt chunk"));
            }
            let format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if (format, channels, rate, bits) != (1, 1, crate::params::FS, 16) {
                return Err(bad("expected 16-bit mono 48 kHz PCM"));
            }
            fmt_ok = true;
        } else if id == b"data" {
            if !fmt_ok {
                return Err(bad("data before fmt"));
            }
            return Ok(body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0).collect());
        }
        i += 8 + len + (len & 1);
    }
    Err(bad("no data chunk"))
}

/// Write samples (full scale +-1, clipped) as 16-bit mono 48 kHz PCM.
pub fn write(path: &Path, samples: &[f64]) -> io::Result<()> {
    let data_len = 2 * samples.len() as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&crate::params::FS.to_le_bytes());
    b.extend_from_slice(&(2 * crate::params::FS).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &v in samples {
        let q = (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
        b.extend_from_slice(&q.to_le_bytes());
    }
    std::fs::write(path, b)
}
