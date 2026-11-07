//! OFDM symbol synthesis and demodulation, clip-and-filter and peak scaling
//! (SPEC 5.2, 7.1; ADR 0005 `symbol-waveform`).
//!
//! A symbol with tone values X[k] is x(n) = A Re{sum_k X[k] exp(j 2 pi (3.5 + k) n / 480)}
//! for n = -cp .. 479. Tone k is FFT bin 3.5 + k of the 480-sample useful
//! symbol, a half-bin offset, so the prefix is the negated tail. The
//! modulator evaluates an integer-bin IFFT and multiplies by exp(j pi n / 480)
//! over the prefix and useful part, which is exactly that sum; the
//! demodulator removes the half-bin offset before its FFT. With
//! A = sqrt(2 / active) and every active tone at unit power, the mean power
//! is 1 (the simulator's scale).

use std::f64::consts::PI;
use std::sync::Arc;

use num_complex::Complex64;
use rustfft::{Fft, FftPlanner};

use crate::params::{CP_LONG, FIRST_BIN, N_USEFUL};

/// Tone values of a run of symbols, row-major (symbol, tone).
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    /// Symbols.
    pub rows: usize,
    /// Tones per symbol (the link width).
    pub cols: usize,
    /// rows x cols values.
    pub data: Vec<Complex64>,
}

impl Grid {
    /// All-zero grid.
    pub fn zeros(rows: usize, cols: usize) -> Self {
        Grid { rows, cols, data: vec![Complex64::new(0.0, 0.0); rows * cols] }
    }

    /// One symbol's tones.
    pub fn row(&self, r: usize) -> &[Complex64] {
        &self.data[r * self.cols..(r + 1) * self.cols]
    }

    /// One symbol's tones, mutable.
    pub fn row_mut(&mut self, r: usize) -> &mut [Complex64] {
        &mut self.data[r * self.cols..(r + 1) * self.cols]
    }

    /// Value at (symbol, tone).
    pub fn at(&self, r: usize, k: usize) -> Complex64 {
        self.data[r * self.cols + k]
    }

    /// Mutable value at (symbol, tone).
    pub fn at_mut(&mut self, r: usize, k: usize) -> &mut Complex64 {
        &mut self.data[r * self.cols + k]
    }

    /// Rows `range` as a new grid.
    pub fn slice_rows(&self, start: usize, end: usize) -> Grid {
        Grid { rows: end - start, cols: self.cols, data: self.data[start * self.cols..end * self.cols].to_vec() }
    }
}

/// FFT plans and the half-bin rotation table for the 480-point symbol.
pub struct Ofdm {
    fwd: Arc<dyn Fft<f64>>,
    inv: Arc<dyn Fft<f64>>,
    /// exp(j pi n / 480) for n = -CP_LONG .. 479, index n + CP_LONG.
    half_bin: Vec<Complex64>,
    scratch: Vec<Complex64>,
    buf: Vec<Complex64>,
}

impl Default for Ofdm {
    fn default() -> Self {
        Self::new()
    }
}

impl Ofdm {
    /// Plan the 480-point transforms.
    pub fn new() -> Self {
        let mut planner = FftPlanner::new();
        let fwd = planner.plan_fft_forward(N_USEFUL);
        let inv = planner.plan_fft_inverse(N_USEFUL);
        let scratch_len = fwd.get_inplace_scratch_len().max(inv.get_inplace_scratch_len());
        let half_bin = (0..CP_LONG + N_USEFUL)
            .map(|i| {
                let n = i as f64 - CP_LONG as f64;
                Complex64::from_polar(1.0, PI * n / N_USEFUL as f64)
            })
            .collect();
        Ofdm {
            fwd,
            inv,
            half_bin,
            scratch: vec![Complex64::new(0.0, 0.0); scratch_len],
            buf: vec![Complex64::new(0.0, 0.0); N_USEFUL],
        }
    }

    #[inline]
    fn hb(&self, n: isize) -> Complex64 {
        self.half_bin[(n + CP_LONG as isize) as usize]
    }

    /// One symbol's samples (cp + 480) from its tone values, appended to `out`.
    pub fn modulate_symbol(&mut self, tones: &[Complex64], cp: usize, active: usize, out: &mut Vec<f64>) {
        assert!(cp <= CP_LONG);
        let a = (2.0 / active as f64).sqrt();
        self.buf.fill(Complex64::new(0.0, 0.0));
        self.buf[FIRST_BIN..FIRST_BIN + tones.len()].copy_from_slice(tones);
        self.inv.process_with_scratch(&mut self.buf, &mut self.scratch);
        for n in -(cp as isize)..N_USEFUL as isize {
            let b = self.buf[n.rem_euclid(N_USEFUL as isize) as usize];
            out.push(a * (b * self.hb(n)).re);
        }
    }

    /// Samples of every symbol of `grid`, each with its prefix. `active` is
    /// the tone count the scale refers to (one value for the whole frame, so
    /// every tone keeps its amplitude, SPEC 6.2).
    pub fn modulate(&mut self, grid: &Grid, cp: usize, active: usize) -> Vec<f64> {
        let mut out = Vec::with_capacity(grid.rows * (N_USEFUL + cp));
        for r in 0..grid.rows {
            self.modulate_symbol(grid.row(r), cp, active, &mut out);
        }
        out
    }

    /// Tone values of the useful part starting at `samples[first]` (tones 0 .. n_tones-1).
    pub fn demodulate_symbol(
        &mut self,
        samples: &[f64],
        first: usize,
        n_tones: usize,
        active: usize,
        out: &mut [Complex64],
    ) {
        for (n, b) in self.buf.iter_mut().enumerate() {
            *b = self.half_bin[n + CP_LONG].conj() * samples[first + n];
        }
        self.fwd.process_with_scratch(&mut self.buf, &mut self.scratch);
        let scale = 2.0 / (N_USEFUL as f64 * (2.0 / active as f64).sqrt());
        for (k, o) in out.iter_mut().take(n_tones).enumerate() {
            *o = self.buf[FIRST_BIN + k] * scale;
        }
    }

    /// Demodulate `n_sym` symbols whose first useful part starts at sample
    /// `start`, symbol s shifted by `shifts[s]` samples if given.
    #[allow(clippy::too_many_arguments)]
    pub fn demodulate(
        &mut self,
        samples: &[f64],
        start: usize,
        n_sym: usize,
        n_tones: usize,
        cp: usize,
        active: usize,
        shifts: Option<&[i64]>,
    ) -> Grid {
        let stride = N_USEFUL + cp;
        let mut g = Grid::zeros(n_sym, n_tones);
        for s in 0..n_sym {
            let off = shifts.map_or(0, |v| v[s]);
            let first = (start as i64 + (stride * s) as i64 + off) as usize;
            let row = &mut g.data[s * n_tones..(s + 1) * n_tones];
            self.demodulate_symbol(samples, first, n_tones, active, row);
        }
        g
    }

    /// SPEC 7.1 clip-and-filter, symbol by symbol: clip at `clip_db` above
    /// the symbol's RMS, keep only the `keep` tones, rebuild the symbol with
    /// its prefix, and clip again (repeated, as the second clip lowers the
    /// RMS) so every symbol's PAPR is at most `clip_db`. `scale_tones` is the
    /// frame's tone-count scale (any value works; it cancels).
    pub fn clip_and_filter(
        &mut self,
        samples: &[f64],
        n_tones: usize,
        keep: &[usize],
        cp: usize,
        clip_db: f64,
        scale_tones: usize,
    ) -> Vec<f64> {
        let stride = N_USEFUL + cp;
        assert_eq!(samples.len() % stride, 0);
        let ratio = 10f64.powf(clip_db / 20.0);
        let mut keep_mask = vec![false; n_tones];
        for &k in keep {
            keep_mask[k] = true;
        }
        let mut out = Vec::with_capacity(samples.len());
        let mut tones = vec![Complex64::new(0.0, 0.0); n_tones];
        let mut sym = vec![0.0; stride];
        let mut rebuilt = Vec::with_capacity(stride);
        for chunk in samples.chunks_exact(stride) {
            let rms = (chunk.iter().map(|v| v * v).sum::<f64>() / stride as f64).sqrt();
            let limit = rms * ratio;
            for (o, &v) in sym.iter_mut().zip(chunk) {
                *o = v.clamp(-limit, limit);
            }
            self.demodulate_symbol(&sym, cp, n_tones, scale_tones, &mut tones);
            for (t, &k) in tones.iter_mut().zip(&keep_mask) {
                if !k {
                    *t = Complex64::new(0.0, 0.0);
                }
            }
            rebuilt.clear();
            self.modulate_symbol(&tones, cp, scale_tones, &mut rebuilt);
            let mut cur = rebuilt.clone();
            for _ in 0..12 {
                let rms2 = (cur.iter().map(|v| v * v).sum::<f64>() / stride as f64).sqrt();
                let limit2 = rms2 * ratio;
                for (c, &v) in cur.iter_mut().zip(&rebuilt) {
                    *c = v.clamp(-limit2, limit2);
                }
            }
            out.extend_from_slice(&cur);
        }
        out
    }
}

/// Per-symbol peak-to-average power ratio in dB.
pub fn papr_db(samples: &[f64], cp: usize) -> Vec<f64> {
    samples
        .chunks_exact(N_USEFUL + cp)
        .map(|s| {
            let peak = s.iter().fold(0.0f64, |m, v| m.max(v * v));
            let mean = s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64;
            10.0 * (peak / mean).log10()
        })
        .collect()
}

/// Scale a frame so its largest sample sits at `peak_dbfs` (SPEC 7.1: -3 dBFS),
/// full scale being 1.0. Returns the gain applied.
pub fn scale_to_peak(samples: &mut [f64], peak_dbfs: f64) -> f64 {
    let peak = samples.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if peak == 0.0 {
        return 1.0;
    }
    let g = 10f64.powf(peak_dbfs / 20.0) / peak;
    for v in samples.iter_mut() {
        *v *= g;
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_direct_sum_and_round_trips() {
        let mut o = Ofdm::new();
        let mut g = Grid::zeros(2, 24);
        for (i, v) in g.data.iter_mut().enumerate() {
            *v = Complex64::from_polar(1.0, i as f64 * 0.7);
        }
        for cp in [60, 120] {
            let x = o.modulate(&g, cp, 24);
            let a = (2.0f64 / 24.0).sqrt();
            for (idx, &v) in x.iter().enumerate().take(N_USEFUL + cp) {
                let n = idx as f64 - cp as f64;
                let direct: f64 = (0..24)
                    .map(|k| (g.at(0, k) * Complex64::from_polar(1.0, 2.0 * PI * (3.5 + k as f64) * n / 480.0)).re)
                    .sum::<f64>()
                    * a;
                assert!((direct - v).abs() < 1e-12);
            }
            // Prefix is the negated tail.
            for n in 0..cp {
                assert!((x[n] + x[N_USEFUL + n]).abs() < 1e-12);
            }
            let back = o.demodulate(&x, cp, 2, 24, cp, 24, None);
            for (a, b) in back.data.iter().zip(&g.data) {
                assert!((a - b).norm() < 1e-12);
            }
        }
    }
}
