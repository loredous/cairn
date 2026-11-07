//! Test channel: white Gaussian noise at a set audio SNR (as `channel.awgn`
//! of the Stage 1 simulator), with a small self-contained generator so tests
//! and benchmarks need no extra dependency. Not part of the modem proper.

use crate::params::{BAND_LOW_HZ, FS, band_high_hz};

/// xoshiro256** pseudo-random generator.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
    spare: Option<f64>,
}

impl Rng {
    /// Seeded generator (SplitMix64 expansion of `seed`).
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Rng { s: [next(), next(), next(), next()], spare: None }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let u1 = 1.0 - self.uniform();
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let (s, c) = (2.0 * std::f64::consts::PI * u2).sin_cos();
        self.spare = Some(r * s);
        r * c
    }

    /// Random bits (0/1).
    pub fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next_u64() >> 63) as u8).collect()
    }
}

/// Add white Gaussian noise so the in-band audio SNR (signal mean power over
/// noise power in 300 Hz .. highest tone + 50 Hz) is `snr_db`. The signal
/// power is the mean over `x` unless given.
pub fn awgn(x: &[f64], snr_db: f64, width: usize, rng: &mut Rng, signal_power: Option<f64>) -> Vec<f64> {
    let p_sig = signal_power.unwrap_or_else(|| x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64);
    let in_band = p_sig / 10f64.powf(snr_db / 10.0);
    let variance = in_band * (FS as f64 / 2.0) / (band_high_hz(width) - BAND_LOW_HZ);
    let sd = variance.sqrt();
    x.iter().map(|v| v + sd * rng.normal()).collect()
}
