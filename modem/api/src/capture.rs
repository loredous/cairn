//! The raw-audio ring buffer and capture files (SPEC 13.4, 13.6, 21.3
//! step 8).
//!
//! [`AudioRing`] keeps the last N seconds of received audio as 16-bit
//! samples in one buffer allocated up front: [`AudioRing::push`] only
//! converts and copies (no allocation, no locking), so it is safe on the
//! receive path; the modem calls it from its event loop, never from the
//! sound card's callback. [`AudioRing::last`] copies a window out for a
//! capture, and [`write_wav`] writes it as 16-bit mono PCM at 48 kHz, the
//! format `cairn_phy::wav` and the conformance vectors read.

use std::io;
use std::path::Path;

/// Sample rate of every capture (SPEC 5.1).
pub const SAMPLE_RATE: u32 = 48_000;

/// Full scale to a 16-bit sample, as `cairn_phy::wav::write` quantizes.
pub fn quantize(v: f64) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// A fixed-size ring of received samples.
#[derive(Clone, Debug)]
pub struct AudioRing {
    buf: Vec<i16>,
    /// Samples ever pushed; sample k of the stream is at `k % len`.
    total: u64,
}

impl AudioRing {
    /// A ring holding `seconds` of audio at 48 kHz (0: off).
    pub fn with_seconds(seconds: f64) -> Self {
        let n = (seconds.max(0.0) * SAMPLE_RATE as f64).round() as usize;
        AudioRing { buf: vec![0; n], total: 0 }
    }

    /// Capacity in samples.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Capacity in seconds.
    pub fn seconds(&self) -> f64 {
        self.buf.len() as f64 / SAMPLE_RATE as f64
    }

    /// Samples held now.
    pub fn len(&self) -> usize {
        (self.total as usize).min(self.buf.len())
    }

    /// Nothing held (or off).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Samples ever pushed (the stream index of the next one).
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Append samples (full scale +-1). Never allocates.
    pub fn push(&mut self, x: &[f64]) {
        let n = self.buf.len();
        if n == 0 {
            self.total += x.len() as u64;
            return;
        }
        // Only the last `n` samples of a long chunk survive.
        let skip = x.len().saturating_sub(n);
        let mut at = ((self.total + skip as u64) % n as u64) as usize;
        for &v in &x[skip..] {
            self.buf[at] = quantize(v);
            at += 1;
            if at == n {
                at = 0;
            }
        }
        self.total += x.len() as u64;
    }

    /// The last `n` samples held (fewer when the ring holds fewer), with
    /// the stream index of the first.
    pub fn last(&self, n: usize) -> (u64, Vec<i16>) {
        let have = self.len();
        let n = n.min(have);
        let start = self.total - n as u64;
        let cap = self.buf.len() as u64;
        let mut out = Vec::with_capacity(n);
        for k in start..self.total {
            out.push(self.buf[(k % cap) as usize]);
        }
        (start, out)
    }
}

/// WAV bytes: 16-bit mono PCM at 48 kHz.
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = 2 * samples.len() as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(2 * SAMPLE_RATE).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// Write samples as a WAV file.
pub fn write_wav(path: &Path, samples: &[i16]) -> io::Result<()> {
    std::fs::write(path, wav_bytes(samples))
}

/// A file-name-safe label: `[A-Za-z0-9_-]{1,32}`.
pub fn valid_label(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `dir/stem.wav` and `dir/stem.json`, with `-2`, `-3`, ... appended to the
/// stem when either exists.
pub fn capture_paths(dir: &Path, stem: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let pair = |s: &str| (dir.join(format!("{s}.wav")), dir.join(format!("{s}.json")));
    let (w, j) = pair(stem);
    if !w.exists() && !j.exists() {
        return (w, j);
    }
    (2..).map(|k| pair(&format!("{stem}-{k}"))).find(|(w, j)| !w.exists() && !j.exists()).expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_samples_in_order() {
        let mut r = AudioRing { buf: vec![0; 5], total: 0 };
        let x: Vec<f64> = (0..8).map(|i| i as f64 / 32768.0).collect();
        r.push(&x[..3]);
        assert_eq!(r.last(10), (0, vec![0, 1, 2]));
        r.push(&x[3..]);
        assert_eq!(r.len(), 5);
        assert_eq!(r.last(10), (3, vec![3, 4, 5, 6, 7]));
        assert_eq!(r.last(2), (6, vec![6, 7]));
        // A chunk longer than the ring.
        let y: Vec<f64> = (100..112).map(|i| i as f64 / 32768.0).collect();
        r.push(&y);
        assert_eq!(r.total(), 20);
        assert_eq!(r.last(5), (15, vec![107, 108, 109, 110, 111]));
        // Off.
        let mut off = AudioRing::with_seconds(0.0);
        off.push(&x);
        assert_eq!((off.len(), off.total()), (0, 8));
    }

    #[test]
    fn quantizes_like_the_phy_wav_writer() {
        assert_eq!(quantize(1.0), 32767);
        assert_eq!(quantize(-1.0), -32768);
        assert_eq!(quantize(0.5 / 32768.0), 1);
        assert_eq!(quantize(-2.0), -32768);
    }

    #[test]
    fn wav_header_is_48k_mono_16_bit() {
        let b = wav_bytes(&[1, -1, 300]);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(b[34..36].try_into().unwrap()), 16);
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(i16::from_le_bytes([b[48], b[49]]), 300);
    }

    #[test]
    fn labels_and_names() {
        assert!(valid_label("fade_2-a"));
        assert!(!valid_label(""));
        assert!(!valid_label("../x"));
        assert!(!valid_label(&"a".repeat(33)));
    }
}
