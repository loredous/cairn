//! Preamble sequences S1 and S2 (SPEC 6.1, ADR 0005 `preamble-sequences`).
//!
//! Frame order: S1, S1, S2, S2, each a full symbol on the 24 base tones with
//! its own prefix. The detector is in [`crate::detector`].

use std::f64::consts::PI;

use num_complex::Complex64;

use crate::gold;
use crate::ofdm::Grid;
use crate::params::{BASE_TONES, BIN_OFFSET, N_USEFUL, PREAMBLE_SYMBOLS, S1_PHASE_DENOM, S1_PHASE_UNITS, S2_C_INIT};

/// S1 on base tone k: exp(j 2 pi u[k] / 256).
pub fn s1() -> Vec<Complex64> {
    S1_PHASE_UNITS.iter().map(|&u| Complex64::from_polar(1.0, 2.0 * PI * u as f64 / S1_PHASE_DENOM as f64)).collect()
}

/// S2 on base tone k: 1 - 2 c(k), c_init = 5237.
pub fn s2() -> Vec<Complex64> {
    gold::sequence(S2_C_INIT, BASE_TONES).into_iter().map(|c| Complex64::new(gold::bpsk(c), 0.0)).collect()
}

/// The four preamble symbols as tone values (4, width); base tones only.
pub fn grid(width: usize) -> Grid {
    let mut g = Grid::zeros(PREAMBLE_SYMBOLS, width);
    let (a, b) = (s1(), s2());
    for k in 0..BASE_TONES {
        *g.at_mut(0, k) = a[k];
        *g.at_mut(1, k) = a[k];
        *g.at_mut(2, k) = b[k];
        *g.at_mut(3, k) = b[k];
    }
    g
}

/// Analytic multisine sum_k values[k] exp(j 2 pi (3.5 + k) t / 480); `t` in
/// 48 kHz samples from the start of the useful part, fractional or negative.
pub fn tone_waveform(values: &[Complex64], t: f64) -> Complex64 {
    values
        .iter()
        .enumerate()
        .map(|(k, &v)| v * Complex64::from_polar(1.0, 2.0 * PI * t * (BIN_OFFSET + k as f64) / N_USEFUL as f64))
        .sum()
}
