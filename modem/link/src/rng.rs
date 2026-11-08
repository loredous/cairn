//! A small deterministic generator for the protocol's random draws
//! (backoffs, p-persistence, timer jitter, session IDs, the frame counter's
//! start). Seeded by the caller so a simulation replays exactly.

use std::time::Duration;

/// SplitMix64 generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Uniform duration in [lo, hi].
    pub fn duration(&mut self, lo: Duration, hi: Duration) -> Duration {
        let span = hi.saturating_sub(lo).as_micros() as u64;
        lo + Duration::from_micros(if span == 0 { 0 } else { self.next_u64() % (span + 1) })
    }

    /// Uniform u16.
    pub fn u16(&mut self) -> u16 {
        (self.next_u64() >> 48) as u16
    }
}
