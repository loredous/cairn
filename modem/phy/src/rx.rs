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
//! from it; width and mask come from the caller's [`LinkState`], the prefix
//! from the detector.
//!
//! Arithmetic is `f64` (LDPC in `f32`, as the simulator), so results agree
//! with the simulator to rounding, not bit for bit.

use std::f64::consts::PI;

use num_complex::Complex64;

use crate::dsp;
use crate::fields::{ControlBlock, FieldError, Header};
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
    /// Sample-clock drift estimate, samples per symbol.
    pub drift: f64,
    /// Channel SNR excluding the designed clipping (ADR 0006), if measured.
    pub channel_snr: Option<ChannelSnr>,
    /// For a frame outside a session with a probe: which tones carried it
    /// (ADR 0007), the width it gave, and the width the frame decoded at.
    pub probe_extent: Option<ProbeExtent>,
}

impl RxResult {
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
    /// Per tone, dB (NaN where no symbol of the tone was known).
    pub per_tone_db: Vec<f64>,
    /// Over the data symbols' active tones: total signal over total noise, dB,
    /// average-referenced (add the clip offset for the 7 dB scale).
    pub effective_db: f64,
    /// Symbols whose content was known (preamble, probe, header if decoded,
    /// data symbols whose blocks all decoded).
    pub known_symbols: usize,
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
    // A pilot absent from all but one row (an upper pilot in a frame with no
    // data symbols) tells nothing about drift.
    let used: Vec<usize> =
        (0..pilots.len()).filter(|&j| obs.iter().filter(|row| row[j].is_some()).count() >= 2).collect();
    let mut slopes = vec![0.0; used.len()];
    for (slope, &j) in slopes.iter_mut().zip(&used) {
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
        let theta = dsp::unwrap(&sm.iter().map(|v| v.arg()).collect::<Vec<_>>());
        let rm = dsp::mean(&rows);
        let tm = dsp::mean(&theta);
        let num: f64 = rows.iter().zip(&theta).map(|(r, t)| (r - rm) * (t - tm)).sum();
        let den: f64 = rows.iter().map(|r| (r - rm) * (r - rm)).sum();
        *slope = num / den.max(1e-12);
    }
    let f: Vec<f64> = used.iter().map(|&j| BIN_OFFSET + pilots[j] as f64).collect();
    let (beta, alpha) = if used.len() > 1 { dsp::linear_fit(&f, &slopes) } else { (0.0, slopes[0]) };
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
        let channel_snr = opts.rebuild_snr.then(|| rebuild_snr(&tones, &corr_kept, cfg, advance, &decoded));
        let (header, header_error) = parse_header(hres.ok, &hres.bits);
        let controls = control_ok
            .iter()
            .zip(&control_bits)
            .map(|(&ok, b)| if ok { ControlBlock::unpack(b).ok() } else { None })
            .collect();
        RxResult {
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
            probe_extent: None,
        }
    }
}

impl Receiver {
    /// Decode a frame without being told its configuration (SPEC 8.2):
    /// decode the preamble and header on the base tones from `start` with
    /// prefix `cp` (the detector's), unpack the header, take the rung, the
    /// control and data block counts and the probe flag from it, and decode
    /// the whole frame at the link's width and mask. A frame whose header
    /// fails (decode, CRC, fields, or no decodable geometry) comes back with
    /// `header` None, `header_error` set, `config` None and no blocks.
    pub fn receive_header_first(
        &mut self,
        rx: &[f64],
        link: &LinkState,
        start: usize,
        cp: usize,
        opts: &RxOptions,
    ) -> RxResult {
        let head = self.receive_header(rx, start, cp, opts);
        let Some(header) = head.header.clone() else {
            return head;
        };
        let extent = (!header.session && header.probe).then(|| self.probe_extent(rx, start, cp, &header, opts));
        self.receive_frame(rx, &header, link, extent, start, cp, opts).unwrap_or_else(|e| {
            let mut head = head;
            head.header = None;
            head.header_error = Some(HeaderError::Config(e));
            head
        })
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
    /// width, mask and cap; any other frame at the width of its probe
    /// (`extent`, from [`Receiver::probe_extent`]; 24 tones without a probe).
    /// If no data block decodes at the probe's width, the widths around it
    /// are tried ([`width_alternatives`]) and the first at which a block
    /// decodes is kept. Err if the header describes no decodable frame.
    #[allow(clippy::too_many_arguments)]
    pub fn receive_frame(
        &mut self,
        rx: &[f64],
        header: &Header,
        link: &LinkState,
        extent: Option<ProbeExtent>,
        start: usize,
        cp: usize,
        opts: &RxOptions,
    ) -> Result<RxResult, ConfigError> {
        let cfg = FrameConfig::from_header(header, &frame_link(header, link, extent.as_ref()), cp)?;
        let mut r = self.receive(rx, &cfg, start, opts);
        if let Some(ext) = extent.as_ref().filter(|_| cfg.n_blocks > 0 && !r.block_ok.iter().any(|&b| b)) {
            for w in width_alternatives(ext.width) {
                let alt = LinkState { width: w, usable: None, cp: link.cp, cap: Some(w) };
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
    pub fn detect_and_receive(&mut self, rx: &[f64], link: &LinkState, opts: &RxOptions) -> Option<RxResult> {
        let dets = crate::detector::detect(rx, &crate::detector::DetectorConfig::default());
        let best = dets
            .iter()
            .filter(|d| link.cp.is_none_or(|cp| cp == d.cp))
            .max_by(|a, b| a.quality.total_cmp(&b.quality))?;
        Some(self.receive_header_first(rx, link, best.start, best.cp, opts))
    }
}

/// A tone carries the probe when its signal estimate clears this many
/// standard deviations of its noise-only value (ADR 0007 probe extent).
pub const EXTENT_SIGMA: f64 = 5.0;
/// ... and this level against the base tones' probe power, dB
/// (clip-and-filter regrowth just above the cap is about -30 dB).
pub const EXTENT_FLOOR_DB: f64 = -24.0;
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
/// (header flag bit 3); outside a session, the probe's extent as width and
/// cap with no mask (ADR 0007), or 24 tones without a probe.
pub fn frame_link(header: &Header, link: &LinkState, extent: Option<&ProbeExtent>) -> LinkState {
    if header.session {
        return link.clone();
    }
    let width = match extent {
        Some(e) if header.probe => e.width,
        _ => BASE_TONES,
    };
    LinkState { width, usable: None, cp: link.cp, cap: Some(width) }
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

/// ADR 0006 channel SNR: rebuild the transmitted frame from the decoded bits
/// (encode, map, clip-and-filter), demodulate it with the receiver's window,
/// and fit received = g_k x corr x rebuilt per tone over the symbols whose
/// content is known; the residual is everything the channel added, without
/// the designed clipping (which is in the rebuilt reference).
fn rebuild_snr(tones: &Grid, corr: &Grid, cfg: &FrameConfig, advance: usize, dec: &Decoded) -> ChannelSnr {
    let Decoded { header_ok, header_bits, control_ok, control_bits, block_ok, block_bits } = *dec;
    let w = cfg.n_tones();
    let n_sym = cfg.total_symbols();
    let first = PREAMBLE_SYMBOLS;
    let hdr: Vec<u8> = header_bits[..params::HEADER_INFO_BITS - params::CRC_BITS].to_vec();
    let cmsgs: Vec<Vec<u8>> =
        control_bits.iter().map(|b| b[..params::CONTROL_INFO_BITS - params::CRC_BITS].to_vec()).collect();
    let payloads: Vec<Vec<u8>> = block_bits.iter().map(|b| b[..params::DATA_PAYLOAD_BITS].to_vec()).collect();
    let frame = tx::Transmitter::new().build_with_controls(cfg, &hdr, &cmsgs, &payloads);
    let mut ofdm = Ofdm::new();
    let mut reference = ofdm.demodulate(&frame.samples, cfg.cp - advance, n_sym, w, cfg.cp, cfg.width, None);
    let cgain = cfg.control_gain();
    for s in cfg.first_control()..cfg.first_data() {
        for v in reference.row_mut(s) {
            *v /= cgain;
        }
    }
    let gain = cfg.data_gain();
    for s in cfg.first_data()..n_sym {
        for v in reference.row_mut(s) {
            *v /= gain;
        }
    }
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
    // The pilot track is a per-tone phase ramp over symbols; extend it back
    // over the preamble rows.
    let step: Vec<Complex64> = (0..w)
        .map(|k| if corr.rows > 1 { corr.at(1, k) * corr.at(0, k).conj() } else { Complex64::new(1.0, 0.0) })
        .collect();
    // Per tone: (symbol, received, rebuilt reference with the phase track).
    let mut obs: Vec<Vec<(f64, Complex64, Complex64)>> = vec![Vec::new(); w];
    for &(s, ks) in &known {
        for &k in ks {
            let c =
                if s >= first { corr.at(s - first, k) } else { corr.at(0, k) * step[k].powi(s as i32 - first as i32) };
            obs[k].push((s as f64, tones.at(s, k), reference.at(s, k) * c));
        }
    }
    let mut per_tone = vec![f64::NAN; w];
    let (mut sig_sum, mut noise_sum) = (0.0, 0.0);
    let data_act = &cfg.data_active_tones;
    for (k, o) in obs.iter().enumerate() {
        if o.len() < 4 {
            continue;
        }
        let (sig, resid) = tone_fit(o);
        per_tone[k] = 10.0 * (sig / resid).log10();
        if data_act.contains(&k) {
            sig_sum += sig;
            noise_sum += resid;
        }
    }
    ChannelSnr { per_tone_db: per_tone, effective_db: 10.0 * (sig_sum / noise_sum).log10(), known_symbols: known.len() }
}

/// Signal and residual power of one tone: received = g x reference by least
/// squares, then again with a residual phase ramp over the symbols fitted
/// (the pilots' drift track is noisy at high SNR, and a timing-slope error
/// grows over a long frame); the smaller residual, per degree of freedom.
fn tone_fit(o: &[(f64, Complex64, Complex64)]) -> (f64, f64) {
    let fit = |rot: &dyn Fn(f64) -> Complex64| -> (Complex64, f64, f64) {
        let (mut yx, mut xx) = (Complex64::new(0.0, 0.0), 0.0);
        for &(s, y, x) in o {
            let x = x * rot(s);
            yx += y * x.conj();
            xx += x.norm_sqr();
        }
        let g = yx / xx;
        let r: f64 = o.iter().map(|&(s, y, x)| (y - g * x * rot(s)).norm_sqr()).sum();
        (g, xx, r)
    };
    let n = o.len() as f64;
    let (g, xx, r1) = fit(&|_| Complex64::new(1.0, 0.0));
    // Residual phase against the first fit, weighted line over the symbols.
    let (mut sw, mut ss, mut st, mut sss, mut sst) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(s, y, x) in o {
        let z = y * (g * x).conj();
        let wgt = (g * x).norm_sqr();
        let t = z.arg();
        sw += wgt;
        ss += wgt * s;
        st += wgt * t;
        sss += wgt * s * s;
        sst += wgt * s * t;
    }
    let den = sw * sss - ss * ss;
    let slope = if den > 0.0 { (sw * sst - ss * st) / den } else { 0.0 };
    let (g2, xx2, r2) = fit(&|s| Complex64::from_polar(1.0, slope * s));
    // Keep the ramp only if it removes far more than the half complex degree
    // of freedom of noise it absorbs by itself; choosing the smaller residual
    // outright would bias the noise low and over-report.
    let per_dof = r1 / (n - 1.0);
    if r1 - r2 > 8.0 * per_dof {
        (g2.norm_sqr() * xx2 / n, (r2 / (n - 1.5)).max(1e-30))
    } else {
        (g.norm_sqr() * xx / n, per_dof.max(1e-30))
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
