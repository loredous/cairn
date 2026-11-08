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
//! decodes every control block, then every data block. Finally it measures
//! the channel SNR (ADR 0006) against the frame rebuilt from the decoded bits.
//!
//! [`Receiver::receive_header_first`] needs no configuration: it decodes the
//! preamble and header on the base tones, unpacks the header (SPEC 8.2) and
//! takes the rung, the control and data block counts and the probe flag
//! from it; width and masks come from the caller's [`LinkSource`] (a
//! [`LinkState`], or a closure that gives one for the decoded header), the
//! mask by the header's Mask ID (ADR 0011), the prefix from the detector.
//! The result names each data block's sequence ([`RxResult::block_sequences`],
//! from the header and the block map of ADR 0008).
//!
//! Arithmetic is `f64` (LDPC in `f32`, as the simulator), so results agree
//! with the simulator to rounding, not bit for bit.

use std::f64::consts::PI;

use num_complex::Complex64;

use crate::dsp;
use crate::fields::{self, ControlBlock, FieldError, Header};
use crate::frame::{ConfigError, FrameConfig, LinkState};
use crate::ldpc::{self, DecodeResult};
use crate::mapping::Demapper;
use crate::ofdm::{Grid, Ofdm};
use crate::params::{
    self, BASE_TONES, BIN_OFFSET, CONTROL_CODED_BITS, HEADER_CODED_BITS, HEADER_POSITION, LDPC_ALPHA, N_USEFUL,
    PREAMBLE_SYMBOLS,
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
    /// Measure the channel SNR by rebuilding the sent symbols (ADR 0006).
    pub rebuild_snr: bool,
    /// Threads decoding data blocks (1: on the calling thread). Results do
    /// not depend on it.
    pub threads: usize,
    /// The sample-clock offset expected before the frame is measured
    /// ([`ClockPrior`]; default 0 +- 50 ppm). A [`LinkSource`] can give a
    /// per-peer prior for the frame once its header names the sender.
    pub clock_prior: ClockPrior,
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
            rebuild_snr: true,
            threads: 1,
            clock_prior: ClockPrior::default(),
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
    /// The header's fields, when its CRC passed and its fields are valid.
    pub header: Option<Header>,
    /// Why there are no header fields, or why they describe no frame.
    pub header_error: Option<HeaderError>,
    /// Per control block: CRC passed.
    pub control_ok: Vec<bool>,
    /// Per control block: information bits (256, CRC included).
    pub control_bits: Vec<Vec<u8>>,
    /// Per control block: its fields (None where it failed or is malformed).
    pub controls: Vec<Option<ControlBlock>>,
    /// The configuration the frame was decoded with (None if the header failed).
    pub config: Option<FrameConfig>,
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
    /// Sample-clock drift estimate, samples per symbol: the pilots'
    /// measurement combined with the clock prior ([`ClockPrior`]).
    pub drift: f64,
    /// Standard deviation of `drift`, samples per symbol (the prior's on a
    /// frame too short to measure it).
    pub drift_sd: f64,
    /// Channel SNR excluding the designed clipping (ADR 0006), if measured.
    pub channel_snr: Option<ChannelSnr>,
    /// For a frame outside a session with a probe: which tones carried it
    /// (ADR 0007), the width it gave, and the width the frame decoded at.
    pub probe_extent: Option<ProbeExtent>,
    /// The sequence of each data block (ADR 0008, [`fields::block_sequences`]):
    /// consecutive from the header's Sequence, or as the block map names
    /// them; None when the header failed, or flag bit 7 is set and the block
    /// map failed or breaks its rules (then no data block counts as received).
    pub block_sequences: Option<Vec<u16>>,
    /// The header named a mask ID the link state does not hold (ADR 0011
    /// `mask-id`): the frame's control and data blocks were not decoded and
    /// count as failed.
    pub unknown_mask: Option<u8>,
}

impl RxResult {
    /// (sequence, 126 payload bytes) of every data block received: CRC passed
    /// and its sequence known (ADR 0008).
    pub fn delivered(&self) -> Vec<(u16, [u8; crate::pdu::BLOCK_BYTES])> {
        let Some(seqs) = &self.block_sequences else { return Vec::new() };
        seqs.iter()
            .zip(&self.block_ok)
            .zip(&self.block_bits)
            .filter(|((_, ok), _)| **ok)
            .map(|((&s, _), bits)| (s, crate::pdu::bits_to_payload(bits)))
            .collect()
    }

    /// The SNR the protocol reports (SPEC 7.1, ADR 0006): the channel SNR on
    /// the 7 dB-equivalent scale, from the rebuilt symbols when measured,
    /// else the decoder's per-tone estimate over the active tones (which
    /// counts the designed clipping and so errs low).
    pub fn snr_7db_equivalent(&self, cfg: &FrameConfig) -> f64 {
        let avg = match &self.channel_snr {
            Some(c) => c.effective_db,
            None => {
                let t = &cfg.data_active_tones;
                let lin: f64 = t.iter().map(|&k| 10f64.powf(self.snr_db[k] / 10.0)).sum::<f64>() / t.len() as f64;
                10.0 * lin.log10()
            }
        };
        avg + cfg.clip_db - params::CLIP_DB_REFERENCE
    }

    /// The sample-clock offset, ppm, and its standard deviation, from the
    /// drift estimate (as `cairn-bench rx` reports it).
    pub fn clock_ppm(&self, cfg: &FrameConfig) -> (f64, f64) {
        let stride = cfg.stride() as f64;
        (-self.drift / stride * 1e6, self.drift_sd / stride * 1e6)
    }
}

/// Why a frame's header gave no usable fields.
#[derive(Clone, Debug, PartialEq)]
pub enum HeaderError {
    /// The header did not decode (LDPC or CRC).
    NotDecoded,
    /// The CRC passed but the fields are malformed (SPEC 8.2: discard).
    Fields(FieldError),
    /// The fields describe no frame this receiver can decode (a reserved
    /// rung, or a frame wider than 24 tones without a width probe).
    Config(ConfigError),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::NotDecoded => write!(f, "header did not decode"),
            HeaderError::Fields(e) => write!(f, "header fields: {e}"),
            HeaderError::Config(e) => write!(f, "header describes no decodable frame: {e}"),
        }
    }
}

/// Channel SNR measured against the rebuilt transmitted symbols (ADR 0006).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSnr {
    /// Per tone, dB: measured on every tone with two known symbols or more
    /// after the preamble (a probed tone always), interpolated across the rest
    /// (masked and silent tones).
    pub per_tone_db: Vec<f64>,
    /// Over the data symbols' active tones: total signal over total noise, dB,
    /// average-referenced (add the clip offset for the 7 dB scale).
    pub effective_db: f64,
    /// Symbols whose content was known (preamble, probe, header if decoded,
    /// control and data symbols whose blocks all decoded).
    pub known_symbols: usize,
    /// Too little was known to rebuild: the decoder's estimate, which counts
    /// the designed clipping and errs low (ADR 0006).
    pub fallback: bool,
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
    drift_sd: f64,
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

/// What the receiver assumes about the sample-clock offset between the
/// sender's sound card and its own before it measures a frame: a Gaussian
/// prior, ppm, with the sign of [`RxResult::clock_ppm`] (positive when the
/// frame arrives compressed, the simulator's `sco_ppm`).
///
/// The pilots measure the offset with a variance that falls with the cube of
/// the frame's length: about +-10 ppm on a one-block frame at 24 tones and
/// 13 dB, +-300 ppm on a header-only frame, under 1 ppm on a 4 s frame. The
/// receiver combines the measurement with this prior by their variances (so
/// a long frame is measured, a short one keeps close to the prior) and
/// bounds the result by [`CLOCK_BOUND_PPM`]. The default prior is SPEC
/// 14.1's tolerance: 0 ppm, standard deviation [`CLOCK_PRIOR_SD_PPM`]. A link
/// layer that has measured a peer on long frames can supply a tighter one
/// ([`RxOptions::clock_prior`], [`LinkSource::clock_prior`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockPrior {
    /// Expected offset, ppm.
    pub ppm: f64,
    /// Its standard deviation, ppm (> 0).
    pub sd_ppm: f64,
}

impl Default for ClockPrior {
    fn default() -> Self {
        ClockPrior { ppm: 0.0, sd_ppm: CLOCK_PRIOR_SD_PPM }
    }
}

impl ClockPrior {
    /// (mean, standard deviation, bound) as drift in samples per symbol at `stride`.
    fn drift_units(&self, stride: usize) -> (f64, f64, f64) {
        let d = |ppm: f64| ppm * 1e-6 * stride as f64;
        (-d(self.ppm), d(self.sd_ppm.max(1e-3)), d(CLOCK_BOUND_PPM))
    }
}

/// Standard deviation of the default clock prior, ppm: SPEC 14.1 allows +-100
/// ppm between stations, so two standard deviations.
pub const CLOCK_PRIOR_SD_PPM: f64 = 50.0;
/// No clock estimate exceeds this, ppm: SPEC 14.1's +-100 ppm plus margin.
pub const CLOCK_BOUND_PPM: f64 = 150.0;

/// Phase correction per body row and tone from the pilots (the sim's
/// `track_drift`): each pilot's phase over the rows it is present in is
/// unwrapped against a smoothed track and fitted with a line, whose
/// residuals give the slope's variance (none with fewer than five rows); the
/// slopes are fitted as alpha + beta f (common phase drift plus the timing
/// drift of a clock offset). The timing drift beta implies is combined with
/// the prior (mean, sd, bound in samples per symbol) by their variances and
/// bounded, and alpha re-fitted for the beta kept. The correction is zero at
/// row `s_ref[k]` for tone k, where its channel was estimated. Returns the
/// correction and the timing drift in samples per symbol with its standard
/// deviation.
fn track_drift(
    obs: &[Vec<Option<Complex64>>],
    pilots: &[usize],
    width: usize,
    s_ref: &[f64],
    window: usize,
    prior: (f64, f64, f64),
) -> (Grid, f64, f64) {
    let n_sym = obs.len();
    // A pilot absent from all but one row (an upper pilot in a frame with no
    // data symbols) tells nothing about drift.
    let used: Vec<usize> =
        (0..pilots.len()).filter(|&j| obs.iter().filter(|row| row[j].is_some()).count() >= 2).collect();
    let mut slopes = vec![0.0; used.len()];
    let mut vars = vec![f64::INFINITY; used.len()];
    for ((slope, var), &j) in slopes.iter_mut().zip(vars.iter_mut()).zip(&used) {
        let mut rows = Vec::new();
        let mut vals = Vec::new();
        for (s, row) in obs.iter().enumerate() {
            if let Some(v) = row[j] {
                rows.push(s as f64);
                vals.push(v);
            }
        }
        let klen = if rows.len() >= window { window } else { rows.len() };
        let kernel = vec![1.0 / klen as f64; klen];
        let sm = dsp::convolve_same_complex(&vals, &kernel);
        let track = dsp::unwrap(&sm.iter().map(|v| v.arg()).collect::<Vec<_>>());
        // Each symbol's own phase, unwrapped against the smoothed track (the
        // smoothing alone would flatten the ends of a short run's slope).
        let theta: Vec<f64> =
            vals.iter().zip(&track).map(|(v, t)| t + (v * Complex64::from_polar(1.0, -t)).arg()).collect();
        let rm = dsp::mean(&rows);
        let tm = dsp::mean(&theta);
        let num: f64 = rows.iter().zip(&theta).map(|(r, t)| (r - rm) * (t - tm)).sum();
        let den: f64 = rows.iter().map(|r| (r - rm) * (r - rm)).sum();
        *slope = num / den.max(1e-12);
        // The slope's variance from the residuals, widened by nu / (nu - 2)
        // (a t distribution's): a few rows estimate it poorly.
        let nu = rows.len() as f64 - 2.0;
        if nu > 2.0 {
            let rss: f64 = rows.iter().zip(&theta).map(|(r, t)| (t - tm - *slope * (r - rm)).powi(2)).sum();
            *var = rss / nu / den.max(1e-12) * nu / (nu - 2.0);
        }
    }
    let f: Vec<f64> = used.iter().map(|&j| BIN_OFFSET + pilots[j] as f64).collect();
    let to_drift = -(N_USEFUL as f64) / (2.0 * PI); // drift = to_drift * beta
    let (mu, sd0, bound) = prior;
    let (measured, var_d) = if used.len() > 1 {
        let fm = dsp::mean(&f);
        let sff: f64 = f.iter().map(|v| (v - fm) * (v - fm)).sum();
        let beta: f64 = f.iter().zip(&slopes).map(|(v, s)| (v - fm) * s).sum::<f64>() / sff;
        let var_beta: f64 = f.iter().zip(&vars).map(|(v, var)| (v - fm).powi(2) * var).sum::<f64>() / (sff * sff);
        (to_drift * beta, var_beta * to_drift * to_drift)
    } else {
        (mu, f64::INFINITY)
    };
    let (drift, var_post) = if var_d.is_finite() {
        let p0 = sd0 * sd0;
        ((measured * p0 + mu * var_d) / (p0 + var_d), p0 * var_d / (p0 + var_d))
    } else {
        (mu, sd0 * sd0)
    };
    let drift = drift.clamp(-bound, bound);
    let beta = drift / to_drift;
    let alpha = if slopes.is_empty() { 0.0 } else { dsp::mean(&slopes) - beta * dsp::mean(&f) };
    let mut corr = Grid::zeros(n_sym, width);
    for s in 0..n_sym {
        for (k, &r) in s_ref.iter().enumerate().take(width) {
            let fk = BIN_OFFSET + k as f64;
            *corr.at_mut(s, k) = Complex64::from_polar(1.0, (alpha + beta * fk) * (s as f64 - r));
        }
    }
    (corr, drift, var_post.sqrt())
}

/// A data symbol whose pilots' error is this many times the assumed noise carries
/// distortion of its own (mirrors `PILOT_EXCESS_RATIO` in the Python sim).
const PILOT_EXCESS_RATIO: f64 = 6.0;

/// Fewer data pilots (24 tones have two) scatter too much to single out a symbol.
const MIN_PILOTS: usize = 3;

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
        let nt = cfg.n_tones();
        let mut g = if left == 0 && right == 0 {
            self.ofdm.demodulate(rx, window_start, n_sym, nt, cfg.cp, cfg.width, shifts)
        } else {
            let mut padded = vec![0.0; left];
            padded.extend_from_slice(rx);
            padded.extend(std::iter::repeat_n(0.0, right));
            self.ofdm.demodulate(&padded, window_start + left, n_sym, nt, cfg.cp, cfg.width, shifts)
        };
        if let Some(sh) = shifts {
            // Undo each shift's known phase: moving the window d samples later
            // turns tone bin f by 2 pi f d / N.
            for (s, &d) in sh.iter().enumerate() {
                if d != 0 {
                    for k in 0..nt {
                        let f = BIN_OFFSET + k as f64;
                        *g.at_mut(s, k) *= Complex64::from_polar(1.0, -2.0 * PI * f * d as f64 / N_USEFUL as f64);
                    }
                }
            }
        }
        let cgain = cfg.control_gain();
        for s in cfg.first_control()..cfg.first_data() {
            for v in g.row_mut(s) {
                *v /= cgain;
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
        let w = cfg.n_tones();
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
        let d0 = cfg.first_data() - first;
        // Header rows carry base pilots only; with control blocks, so do the
        // control rows, and the upper pilots then use the data rows alone (a
        // contiguous run: probe rows dropped).
        let upper_until = if cfg.n_control > 0 { d0 } else { hs };
        let mut obs: Vec<Vec<Option<Complex64>>> = (0..n_body)
            .map(|s| {
                pilots
                    .iter()
                    .map(|&p| {
                        if s < upper_until && p >= b {
                            None
                        } else {
                            Some(tones.at(first + s, p) / gold::pilot_value(&table, first + s, p))
                        }
                    })
                    .collect()
            })
            .collect();
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
        let prior = opts.clock_prior.drift_units(cfg.stride());
        // The drift correction is zero where each tone's channel was
        // estimated: the S2 pair (rows 2-3) for the base tones, the probe pair
        // for the rest (a clock offset turns them by the drift between the
        // two, 18 degrees at tone 50 and 100 ppm).
        let s_ref: Vec<f64> = (0..w).map(|k| if k < b { 2.5 - first as f64 } else { hs as f64 + 0.5 }).collect();
        let (corr, drift, drift_sd) = track_drift(&obs, pilots, w, &s_ref, opts.phase_window, prior);

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
        Estimate { h, noise, thermal, excess, snr_db, corr, drift, drift_sd }
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
        let Estimate { h, mut noise, thermal, excess, mut snr_db, corr, drift, drift_sd } = est;
        let corr_kept = corr.clone();
        let w = cfg.n_tones();
        let hs = cfg.header_symbols();
        let c0 = cfg.first_control() - first;
        let d0 = cfg.first_data() - first;
        let n_body = n_sym - first;
        let gain = cfg.data_gain();
        let cgain = cfg.control_gain();
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
        let cn = data_noise(&noise, &thermal, cgain);
        for s in 0..n_body {
            for k in 0..w {
                nv[s * w + k] = if s < c0 {
                    noise[k]
                } else if s < d0 {
                    cn[k]
                } else {
                    dn[k]
                } / h2[k];
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
            let cn = data_noise(&noise, &thermal, cgain);
            for s in c0..n_body {
                for k in 0..w {
                    nv[s * w + k] = if s < d0 { cn[k] } else { dn[k] } / h2[k];
                }
            }
        }

        // Control blocks: MCS 0 on the control tones, scrambler positions 1 .. n_control.
        let qpsk = Demapper::new(2);
        let ct = &cfg.control_tones;
        let mut cl = Vec::with_capacity((d0 - c0) * ct.len() * 2);
        for s in c0..d0 {
            for &k in ct {
                qpsk.push_llrs(eq.at(s, k), nv[s * w + k], &mut cl);
            }
        }
        let ccode = ldpc::control_code();
        let crows = ccode.rows_for_length(CONTROL_CODED_BITS);
        let mut cdec = ccode.decoder();
        let cres: Vec<DecodeResult> = (0..cfg.n_control)
            .map(|i| {
                let e = CONTROL_CODED_BITS;
                let mut l = interleave::deinterleave(&cl[i * e..(i + 1) * e]);
                gold::descramble_llr(&mut l, 1 + i as u32);
                cdec.decode(&ccode.rate_recover(&l, 0), &crows, opts.max_iter, LDPC_ALPHA)
            })
            .collect();

        // Data blocks.
        let act = &cfg.data_active_tones;
        if n_body > d0 && cfg.data_pilots.len() >= MIN_PILOTS {
            // Clip-and-filter distortion is concentrated in the few symbols
            // whose peaks were clipped (-15 to -25 dB, flat across the
            // symbol); the frame-average noise estimate would have the LLRs
            // trust them. A symbol's own pilots measure its distortion.
            let table = gold::pilot_table(n_sym);
            let np = cfg.data_pilots.len() as f64;
            for s in d0..n_body {
                let perr = cfg
                    .data_pilots
                    .iter()
                    .map(|&p| (eq.at(s, p) - gold::pilot_value(&table, first + s, p)).norm_sqr())
                    .sum::<f64>()
                    / np;
                let assumed = cfg.data_pilots.iter().map(|&p| nv[s * w + p]).sum::<f64>() / np;
                if perr > PILOT_EXCESS_RATIO * assumed {
                    for &k in act {
                        nv[s * w + k] = nv[s * w + k].max(perr);
                    }
                }
            }
        }
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
        let results = decode_data_blocks(&dl, e, 1 + cfg.n_control, cfg.n_blocks, opts);
        let block_ok: Vec<bool> = results.iter().map(|r| r.ok).collect();
        let block_bits: Vec<Vec<u8>> = results.iter().map(|r| r.bits.clone()).collect();
        let control_ok: Vec<bool> = cres.iter().map(|r| r.ok).collect();
        let control_bits: Vec<Vec<u8>> = cres.iter().map(|r| r.bits.clone()).collect();
        let decoded = Decoded {
            header_ok: hres.ok,
            header_bits: &hres.bits,
            control_ok: &control_ok,
            control_bits: &control_bits,
            block_ok: &block_ok,
            block_bits: &block_bits,
        };
        let est = DecoderEstimate { h: &h, noise: &noise, thermal: &thermal };
        let channel_snr = opts.rebuild_snr.then(|| rebuild_snr(&tones, &corr_kept, cfg, advance, &decoded, &est));
        let (header, header_error) = parse_header(hres.ok, &hres.bits);
        let controls: Vec<Option<ControlBlock>> = control_ok
            .iter()
            .zip(&control_bits)
            .map(|(&ok, b)| if ok { ControlBlock::unpack(b).ok() } else { None })
            .collect();
        let block_sequences = sequences(header.as_ref(), &controls, cfg.n_blocks);
        RxResult {
            block_sequences,
            unknown_mask: None,
            channel_snr,
            header_ok: hres.ok,
            header_bits: hres.bits,
            header_iterations: hres.iterations,
            header,
            header_error,
            control_ok,
            control_bits,
            controls,
            config: Some(cfg.clone()),
            block_ok,
            iterations: results.iter().map(|r| r.iterations).collect(),
            block_bits,
            start,
            snr_db,
            drift,
            drift_sd,
            probe_extent: None,
        }
    }
}

fn sequences(header: Option<&Header>, controls: &[Option<ControlBlock>], n_blocks: usize) -> Option<Vec<u16>> {
    header.filter(|h| h.n_data == n_blocks).and_then(|h| fields::block_sequences(h, controls))
}

/// Where the link state for a decoded header comes from: a fixed
/// [`LinkState`], or a closure `Fn(&Header) -> Option<LinkState>` that the
/// caller answers once it knows who sent the frame (None: no session state;
/// the frame then takes its width from its probe, as outside a session).
pub trait LinkSource {
    /// The link state for a frame with this header.
    fn link_for(&self, header: &Header) -> Option<LinkState>;
    /// A prefix to insist on before any header is decoded (None: the detector's).
    fn forced_cp(&self) -> Option<usize> {
        None
    }
    /// The clock-offset prior for the frame with this header, e.g. what the
    /// link layer has measured of its sender on long frames (None: the
    /// receiver options' [`RxOptions::clock_prior`]).
    fn clock_prior(&self, _header: &Header) -> Option<ClockPrior> {
        None
    }
}

impl LinkSource for LinkState {
    fn link_for(&self, _header: &Header) -> Option<LinkState> {
        Some(self.clone())
    }
    fn forced_cp(&self) -> Option<usize> {
        self.cp
    }
}

impl<F: Fn(&Header) -> Option<LinkState>> LinkSource for F {
    fn link_for(&self, header: &Header) -> Option<LinkState> {
        self(header)
    }
}

impl Receiver {
    /// Decode a frame without being told its configuration (SPEC 8.2):
    /// decode the preamble and header on the base tones from `start` with
    /// prefix `cp` (the detector's), unpack the header, take the rung, the
    /// control and data block counts and the probe flag from it, ask `link`
    /// for the link state of that header, and decode the whole frame at its
    /// width with the mask the header's Mask ID names (ADR 0011). A frame
    /// whose header fails (decode, CRC, fields, or no decodable geometry)
    /// comes back with `header` None, `header_error` set, `config` None and
    /// no blocks. A session frame naming a mask ID the link state does not
    /// hold comes back with its header, `unknown_mask` set and every control
    /// and data block failed.
    pub fn receive_header_first<L: LinkSource + ?Sized>(
        &mut self,
        rx: &[f64],
        link: &L,
        start: usize,
        cp: usize,
        opts: &RxOptions,
    ) -> RxResult {
        let head = self.receive_header(rx, start, cp, opts);
        let Some(header) = head.header.clone() else {
            return head;
        };
        let state = link.link_for(&header);
        let peer_opts;
        let opts = match link.clock_prior(&header) {
            Some(clock_prior) => {
                peer_opts = RxOptions { clock_prior, ..opts.clone() };
                &peer_opts
            }
            None => opts,
        };
        let extent = ((state.is_none() || !header.session) && header.probe)
            .then(|| self.probe_extent(rx, start, cp, &header, opts));
        match self.receive_frame(rx, &header, state.as_ref(), extent, start, cp, opts) {
            Ok(r) => r,
            Err(ConfigError::UnknownMask(id)) => unknown_mask(head, &header, id),
            Err(e) => {
                let mut head = head;
                head.header = None;
                head.header_error = Some(HeaderError::Config(e));
                head
            }
        }
    }

    /// Decode the preamble and header alone on the base tones (the first
    /// half of [`Receiver::receive_header_first`]); `config` is None. The
    /// channel SNR is measured only if the header fails (the frame's own
    /// decode measures it otherwise).
    pub fn receive_header(&mut self, rx: &[f64], start: usize, cp: usize, opts: &RxOptions) -> RxResult {
        let head_cfg = FrameConfig::new(BASE_TONES, 0, 0).and_then(|c| c.with_cp(cp));
        let Ok(head_cfg) = head_cfg else {
            panic!("prefix {cp} is neither {} nor {}", params::CP_SHORT, params::CP_LONG);
        };
        let quick = RxOptions { rebuild_snr: false, ..opts.clone() };
        let mut head = self.receive(rx, &head_cfg, start, &quick);
        if head.header.is_none() && opts.rebuild_snr {
            head = self.receive(rx, &head_cfg, start, opts);
        }
        head.config = None;
        head
    }

    /// Decode the frame a decoded `header` describes (the second half of
    /// [`Receiver::receive_header_first`]): a session frame at `link`'s
    /// width and probe extent with the mask its Mask ID names; any other
    /// frame, or a session frame with no link state, at the width of its
    /// probe (`extent`, from [`Receiver::probe_extent`]; 24 tones without a
    /// probe). If no data block decodes at the probe's width, the widths
    /// around it are tried ([`width_alternatives`]) and the first at which a
    /// block decodes is kept. Err if the header describes no decodable frame
    /// ([`ConfigError::UnknownMask`] for a mask ID `link` does not hold).
    #[allow(clippy::too_many_arguments)]
    pub fn receive_frame(
        &mut self,
        rx: &[f64],
        header: &Header,
        link: Option<&LinkState>,
        extent: Option<ProbeExtent>,
        start: usize,
        cp: usize,
        opts: &RxOptions,
    ) -> Result<RxResult, ConfigError> {
        let cfg = FrameConfig::from_header(header, &frame_link(header, link, extent.as_ref()), cp)?;
        let mut r = self.receive(rx, &cfg, start, opts);
        if let Some(ext) = extent.as_ref().filter(|_| cfg.n_blocks > 0 && !r.block_ok.iter().any(|&b| b)) {
            for w in width_alternatives(ext.width) {
                let alt = LinkState::from_cap(w, None);
                let Ok(acfg) = FrameConfig::from_header(header, &alt, cp) else { continue };
                let ra = self.receive(rx, &acfg, start, opts);
                if ra.block_ok.iter().any(|&b| b) {
                    r = ra;
                    break;
                }
            }
        }
        // The header pass decided the geometry; report its header.
        if !r.header_ok {
            r.header_ok = true;
            r.header_bits = header.bits().expect("a decoded header packs");
        }
        r.block_sequences = sequences(Some(header), &r.controls, r.block_ok.len());
        r.header = Some(header.clone());
        r.header_error = None;
        r.probe_extent = extent;
        Ok(r)
    }

    /// Which tones above 23 carry a frame's width probe, and the width they
    /// give (ADR 0007 `broadcast-width-from-probe`); mirrors the simulator's
    /// `frame.probe_extent`. Reads [`extent_samples`] samples from `start`
    /// (zero-padded beyond `rx`).
    ///
    /// Each tone's two probe symbols, divided by the known values, both
    /// measure the channel when the tone carries the probe, so their
    /// correlation Re{z0 conj(z1)} estimates its signal power; without the
    /// probe they are unrelated and it averages zero (clip-and-filter
    /// regrowth above the cap is not the probe sequence). In a broadcast the
    /// data uses exactly the probed tones, so the power of the data symbols
    /// every width has adds to the estimate, less the noise measured on the
    /// same tones in the preamble, header and control symbols (silent above
    /// tone 23). A tone counts when the estimate clears [`EXTENT_SIGMA`]
    /// noise deviations and [`EXTENT_FLOOR_DB`] below the base tones' probe
    /// power; the width is the change point of the run (the count of tones
    /// that do minus those that do not, from tone 24 up, peaks one past the
    /// last probed tone; ties to the higher).
    pub fn probe_extent(
        &mut self,
        rx: &[f64],
        start: usize,
        cp: usize,
        header: &Header,
        opts: &RxOptions,
    ) -> ProbeExtent {
        let (b, m) = (BASE_TONES, params::MAX_TONES);
        if !header.probe {
            return ProbeExtent {
                width: b,
                signal: vec![0.0; m - b],
                threshold: vec![1.0; m - b],
                present: vec![false; m - b],
                data_symbols: 0,
            };
        }
        let cfg = extent_geometry(header, cp);
        let n_data = cfg.data_symbols();
        let rows = cfg.first_data() + n_data;
        let advance = opts.advance.unwrap_or(cp / 4);
        let ws = (start + cp).saturating_sub(advance);
        let need = ws + rows * cfg.stride();
        let y = if rx.len() >= need {
            self.ofdm.demodulate(rx, ws, rows, m, cp, m, None)
        } else {
            let mut padded = rx.to_vec();
            padded.resize(need, 0.0);
            self.ofdm.demodulate(&padded, ws, rows, m, cp, m, None)
        };
        let p0 = cfg.first_probe();
        let known = tx::probe_grid(m, p0);
        let z0: Vec<Complex64> = (0..m).map(|k| y.at(p0, k) / known.at(0, k)).collect();
        let z1: Vec<Complex64> = (0..m).map(|k| y.at(p0 + 1, k) / known.at(1, k)).collect();
        let silent: Vec<usize> = (0..p0).chain(cfg.first_control()..cfg.first_data()).collect();
        let pw: Vec<f64> =
            (0..m).map(|k| silent.iter().map(|&s| y.at(s, k).norm_sqr()).sum::<f64>() / silent.len() as f64).collect();
        let half = EXTENT_NOISE_TONES / 2;
        let nd = n_data as f64;
        let ref_pw: Vec<f64> = (0..b).map(|k| ((z0[k] + z1[k]) / 2.0).norm_sqr()).collect();
        let reference = dsp::median(&ref_pw);
        let floor = reference * 10f64.powf(EXTENT_FLOOR_DB / 10.0);
        let (mut signal, mut threshold, mut present) = (Vec::new(), Vec::new(), Vec::new());
        for k in b..m {
            let (lo, hi) = (k.saturating_sub(half).max(b), (k + half + 1).min(m));
            let noise = pw[lo..hi].iter().sum::<f64>() / (hi - lo) as f64;
            let n_ref = ((hi - lo) * silent.len()) as f64;
            let energy = if n_data > 0 {
                (cfg.first_data()..rows).map(|s| y.at(s, k).norm_sqr()).sum::<f64>() / nd - noise
            } else {
                0.0
            };
            let q = (z0[k] * z1[k].conj()).re;
            let sig = (2.0 * q + nd * energy) / (2.0 + nd);
            let sd = noise * (2.0 + nd + nd * nd / n_ref).sqrt() / (2.0 + nd);
            let thr = (EXTENT_SIGMA * sd).max(floor);
            signal.push(sig);
            threshold.push(thr);
            present.push(sig > thr);
        }
        let (mut run, mut best, mut best_j) = (0i64, 0i64, 0usize);
        for (i, &p) in present.iter().enumerate() {
            run += if p { 1 } else { -1 };
            if run >= best {
                best = run;
                best_j = i + 1;
            }
        }
        ProbeExtent { width: b + best_j, signal, threshold, present, data_symbols: n_data }
    }

    /// Find the strongest frame in `rx` with the SPEC 6.1 detector (blind
    /// prefix) and decode it header first; None if nothing is detected.
    pub fn detect_and_receive<L: LinkSource + ?Sized>(
        &mut self,
        rx: &[f64],
        link: &L,
        opts: &RxOptions,
    ) -> Option<RxResult> {
        let dets = crate::detector::detect(rx, &crate::detector::DetectorConfig::default());
        let forced = link.forced_cp();
        let best = dets
            .iter()
            .filter(|d| forced.is_none_or(|cp| cp == d.cp))
            .max_by(|a, b| a.quality.total_cmp(&b.quality))?;
        Some(self.receive_header_first(rx, link, best.start, best.cp, opts))
    }
}

/// A tone carries the probe when its signal estimate clears this many
/// standard deviations of its noise-only value (ADR 0007 probe extent).
pub const EXTENT_SIGMA: f64 = 5.0;
/// ... and this level against the base tones' probe power, dB
/// (clip-and-filter regrowth just above the cap is about -30 dB; the mic
/// path's limiter and filter tails leave -22 to -25 dB on the next tones).
pub const EXTENT_FLOOR_DB: f64 = -18.0;
/// Neighbouring upper tones pooled for each tone's noise reference.
pub const EXTENT_NOISE_TONES: usize = 11;
/// Width offsets tried around the probe's when no data block decodes there.
pub const EXTENT_RETRY: [i64; 6] = [1, -1, 2, -2, 3, -3];

/// Which tones above 23 carry a frame's width probe ([`Receiver::probe_extent`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeExtent {
    /// Highest tone carrying the probe, plus one (24 if none does).
    pub width: usize,
    /// Per tone 24-55: signal power estimate.
    pub signal: Vec<f64>,
    /// Per tone 24-55: the threshold it was compared with.
    pub threshold: Vec<f64>,
    /// Per tone 24-55: counted as carrying the probe.
    pub present: Vec<bool>,
    /// Data symbols the estimate used.
    pub data_symbols: usize,
}

impl ProbeExtent {
    /// Decision margins at the edge, dB: the last tone in the extent over its
    /// threshold, and the threshold over the first tone beyond (infinite
    /// where there is no such tone).
    pub fn margin_db(&self) -> (f64, f64) {
        let b = BASE_TONES;
        let db = |a: f64, c: f64| 10.0 * (a.max(1e-30) / c.max(1e-30)).log10();
        let inside = if self.width > b {
            db(self.signal[self.width - 1 - b], self.threshold[self.width - 1 - b])
        } else {
            f64::INFINITY
        };
        let outside = if self.width - b < self.signal.len() {
            db(self.threshold[self.width - b], self.signal[self.width - b])
        } else {
            f64::INFINITY
        };
        (inside, outside)
    }
}

/// The frame a header describes at 56 tones: the rows before the data do not
/// depend on the width, and its data symbols are the fewest any width gives.
pub fn extent_geometry(header: &Header, cp: usize) -> FrameConfig {
    let mcs = if header.n_data > 0 { header.mcs } else { 0 };
    FrameConfig::new(params::MAX_TONES, mcs.min(6), header.n_data)
        .and_then(|c| c.with_controls(header.n_control))
        .and_then(|c| c.with_cp(cp))
        .and_then(|c| c.with_probe(true))
        .expect("a 56-tone frame from a decoded header")
}

/// Samples from a frame's start that [`Receiver::probe_extent`] reads.
pub fn extent_samples(header: &Header, cp: usize) -> usize {
    let cfg = extent_geometry(header, cp);
    (cfg.first_data() + cfg.data_symbols()) * cfg.stride()
}

/// Widths to try, nearest first, when nothing decodes at the probe's.
pub fn width_alternatives(width: usize) -> Vec<usize> {
    EXTENT_RETRY
        .iter()
        .map(|d| width as i64 + d)
        .filter(|w| (BASE_TONES as i64..=params::MAX_TONES as i64).contains(w))
        .map(|w| w as usize)
        .collect()
}

/// The link state a frame is decoded with: the caller's for a session frame
/// (header flag bit 3); outside a session, or with no link state (`link`
/// None, as for a CONNECT-ACK before its B.3 is known), the probe's extent
/// as width and cap with no mask (ADR 0007), or 24 tones without a probe.
pub fn frame_link(header: &Header, link: Option<&LinkState>, extent: Option<&ProbeExtent>) -> LinkState {
    if let Some(l) = link.filter(|_| header.session) {
        return l.clone();
    }
    let width = match extent {
        Some(e) if header.probe => e.width,
        _ => BASE_TONES,
    };
    LinkState { cp: link.and_then(|l| l.cp), ..LinkState::from_cap(width, None) }
}

/// The result for a session frame whose header names a mask ID the link
/// state does not hold (ADR 0011 `mask-id`): the header pass, with every
/// control and data block failed and nothing delivered.
fn unknown_mask(mut head: RxResult, header: &Header, id: u8) -> RxResult {
    head.header = Some(header.clone());
    head.header_error = None;
    head.unknown_mask = Some(id);
    head.config = None;
    head.control_ok = vec![false; header.n_control];
    head.control_bits = vec![vec![0; params::CONTROL_INFO_BITS]; header.n_control];
    head.controls = vec![None; header.n_control];
    head.block_ok = vec![false; header.n_data];
    head.block_bits = vec![vec![0; params::DATA_INFO_BITS]; header.n_data];
    head.iterations = vec![0; header.n_data];
    head.block_sequences = None;
    head
}

/// Decode `n` data blocks of `e` coded bits from their demapped LLRs (the
/// first at scrambler position `first_position`), on `opts.threads` threads.
fn decode_data_blocks(llrs: &[f64], e: usize, first_position: usize, n: usize, opts: &RxOptions) -> Vec<DecodeResult> {
    let dcode = ldpc::data_code();
    let rows = dcode.rows_for_length(e);
    let one = |dec: &mut ldpc::Decoder<'_>, i: usize| {
        let mut l = interleave::deinterleave(&llrs[i * e..(i + 1) * e]);
        gold::descramble_llr(&mut l, (first_position + i) as u32);
        dec.decode(&dcode.rate_recover(&l, 0), &rows, opts.max_iter, LDPC_ALPHA)
    };
    let threads = opts.threads.clamp(1, n.max(1));
    if threads == 1 {
        let mut dec = dcode.decoder();
        return (0..n).map(|i| one(&mut dec, i)).collect();
    }
    // Interleaved assignment: block i to thread i mod threads, so a burst
    // that slows a run of blocks spreads over the threads.
    let mut parts: Vec<Vec<(usize, DecodeResult)>> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let one = &one;
                sc.spawn(move || {
                    let mut dec = dcode.decoder();
                    (t..n).step_by(threads).map(|i| (i, one(&mut dec, i))).collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("decoder thread")).collect()
    });
    let mut out: Vec<Option<DecodeResult>> = (0..n).map(|_| None).collect();
    for part in parts.drain(..) {
        for (i, r) in part {
            out[i] = Some(r);
        }
    }
    out.into_iter().map(|r| r.expect("every block decoded")).collect()
}

fn parse_header(ok: bool, bits: &[u8]) -> (Option<Header>, Option<HeaderError>) {
    if !ok {
        return (None, Some(HeaderError::NotDecoded));
    }
    match Header::unpack(bits) {
        Ok(h) => (Some(h), None),
        Err(e) => (None, Some(HeaderError::Fields(e))),
    }
}

/// What the receiver decoded, for rebuilding the frame.
struct Decoded<'a> {
    header_ok: bool,
    header_bits: &'a [u8],
    control_ok: &'a [bool],
    control_bits: &'a [Vec<u8>],
    block_ok: &'a [bool],
    block_bits: &'a [Vec<u8>],
}

/// Fewer known symbols than this: the channel SNR is the fallback estimate
/// (mirrors `MIN_KNOWN_SYMBOLS` in the simulator).
pub const MIN_KNOWN_SYMBOLS: usize = 8;
/// The fallback sees a weak frame through a few symbols: it errs low by this, dB.
pub const FALLBACK_MARGIN_DB: f64 = 1.0;
/// Noise power left in |h|^2 by the S2 estimate (two symbols averaged, 7-tone
/// quadratic smoothing), as a fraction of the S2 noise.
pub const FALLBACK_H_BIAS: f64 = 1.0 / 6.0;
/// Symbols from the frame's start in which the radios' audio level may still
/// be settling (the 2026-11-07 captures: the level rises from 13 to 22 dB
/// below its steady value at the first preamble sample, with a 19 ms time
/// constant, frequency-flat). In the channel-SNR fit each known symbol after
/// the preamble and before this has a gain of its own.
pub const SNR_SETTLE_SYMBOLS: usize = 8;
/// Alternations between the per-tone and per-symbol gains in the SNR fit.
const SETTLE_ITERATIONS: usize = 4;
/// A tone measured on two symbols only (a probed tone the data did not
/// reach) pools its noise with such tones within this many tones.
const LOW_DOF_POOL: usize = 2;

/// The decoder's own estimate, for the fallback.
struct DecoderEstimate<'a> {
    h: &'a [Complex64],
    noise: &'a [f64],
    thermal: &'a [f64],
}

/// ADR 0006 channel SNR (the simulator's `frame.channel_snr`): rebuild the
/// transmitted frame from the decoded bits (encode, map, clip-and-filter),
/// demodulate it with the receiver's window, and fit
/// received = g_k a_s x drift x rebuilt over the symbols after the preamble
/// whose content is known: a gain g_k per tone, a gain a_s per known symbol
/// before [`SNR_SETTLE_SYMBOLS`] (the radios' level settling; 1 after; its
/// residual taken at the steady level), and the pilots' phase track refined
/// against the rebuilt tones as (alpha + beta f) per symbol. The preamble
/// is left out (built for a low crest factor, it escapes the radios'
/// limiters, and their level is still settling under it). The residual,
/// per degree of freedom, is
/// everything the channel added, without the designed clipping (which is in
/// the rebuilt reference); the signal is the fitted gain on the rebuilt tones
/// as sent (a mask's boost counts). With fewer than [`MIN_KNOWN_SYMBOLS`]
/// known symbols, or a frame with data none of whose data symbols is known,
/// it is the fallback, which counts the designed clipping and errs low.
fn rebuild_snr(
    tones: &Grid,
    corr: &Grid,
    cfg: &FrameConfig,
    advance: usize,
    dec: &Decoded,
    est: &DecoderEstimate,
) -> ChannelSnr {
    let Decoded { header_ok, header_bits, control_ok, control_bits, block_ok, block_bits } = *dec;
    let w = cfg.n_tones();
    let n_sym = cfg.total_symbols();
    let first = PREAMBLE_SYMBOLS;
    // Known symbols and the tones each one carries.
    let base: Vec<usize> = (0..BASE_TONES).collect();
    let all: Vec<usize> = (0..w).collect();
    let mut known: Vec<(usize, &[usize])> = (0..first).map(|s| (s, &base[..])).collect();
    if header_ok {
        known.extend((cfg.first_header()..cfg.first_probe()).map(|s| (s, &base[..])));
    }
    known.extend((cfg.first_probe()..cfg.first_control()).map(|s| (s, &all[..cfg.cap])));
    let ct = cfg.control_tones.len();
    for cs in 0..cfg.control_symbols() {
        // Points cs*D .. (cs+1)*D; control block i holds points i*256 .. (i+1)*256.
        let (p0, p1) = (cs * ct, (cs + 1) * ct);
        let half = CONTROL_CODED_BITS / 2;
        if (0..cfg.n_control).filter(|&i| i * half < p1 && (i + 1) * half > p0).all(|i| control_ok[i]) {
            known.push((cfg.first_control() + cs, &cfg.control_active_tones[..]));
        }
    }
    let (q, d, e) = (cfg.rung().bits_per_point, cfg.data_tones.len(), cfg.rung().coded_bits);
    for ds in 0..cfg.data_symbols() {
        // Points ds*D .. (ds+1)*D; block i holds points i*E/Q .. (i+1)*E/Q, padding after.
        let (p0, p1) = (ds * d * q, (ds + 1) * d * q);
        let all_ok = (0..cfg.n_blocks).filter(|&i| i * e < p1 && (i + 1) * e > p0).all(|i| block_ok[i]);
        if all_ok {
            known.push((cfg.first_data() + ds, &cfg.data_active_tones[..]));
        }
    }
    known.sort_by_key(|&(s, _)| s);
    let n_known = known.len();
    // A frame with data whose data symbols all failed is weak, and its header
    // decoding at all selects for a lucky, low-noise header: measured over
    // the preamble and header alone it reads high. Header-only and
    // control-only frames are measured over everything they carry.
    let no_data_known = cfg.data_symbols() > 0 && !known.iter().any(|&(s, _)| s >= cfg.first_data());
    if n_known < MIN_KNOWN_SYMBOLS || no_data_known {
        return fallback_snr(cfg, est, n_known);
    }

    let hdr: Vec<u8> = header_bits[..params::HEADER_INFO_BITS - params::CRC_BITS].to_vec();
    let cmsgs: Vec<Vec<u8>> =
        control_bits.iter().map(|b| b[..params::CONTROL_INFO_BITS - params::CRC_BITS].to_vec()).collect();
    let payloads: Vec<Vec<u8>> = block_bits.iter().map(|b| b[..params::DATA_PAYLOAD_BITS].to_vec()).collect();
    let frame = tx::Transmitter::new().build_with_controls(cfg, &hdr, &cmsgs, &payloads);
    let mut ofdm = Ofdm::new();
    let reference = ofdm.demodulate(&frame.samples, cfg.cp - advance, n_sym, w, cfg.cp, cfg.width, None);
    // The received tones with the mask's known boost put back, as sent.
    let boost = |s: usize| {
        if s >= cfg.first_data() {
            cfg.data_gain()
        } else if s >= cfg.first_control() {
            cfg.control_gain()
        } else {
            1.0
        }
    };
    // The preamble is left out of the fit: its symbols are built for a low
    // crest factor (SPEC 6.1), so a radio's limiter, which sets the noise of
    // a strong FM link, does not touch them, and the radios' level is still
    // settling under them (over the air they read 5-8 dB cleaner than the
    // data). Counted in, they let a short frame over-report.
    let known: Vec<(usize, &[usize])> = known.into_iter().filter(|&(s, _)| s >= first).collect();
    // Dense (known symbol, tone) cells; x is zero where the symbol carries nothing.
    let n = known.len();
    let zero = Complex64::new(0.0, 0.0);
    let one = Complex64::new(1.0, 0.0);
    let mut y = vec![zero; n * w];
    let mut x = vec![zero; n * w];
    let mut live = vec![false; n * w];
    let mut dc = vec![one; n * w];
    for (i, &(s, ks)) in known.iter().enumerate() {
        for k in 0..w {
            y[i * w + k] = tones.at(s, k) * boost(s);
            if s >= first {
                dc[i * w + k] = corr.at(s - first, k);
            }
        }
        for &k in ks {
            x[i * w + k] = reference.at(s, k);
            live[i * w + k] = true;
        }
    }
    let u: Vec<f64> = known.iter().map(|&(s, _)| s as f64 - 2.5).collect();
    // Symbols with a gain of their own: the settling ones, when a later symbol anchors the level.
    let settle: Vec<bool> = if known.iter().any(|&(s, _)| s >= SNR_SETTLE_SYMBOLS) {
        known.iter().map(|&(s, _)| s < SNR_SETTLE_SYMBOLS).collect()
    } else {
        vec![false; n]
    };
    let mut a = vec![one; n];
    let ones = vec![1.0; w];
    let mut yc = vec![zero; n * w];
    let mut g = vec![zero; w];
    for _ in 0..3 {
        for c in 0..n * w {
            yc[c] = y[c] / dc[c];
        }
        fit_gains(&yc, &x, w, &settle, &ones, &mut a, &mut g);
        // The common phase drift (alpha + beta f) per symbol re-fit against
        // the rebuilt tones: pilots alone leave a few hundredths of a radian
        // of the designed clipping's phase noise.
        let (mut m00, mut m01, mut m11, mut b0, mut b1) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..n {
            for (k, gk) in g.iter().enumerate() {
                let c = i * w + k;
                if !live[c] {
                    continue;
                }
                let r = gk * a[i] * x[c];
                let theta = (y[c] * (r * dc[c]).conj() + 1e-300).arg();
                let wt = r.norm_sqr();
                let (p, q) = (u[i], u[i] * (BIN_OFFSET + k as f64));
                m00 += wt * p * p;
                m01 += wt * p * q;
                m11 += wt * q * q;
                b0 += wt * p * theta;
                b1 += wt * q * theta;
            }
        }
        let det = m00 * m11 - m01 * m01;
        let (p0, p1) = if det.abs() > 1e-12 * (m00 * m11).max(1e-300) {
            ((m11 * b0 - m01 * b1) / det, (m00 * b1 - m01 * b0) / det)
        } else if m00 > 0.0 {
            (b0 / m00, 0.0)
        } else {
            (0.0, 0.0)
        };
        for i in 0..n {
            for k in 0..w {
                let ff = BIN_OFFSET + k as f64;
                dc[i * w + k] *= Complex64::from_polar(1.0, p0 * u[i] + p1 * u[i] * ff);
            }
        }
    }
    for c in 0..n * w {
        yc[c] = y[c] / dc[c];
    }
    fit_gains(&yc, &x, w, &settle, &ones, &mut a, &mut g);
    // Each own gain takes one complex degree of freedom from its symbol's tones.
    let row_live: Vec<f64> = (0..n).map(|i| live[i * w..(i + 1) * w].iter().filter(|&&l| l).count() as f64).collect();
    let mut per_tone = vec![f64::NAN; w];
    let mut sig = vec![0.0; w];
    let mut noise = vec![0.0; w];
    let mut ok = vec![false; w];
    let mut measured_tone = vec![false; w];
    let mut resid_k = vec![0.0; w];
    let mut dof_k = vec![0.0; w];
    // Twice: then again with each tone weighted by its inverse noise in the
    // settling symbols' gains, so a few very noisy tones (an interferer, a
    // tone the mask will drop) do not spread their noise over the others.
    let any_settle = settle.iter().any(|&v| v);
    for pass in 0..2 {
        if pass == 1 {
            if !any_settle {
                break;
            }
            let floor = 1e-12 * noise.iter().copied().fold(1e-30, f64::max);
            let wt: Vec<f64> = noise.iter().map(|&v| 1.0 / v.max(floor)).collect();
            fit_gains(&yc, &x, w, &settle, &wt, &mut a, &mut g);
        }
        for k in 0..w {
            let (mut nk, mut mk, mut resid, mut den, mut xx) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for i in 0..n {
                let c = i * w + k;
                if !live[c] {
                    continue;
                }
                let xa = a[i] * x[c];
                nk += 1.0;
                if settle[i] {
                    mk += 1.0 / row_live[i];
                }
                // A settling symbol's level scales its noise too (the radios'
                // clipping, which follows the level): its residual counts at the
                // steady level.
                resid += (yc[c] - g[k] * xa).norm_sqr() / a[i].norm_sqr().max(1e-30);
                den += xa.norm_sqr();
                xx += x[c].norm_sqr();
            }
            noise[k] = resid / (nk - 1.0 - mk).max(1.0);
            resid_k[k] = resid;
            dof_k[k] = nk - 1.0 - mk;
            // |g|^2 over-reads by the noise left in the fit (noise / sum |x|^2).
            let g2 = g[k].norm_sqr();
            let h2 = (g2 - noise[k] / den.max(1e-30)).max(0.05 * g2);
            sig[k] = h2 * xx / nk.max(1.0);
            ok[k] = nk >= 3.0;
            // A tone with one degree of freedom left (a probed tone above 23 in
            // a frame whose data did not reach it: its two probe symbols) is
            // measured in its own right, not taken from its neighbours.
            measured_tone[k] = dof_k[k] >= 1.0 - 1e-9;
        }
    }
    let tiny = 1e-12 * sig.iter().copied().fold(1e-30, f64::max);
    let t: Vec<usize> = cfg.data_active_tones.iter().copied().filter(|&k| k < w && ok[k]).collect();
    if t.is_empty() {
        return fallback_snr(cfg, est, n_known);
    }
    let effective =
        10.0 * (t.iter().map(|&k| sig[k]).sum::<f64>() / t.iter().map(|&k| noise[k]).sum::<f64>().max(tiny)).log10();
    // One degree of freedom scatters a tone's noise by +-9 dB: such tones
    // pool their residual with their like within LOW_DOF_POOL tones.
    let low: Vec<bool> = (0..w).map(|k| measured_tone[k] && dof_k[k] < 2.0 - 1e-9).collect();
    let noise_tone: Vec<f64> = (0..w)
        .map(|k| {
            if !low[k] {
                return noise[k];
            }
            let js = k.saturating_sub(LOW_DOF_POOL)..(k + LOW_DOF_POOL + 1).min(w);
            let (r, d) = js.filter(|&j| low[j]).fold((0.0, 0.0), |(r, d), j| (r + resid_k[j], d + dof_k[j]));
            r / d
        })
        .collect();
    let measured: Vec<f64> = (0..w).filter(|&k| measured_tone[k]).map(|k| k as f64).collect();
    let values: Vec<f64> =
        (0..w).filter(|&k| measured_tone[k]).map(|k| 10.0 * (sig[k] / noise_tone[k].max(tiny)).log10()).collect();
    for (k, v) in per_tone.iter_mut().enumerate() {
        *v = dsp::interp(k as f64, &measured, &values);
    }
    ChannelSnr { per_tone_db: per_tone, effective_db: effective, known_symbols: n_known, fallback: false }
}

/// Per-tone gains g (and the own gains a of the `settle` symbols, 1 for the
/// rest) of yc = g_k a_s x by alternating least squares, each tone weighted
/// by `wt` in the symbols' gains.
#[allow(clippy::too_many_arguments)]
fn fit_gains(
    yc: &[Complex64],
    x: &[Complex64],
    w: usize,
    settle: &[bool],
    wt: &[f64],
    a: &mut [Complex64],
    g: &mut [Complex64],
) {
    let n = a.len();
    let tone_gains = |a: &[Complex64], g: &mut [Complex64]| {
        for (k, gk) in g.iter_mut().enumerate() {
            let (mut num, mut den) = (Complex64::new(0.0, 0.0), 0.0);
            for i in 0..n {
                let xa = a[i] * x[i * w + k];
                num += yc[i * w + k] * xa.conj();
                den += xa.norm_sqr();
            }
            *gk = num / den.max(1e-30);
        }
    };
    if !settle.iter().any(|&v| v) {
        tone_gains(a, g);
        return;
    }
    for _ in 0..SETTLE_ITERATIONS {
        tone_gains(a, g);
        for i in (0..n).filter(|&i| settle[i]) {
            let (mut num, mut den) = (Complex64::new(0.0, 0.0), 0.0);
            for (k, (gk, wk)) in g.iter().zip(wt).enumerate() {
                let r = gk * x[i * w + k];
                num += yc[i * w + k] * r.conj() * *wk;
                den += r.norm_sqr() * wk;
            }
            a[i] = num / den.max(1e-300);
        }
    }
    tone_gains(a, g);
}

/// The fallback channel SNR (the simulator's `_clip_included_snr`): the
/// decoder's estimate over the data's active tones, which counts the
/// designed clipping as noise, less [`FALLBACK_MARGIN_DB`].
fn fallback_snr(cfg: &FrameConfig, est: &DecoderEstimate, known_symbols: usize) -> ChannelSnr {
    // |h|^2 carries the noise left in the smoothed estimate: take it out, or a weak frame reads high.
    let p: Vec<f64> = est
        .h
        .iter()
        .zip(est.thermal)
        .map(|(h, t)| (h.norm_sqr() - t * FALLBACK_H_BIAS).max(0.1 * h.norm_sqr()))
        .collect();
    let t: Vec<usize> = cfg.data_active_tones.iter().copied().filter(|&k| k < p.len()).collect(); // no probe: the base tones
    let overall = 10.0 * (t.iter().map(|&k| p[k]).sum::<f64>() / t.iter().map(|&k| est.noise[k]).sum::<f64>()).log10();
    let per_tone =
        p.iter().zip(est.noise).map(|(p, n)| 10.0 * (p / n.max(1e-30)).log10() - FALLBACK_MARGIN_DB).collect();
    ChannelSnr { per_tone_db: per_tone, effective_db: overall - FALLBACK_MARGIN_DB, known_symbols, fallback: true }
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
