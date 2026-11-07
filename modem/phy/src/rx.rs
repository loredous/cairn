//! Frame receiver, ported from `frame.receive` of the Stage 1 simulator.
//!
//! Given the frame's configuration and its start (from the detector), it
//! demodulates every symbol; estimates the per-tone channel and noise from
//! the two S2 symbols (base tones) and the width probe (tones above);
//! tracks common phase and sample-clock drift with the continuous pilots,
//! re-placing the FFT windows if the clock drifts by a sample or more;
//! measures the pilots' distortion; decodes the header with the
//! header-noise method ("measured", the simulator's default); re-estimates
//! the base tones' noise from the decoded header (decision-directed);
//! raises the noise of burst-hit symbols (SPEC 6.5); demaps max-log LLRs and
//! decodes every data block.
//!
//! Arithmetic is `f64` (LDPC in `f32`, as the simulator), so results agree
//! with the simulator to rounding, not bit for bit.

use std::f64::consts::PI;

use num_complex::Complex64;

use crate::dsp;
use crate::frame::FrameConfig;
use crate::ldpc::{self, DecodeResult};
use crate::mapping::Demapper;
use crate::ofdm::{Grid, Ofdm};
use crate::params::{
    self, BASE_TONES, BIN_OFFSET, HEADER_CODED_BITS, HEADER_POSITION, LDPC_ALPHA, N_USEFUL, PREAMBLE_SYMBOLS,
};
use crate::{gold, interleave, preamble, tx};

/// How the header's per-tone noise is estimated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderNoise {
    /// Preamble-pair noise floor raised by the header's own measurement (default).
    Measured,
    /// The S2 difference plus the pilots' excess interpolated across the band.
    S2,
}

/// Receiver settings (defaults are the simulator's).
#[derive(Clone, Debug, PartialEq)]
pub struct RxOptions {
    /// Samples the FFT window moves into the prefix (default cp / 4).
    pub advance: Option<usize>,
    /// Pilot phase smoothing window, symbols.
    pub phase_window: usize,
    /// Channel smoothing window, tones (< 3: raw estimate).
    pub smooth_window: usize,
    /// Re-estimate base-tone noise from the decoded header.
    pub decision_directed: bool,
    /// Discount symbols hit by a noise burst (SPEC 6.5).
    pub burst_aware: bool,
    /// Header noise method.
    pub header_noise: HeaderNoise,
    /// LDPC iterations.
    pub max_iter: usize,
}

impl Default for RxOptions {
    fn default() -> Self {
        RxOptions {
            advance: None,
            phase_window: 15,
            smooth_window: 7,
            decision_directed: true,
            burst_aware: true,
            header_noise: HeaderNoise::Measured,
            max_iter: params::LDPC_MAX_ITER,
        }
    }
}

/// What the receiver found.
#[derive(Clone, Debug)]
pub struct RxResult {
    /// Header CRC passed.
    pub header_ok: bool,
    /// Header information bits (144, CRC included).
    pub header_bits: Vec<u8>,
    /// Header decoder iterations.
    pub header_iterations: usize,
    /// Per data block: CRC passed.
    pub block_ok: Vec<bool>,
    /// Per data block: information bits (1024, CRC included).
    pub block_bits: Vec<Vec<u8>>,
    /// Per data block: decoder iterations.
    pub iterations: Vec<usize>,
    /// Frame start used (sample index of the first preamble sample).
    pub start: usize,
    /// Per-tone SNR estimate, dB (average-referenced, not clip-adjusted).
    pub snr_db: Vec<f64>,
    /// Sample-clock drift estimate, samples per symbol.
    pub drift: f64,
}

impl RxResult {
    /// Effective SNR over the tones, on the 7 dB-clip scale (SPEC 7.1): the
    /// mean per-tone linear SNR in dB plus the rung's clip offset.
    pub fn snr_7db_equivalent(&self, cfg: &FrameConfig) -> f64 {
        let lin: f64 = self.snr_db.iter().map(|d| 10f64.powf(d / 10.0)).sum::<f64>() / self.snr_db.len() as f64;
        10.0 * lin.log10() + params::clip_offset_db(cfg.mcs)
    }
}

struct Estimate {
    h: Vec<Complex64>,
    noise: Vec<f64>,
    thermal: Vec<f64>,
    excess: Vec<f64>,
    snr_db: Vec<f64>,
    /// (body rows) x width phase correction.
    corr: Grid,
    drift: f64,
}

fn snr_of(h: &[Complex64], noise: &[f64]) -> Vec<f64> {
    h.iter().zip(noise).map(|(h, n)| 10.0 * (h.norm_sqr() / n).log10()).collect()
}

/// Per-tone channel from two known symbols, de-rotated by the mean phase
/// step, smoothed by a quadratic Savitzky-Golay fit over `window` tones.
fn estimate_channel(
    y2: &[Complex64],
    y3: &[Complex64],
    s2: &[Complex64],
    second: &[Complex64],
    window: usize,
) -> Vec<Complex64> {
    let raw: Vec<Complex64> = (0..y2.len()).map(|k| (y2[k] / s2[k] + y3[k] / second[k]) / 2.0).collect();
    if window < 3 {
        return raw;
    }
    let acc: Complex64 = raw.windows(2).map(|w| w[1] * w[0].conj()).sum();
    let step = acc.arg();
    let flat: Vec<Complex64> =
        raw.iter().enumerate().map(|(k, v)| v * Complex64::from_polar(1.0, -step * k as f64)).collect();
    let re = dsp::savgol_interp(&flat.iter().map(|v| v.re).collect::<Vec<_>>(), window, 2);
    let im = dsp::savgol_interp(&flat.iter().map(|v| v.im).collect::<Vec<_>>(), window, 2);
    (0..raw.len()).map(|k| Complex64::new(re[k], im[k]) * Complex64::from_polar(1.0, step * k as f64)).collect()
}

/// Per-tone noise variance from the difference of two repeated symbols.
fn estimate_noise(y2: &[Complex64], y3: &[Complex64]) -> Vec<f64> {
    let raw: Vec<f64> = y2.iter().zip(y3).map(|(a, b)| (a - b).norm_sqr() / 2.0).collect();
    dsp::box_average(&raw, 7)
}

/// Phase correction per body row and tone from the pilots (see the sim's
/// `track_drift`); returns the correction and the timing drift in samples
/// per symbol.
fn track_drift(
    obs: &[Vec<Option<Complex64>>],
    pilots: &[usize],
    width: usize,
    s_ref: f64,
    window: usize,
) -> (Grid, f64) {
    let n_sym = obs.len();
    let mut slopes = vec![0.0; pilots.len()];
    for (j, slope) in slopes.iter_mut().enumerate() {
        let mut rows = Vec::new();
        let mut vals = Vec::new();
        for (s, row) in obs.iter().enumerate() {
            if let Some(v) = row[j] {
                rows.push(s as f64);
                vals.push(v);
            }
        }
        if rows.is_empty() {
            continue;
        }
        let klen = if rows.len() >= window { window } else { rows.len() };
        let kernel = vec![1.0 / klen as f64; klen];
        let sm = dsp::convolve_same_complex(&vals, &kernel);
        let theta = dsp::unwrap(&sm.iter().map(|v| v.arg()).collect::<Vec<_>>());
        let rm = dsp::mean(&rows);
        let tm = dsp::mean(&theta);
        let num: f64 = rows.iter().zip(&theta).map(|(r, t)| (r - rm) * (t - tm)).sum();
        let den: f64 = rows.iter().map(|r| (r - rm) * (r - rm)).sum();
        *slope = num / den.max(1e-12);
    }
    let f: Vec<f64> = pilots.iter().map(|&p| BIN_OFFSET + p as f64).collect();
    let (beta, alpha) = if pilots.len() > 1 { dsp::linear_fit(&f, &slopes) } else { (0.0, slopes[0]) };
    let mut corr = Grid::zeros(n_sym, width);
    for s in 0..n_sym {
        for k in 0..width {
            let fk = BIN_OFFSET + k as f64;
            *corr.at_mut(s, k) = Complex64::from_polar(1.0, (alpha + beta * fk) * (s as f64 - s_ref));
        }
    }
    (corr, -beta * N_USEFUL as f64 / (2.0 * PI))
}

/// Noise plus distortion on data symbols raised by `gain`: the additive part
/// drops by gain^2 once the gain is divided out; distortion stays.
fn data_noise(noise: &[f64], thermal: &[f64], gain: f64) -> Vec<f64> {
    noise.iter().zip(thermal).map(|(&n, &t)| (n - t * (1.0 - gain.powi(-2))).max(n / (gain * gain))).collect()
}

/// Receiver with reusable FFT plans.
pub struct Receiver {
    ofdm: Ofdm,
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Receiver {
    /// A receiver.
    pub fn new() -> Self {
        Receiver { ofdm: Ofdm::new() }
    }

    fn demodulate(&mut self, rx: &[f64], window_start: usize, cfg: &FrameConfig, shifts: Option<&[i64]>) -> Grid {
        let n_sym = cfg.total_symbols();
        let stride = cfg.stride();
        // Zero-pad wherever a window would leave the buffer.
        let lo = shifts.map_or(0, |s| {
            s.iter().enumerate().map(|(i, &d)| (window_start + stride * i) as i64 + d).min().unwrap_or(0)
        });
        let hi = shifts.map_or((window_start + stride * (n_sym - 1)) as i64, |s| {
            s.iter().enumerate().map(|(i, &d)| (window_start + stride * i) as i64 + d).max().unwrap_or(0)
        }) + N_USEFUL as i64;
        let left = (-lo).max(0) as usize;
        let right = (hi - rx.len() as i64).max(0) as usize;
        let mut g = if left == 0 && right == 0 {
            self.ofdm.demodulate(rx, window_start, n_sym, cfg.width, cfg.cp, cfg.width, shifts)
        } else {
            let mut padded = vec![0.0; left];
            padded.extend_from_slice(rx);
            padded.extend(std::iter::repeat_n(0.0, right));
            self.ofdm.demodulate(&padded, window_start + left, n_sym, cfg.width, cfg.cp, cfg.width, shifts)
        };
        if let Some(sh) = shifts {
            // Undo each shift's known phase: moving the window d samples later
            // turns tone bin f by 2 pi f d / N.
            for (s, &d) in sh.iter().enumerate() {
                if d != 0 {
                    for k in 0..cfg.width {
                        let f = BIN_OFFSET + k as f64;
                        *g.at_mut(s, k) *= Complex64::from_polar(1.0, -2.0 * PI * f * d as f64 / N_USEFUL as f64);
                    }
                }
            }
        }
        let gain = cfg.data_gain();
        for s in cfg.first_data()..n_sym {
            for v in g.row_mut(s) {
                *v /= gain;
            }
        }
        g
    }

    fn estimate(&self, tones: &Grid, cfg: &FrameConfig, opts: &RxOptions) -> Estimate {
        let b = BASE_TONES;
        let w = cfg.width;
        let first = PREAMBLE_SYMBOLS;
        let hs = cfg.header_symbols();
        let s2 = preamble::s2();
        let mut h = vec![Complex64::new(0.0, 0.0); w];
        let mut noise = vec![0.0; w];
        let (y2, y3) = (&tones.row(2)[..b], &tones.row(3)[..b]);
        h[..b].copy_from_slice(&estimate_channel(y2, y3, &s2, &s2, opts.smooth_window));
        noise[..b].copy_from_slice(&estimate_noise(y2, y3));
        if cfg.has_probe {
            let p0 = first + hs;
            let known = tx::probe_grid(w, p0);
            let hp = estimate_channel(tones.row(p0), tones.row(p0 + 1), known.row(0), known.row(1), opts.smooth_window);
            let a: Vec<Complex64> = (0..w).map(|k| tones.at(p0, k) / known.at(0, k)).collect();
            let c: Vec<Complex64> = (0..w).map(|k| tones.at(p0 + 1, k) / known.at(1, k)).collect();
            let np = estimate_noise(&a, &c);
            for k in b..w {
                h[k] = hp[k];
                noise[k] = np[k] * known.at(0, k).norm_sqr();
            }
        }
        let mean_h2 = h.iter().map(|v| v.norm_sqr()).sum::<f64>() / w as f64;
        for n in noise.iter_mut() {
            *n = n.max(1e-6 * mean_h2);
        }
        let thermal = noise.clone();
        let mut snr_db = snr_of(&h, &noise);

        let n_body = tones.rows - first;
        let pilots = &cfg.data_pilots;
        let table = gold::pilot_table(tones.rows);
        let mut obs: Vec<Vec<Option<Complex64>>> = (0..n_body)
            .map(|s| {
                pilots
                    .iter()
                    .map(|&p| {
                        if s < hs && p >= b {
                            None
                        } else {
                            Some(tones.at(first + s, p) / gold::pilot_value(&table, first + s, p))
                        }
                    })
                    .collect()
            })
            .collect();
        let d0 = hs + cfg.probe_symbols();
        let act = &cfg.data_active_tones;
        if opts.burst_aware && n_body > d0 {
            let power: Vec<f64> = (d0..n_body)
                .map(|s| {
                    act.iter().map(|&k| tones.at(first + s, k).norm_sqr() / h[k].norm_sqr()).sum::<f64>()
                        / act.len() as f64
                })
                .collect();
            for (i, out) in dsp::outliers(&power, 6.0).into_iter().enumerate() {
                if out {
                    obs[d0 + i].iter_mut().for_each(|v| *v = None);
                }
            }
        }
        let (corr, drift) = track_drift(&obs, pilots, w, 2.5 - first as f64, opts.phase_window);

        let mut excess = vec![0.0; pilots.len()];
        if n_body > d0 {
            let gain = cfg.data_gain();
            let mut sums = vec![0.0; pilots.len()];
            let mut count = 0usize;
            for (s, row) in obs.iter().enumerate().skip(d0) {
                if row.iter().any(Option::is_none) {
                    continue;
                }
                count += 1;
                for (j, &p) in pilots.iter().enumerate() {
                    let r = row[j].unwrap() / (h[p] * corr.at(s, p)) - 1.0;
                    sums[j] += r.norm_sqr();
                }
            }
            if count > 0 {
                for (j, &p) in pilots.iter().enumerate() {
                    let floor = noise[p] / (h[p].norm_sqr() * gain * gain);
                    excess[j] = (sums[j] / count as f64 - floor).max(0.0);
                }
            }
            let pf: Vec<f64> = pilots.iter().map(|&p| p as f64).collect();
            for k in 0..w {
                noise[k] += dsp::interp(k as f64, &pf, &excess) * h[k].norm_sqr();
            }
            snr_db = snr_of(&h, &noise);
        }
        Estimate { h, noise, thermal, excess, snr_db, corr, drift }
    }

    /// Decode one frame from real samples `rx` starting at sample `start`
    /// (the first sample of the first preamble symbol's prefix).
    pub fn receive(&mut self, rx: &[f64], cfg: &FrameConfig, start: usize, opts: &RxOptions) -> RxResult {
        let advance = opts.advance.unwrap_or(cfg.cp / 4);
        let n_sym = cfg.total_symbols();
        let first = PREAMBLE_SYMBOLS;
        let window_start = (start + cfg.cp).saturating_sub(advance);
        let mut tones = self.demodulate(rx, window_start, cfg, None);
        let mut est = self.estimate(&tones, cfg, opts);
        // Follow sample-clock drift: if the windows would wander by a sample
        // or more over the frame, move each symbol's window and estimate again.
        let shifts: Vec<i64> = (0..n_sym).map(|s| (est.drift * (s as f64 - 2.5)).round_ties_even() as i64).collect();
        if shifts.iter().any(|&d| d != 0) {
            tones = self.demodulate(rx, window_start, cfg, Some(&shifts));
            est = self.estimate(&tones, cfg, opts);
        }
        let Estimate { h, mut noise, thermal, excess, mut snr_db, corr, drift } = est;
        let w = cfg.width;
        let hs = cfg.header_symbols();
        let d0 = hs + cfg.probe_symbols();
        let n_body = n_sym - first;
        let gain = cfg.data_gain();
        let h2: Vec<f64> = h.iter().map(|v| v.norm_sqr()).collect();
        let mean_h2 = dsp::mean(&h2);

        let mut eq = Grid::zeros(n_body, w);
        for s in 0..n_body {
            for (k, hk) in h.iter().enumerate() {
                *eq.at_mut(s, k) = tones.at(first + s, k) / (hk * corr.at(s, k));
            }
        }
        let mut nv = vec![0.0; n_body * w];
        let dn = data_noise(&noise, &thermal, gain);
        for s in 0..n_body {
            for k in 0..w {
                nv[s * w + k] = if s < d0 { noise[k] } else { dn[k] } / h2[k];
            }
        }

        // Header.
        let btones = &cfg.base_data_tones;
        let b = BASE_TONES;
        if opts.header_noise == HeaderNoise::Measured {
            let floor = header_noise_floor(&tones, &h[..b], &thermal[..b], &excess);
            let bp = params::pilots_for(b);
            let table = gold::pilot_table(first + hs);
            let mut m = vec![0.0; b];
            for &k in btones {
                m[k] = (0..hs).map(|s| eq.at(s, k).norm_sqr()).sum::<f64>() / hs as f64 - 1.0;
            }
            for &p in &bp {
                m[p] = (0..hs).map(|s| (eq.at(s, p) - gold::pilot_value(&table, first + s, p)).norm_sqr()).sum::<f64>()
                    / hs as f64;
            }
            let pooled = dsp::convolve_same(&m, &[1.0; 3]);
            let count = dsp::convolve_same(&vec![1.0; b], &[1.0; 3]);
            for k in 0..b {
                let pk = pooled[k] / count[k];
                let sd = ((2.0 * floor[k] + floor[k] * floor[k]) / (count[k] * hs as f64)).sqrt();
                let v = floor[k].max(pk - 2.0 * sd);
                for s in 0..hs {
                    nv[s * w + k] = v;
                }
            }
        }
        let demap = Demapper::new(2);
        let mut hl = Vec::with_capacity(hs * btones.len() * 2);
        for s in 0..hs {
            for &k in btones {
                demap.push_llrs(eq.at(s, k), nv[s * w + k], &mut hl);
            }
        }
        hl.truncate(HEADER_CODED_BITS);
        let mut hl = interleave::deinterleave(&hl);
        gold::descramble_llr(&mut hl, HEADER_POSITION);
        let hcode = ldpc::header_code();
        let hres = hcode.decoder().decode(
            &hcode.rate_recover(&hl, 0),
            &hcode.rows_for_length(HEADER_CODED_BITS),
            opts.max_iter,
            LDPC_ALPHA,
        );
        if hres.ok && opts.decision_directed {
            // The decoded header is a known signal on every base data tone.
            let known = tx::header_points(cfg, &hres.bits);
            let d = btones.len();
            let err: Vec<f64> = (0..d)
                .map(|slot| {
                    (0..hs).map(|s| (eq.at(s, btones[slot]) - known[s * d + slot]).norm_sqr()).sum::<f64>() / hs as f64
                })
                .collect();
            let bt: Vec<f64> = btones.iter().map(|&k| k as f64).collect();
            let rel: Vec<f64> = (0..b).map(|k| dsp::interp(k as f64, &bt, &err)).collect();
            let rel = dsp::box_average(&rel, 3);
            for k in 0..b {
                noise[k] = (rel[k] * h2[k]).max(1e-6 * mean_h2);
            }
            snr_db = snr_of(&h, &noise);
            let dn = data_noise(&noise, &thermal, gain);
            for s in d0..n_body {
                for k in 0..w {
                    nv[s * w + k] = dn[k] / h2[k];
                }
            }
        }

        // Data blocks.
        let act = &cfg.data_active_tones;
        if opts.burst_aware && n_body > d0 {
            let inv: Vec<f64> = act.iter().map(|&k| 1.0 / h2[k]).collect();
            let inv_mean = dsp::mean(&inv);
            let m: Vec<f64> = (d0..n_body)
                .map(|s| act.iter().map(|&k| eq.at(s, k).norm_sqr() - nv[s * w + k]).sum::<f64>() / act.len() as f64)
                .collect();
            let med = dsp::median(&m);
            for (i, out) in dsp::outliers(&m, 6.0).into_iter().enumerate() {
                if out {
                    let ex = m[i] - med;
                    for (j, &k) in act.iter().enumerate() {
                        nv[(d0 + i) * w + k] += ex * inv[j] / inv_mean;
                    }
                }
            }
        }
        let rung = cfg.rung();
        let demap = Demapper::new(rung.bits_per_point);
        let dt = &cfg.data_tones;
        let mut dl = Vec::with_capacity((n_body - d0) * dt.len() * rung.bits_per_point);
        for s in d0..n_body {
            for &k in dt {
                demap.push_llrs(eq.at(s, k), nv[s * w + k], &mut dl);
            }
        }
        let e = rung.coded_bits;
        let dcode = ldpc::data_code();
        let rows = dcode.rows_for_length(e);
        let mut dec = dcode.decoder();
        let results: Vec<DecodeResult> = (0..cfg.n_blocks)
            .map(|i| {
                let mut l = interleave::deinterleave(&dl[i * e..(i + 1) * e]);
                gold::descramble_llr(&mut l, 1 + i as u32);
                dec.decode(&dcode.rate_recover(&l, 0), &rows, opts.max_iter, LDPC_ALPHA)
            })
            .collect();
        RxResult {
            header_ok: hres.ok,
            header_bits: hres.bits,
            header_iterations: hres.iterations,
            block_ok: results.iter().map(|r| r.ok).collect(),
            iterations: results.iter().map(|r| r.iterations).collect(),
            block_bits: results.into_iter().map(|r| r.bits).collect(),
            start,
            snr_db,
            drift,
        }
    }
}

/// Noise variance the preamble and pilots predict for header symbols: the
/// differences of both repeated preamble pairs smoothed by a linear fit over
/// 11 tones (never below a quarter of the S2 estimate), over |h|^2, plus the
/// pilots' mean distortion.
fn header_noise_floor(tones: &Grid, h: &[Complex64], thermal: &[f64], excess: &[f64]) -> Vec<f64> {
    let b = h.len();
    let raw: Vec<f64> = (0..b)
        .map(|k| ((tones.at(0, k) - tones.at(1, k)).norm_sqr() + (tones.at(2, k) - tones.at(3, k)).norm_sqr()) / 4.0)
        .collect();
    let smooth = dsp::savgol_interp(&raw, 11, 1);
    let ex = if excess.is_empty() { 0.0 } else { dsp::mean(excess) };
    (0..b).map(|k| smooth[k].max(thermal[k] / 4.0) / h[k].norm_sqr() + ex).collect()
}
