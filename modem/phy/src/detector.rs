//! SPEC 6.1 preamble detector, ported from `sim/cairn_sim/preamble.py`.
//!
//! Per prefix hypothesis cp in (60, 120), lag L = 480 + cp:
//!
//! 1. Front end: band-limit to 300-2700 Hz, keep the analytic signal and
//!    decimate to 8 kHz ([`analytic_decimate`]).
//! 2. Autocorrelation (Schmidl-Cox) plateau metric M at the repeat lag.
//! 3. Cross-correlation rho with the stored S1, S1 waveform for each
//!    sample-clock offset of the grid (-100, 0, +100 ppm).
//! 4. A detection is a local maximum (over +-3 symbol periods) of
//!    max_{cp, ppm} rho where rho >= 0.55 and, for the same cp, M >= 0.45.
//! 5. Fine timing: the analytic S2, S2 waveform correlated at 48 kHz over
//!    +-12 samples around the coarse position, with parabolic interpolation.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use num_complex::Complex64;
use realfft::{RealFftPlanner, RealToComplex};
use rustfft::{Fft, FftPlanner};

use crate::dsp;
use crate::params::{CP_LONG, CP_SHORT, FS, N_USEFUL};
use crate::preamble::{s1, s2, tone_waveform};

/// 48 kHz to 8 kHz complex processing.
pub const DECIM: usize = 6;
const LH: usize = 6 * 192 + 1; // front-end FIR length at 48 kHz (24 ms)
const Q: usize = (LH - 1) / DECIM;
const MF: usize = 8192; // decimated FFT block
const BAND_CENTER_HZ: f64 = 1500.0;
const HALF_BAND_HZ: f64 = 1290.0;
const KAISER_BETA: f64 = 6.8;

/// Normalized S1 cross-correlation threshold (rho).
pub const XCORR_THRESHOLD: f64 = 0.55;
/// Schmidl-Cox plateau threshold (M).
pub const AUTOCORR_THRESHOLD: f64 = 0.45;
/// Sample-clock offset grid (SPEC 6.1), ppm.
pub const PPM_GRID: [f64; 3] = [-100.0, 0.0, 100.0];
/// Prefix hypotheses.
pub const PREFIXES: [usize; 2] = [CP_SHORT, CP_LONG];
/// Fine-timing search, samples either side.
pub const FINE_SEARCH: usize = 12;

/// One detected preamble.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    /// 48 kHz index of the first sample of the first S1 prefix.
    pub start: usize,
    /// Detected prefix length (60 or 120).
    pub cp: usize,
    /// Normalized S1 cross-correlation rho, 0..1.
    pub quality: f64,
    /// Schmidl-Cox plateau metric M at the detected lag.
    pub autocorr: f64,
    /// Rough in-band SNR estimate, rho^2 / (1 - rho^2), dB.
    pub snr_db: f64,
    /// Sample-clock grid point that matched best.
    pub ppm: f64,
    /// Start from the 8 kHz S1 correlation, before S2 refinement.
    pub coarse_start: f64,
}

/// Detector settings.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectorConfig {
    /// rho threshold.
    pub xcorr_threshold: f64,
    /// M threshold.
    pub autocorr_threshold: f64,
    /// Sample-clock grid, ppm.
    pub ppm_grid: Vec<f64>,
    /// Prefix hypotheses.
    pub prefixes: Vec<usize>,
    /// Fine-timing search half-width.
    pub fine_search: usize,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            xcorr_threshold: XCORR_THRESHOLD,
            autocorr_threshold: AUTOCORR_THRESHOLD,
            ppm_grid: PPM_GRID.to_vec(),
            prefixes: PREFIXES.to_vec(),
            fine_search: FINE_SEARCH,
        }
    }
}

struct FrontEnd {
    resp: Vec<Complex64>,
    rfft: Arc<dyn RealToComplex<f64>>,
    ifft: Arc<dyn Fft<f64>>,
}

fn front_end() -> &'static FrontEnd {
    static F: OnceLock<FrontEnd> = OnceLock::new();
    F.get_or_init(|| {
        let h = dsp::firwin_kaiser(LH, HALF_BAND_HZ, KAISER_BETA, FS as f64);
        let nf = DECIM * MF;
        let mut buf = vec![Complex64::new(0.0, 0.0); nf];
        for (n, &v) in h.iter().enumerate() {
            let k = n as f64 - (LH as f64 - 1.0) / 2.0;
            let ph = 2.0 * std::f64::consts::PI * BAND_CENTER_HZ * k / FS as f64;
            buf[n] = Complex64::from_polar(2.0 * v, ph);
        }
        let mut planner = FftPlanner::new();
        planner.plan_fft_forward(nf).process(&mut buf);
        buf.truncate(MF);
        let rfft = RealFftPlanner::<f64>::new().plan_fft_forward(nf);
        let ifft = planner.plan_fft_inverse(MF);
        FrontEnd { resp: buf, rfft, ifft }
    })
}

/// Band-limit real 48 kHz audio to about 300-2700 Hz, keep the positive
/// frequencies and decimate by 6: z[m] is the analytic signal at sample 6 m,
/// lined up with the input (the filter delay is removed).
pub fn analytic_decimate(x: &[f64]) -> Vec<Complex64> {
    let fe = front_end();
    let n_out = x.len().div_ceil(DECIM);
    let c = (LH - 1) / 2;
    let nf = DECIM * MF;
    let step = DECIM * (MF - Q);
    let mut xp = vec![0.0; c + DECIM * n_out + nf + step];
    xp[c..c + x.len()].copy_from_slice(x);
    let mut out = vec![Complex64::new(0.0, 0.0); n_out];
    let mut inbuf = fe.rfft.make_input_vec();
    let mut spec = fe.rfft.make_output_vec();
    let mut seg = vec![Complex64::new(0.0, 0.0); MF];
    let scale = 1.0 / (MF as f64 * DECIM as f64);
    let mut s = 0;
    while s < DECIM * n_out {
        inbuf.copy_from_slice(&xp[s..s + nf]);
        fe.rfft.process(&mut inbuf, &mut spec).expect("rfft");
        for ((o, a), b) in seg.iter_mut().zip(&spec[..MF]).zip(&fe.resp) {
            *o = a * b;
        }
        fe.ifft.process(&mut seg);
        let m0 = s / DECIM;
        let take = (MF - Q).min(n_out - m0);
        for i in 0..take {
            out[m0 + i] = seg[Q + i] * scale;
        }
        s += step;
    }
    out
}

fn stride(cp: usize) -> usize {
    N_USEFUL + cp
}

/// Analytic S1, S1 (with prefixes) at 8 kHz, stretched by `ppm`.
fn s1_reference(cp: usize, ppm: f64) -> Vec<Complex64> {
    let st = stride(cp) as f64;
    let vals = s1();
    (0..2 * stride(cp) / DECIM)
        .map(|m| {
            let t = (DECIM * m) as f64 * (1.0 + ppm * 1e-6);
            let sym = (t / st).floor();
            let local = t - sym * st - cp as f64;
            tone_waveform(&vals, local)
        })
        .collect()
}

/// Analytic S2, S2 (with prefixes) at 48 kHz.
fn s2_reference(cp: usize) -> Vec<Complex64> {
    let st = stride(cp);
    let vals = s2();
    (0..2 * st).map(|t| tone_waveform(&vals, (t % st) as f64 - cp as f64)).collect()
}

/// FFT size, forward and inverse plans, and the scaled conjugate spectra of
/// the S1 references at that size.
type Spectra = (usize, Arc<dyn Fft<f64>>, Arc<dyn Fft<f64>>, Vec<Vec<Complex64>>);

struct References {
    s1: Vec<((usize, u64), Vec<Complex64>, f64)>,
    s2: Vec<(usize, Vec<Complex64>)>,
    /// FFT size, its plans, and the conjugate spectra of the S1 references.
    spectra: Option<Spectra>,
}

impl References {
    /// Plans and reference spectra for correlating `len` samples.
    fn spectra(&mut self, len: usize) -> &Spectra {
        let m = len.next_power_of_two();
        if self.spectra.as_ref().is_none_or(|s| s.0 != m) {
            let mut planner = FftPlanner::new();
            let fwd = planner.plan_fft_forward(m);
            let inv = planner.plan_fft_inverse(m);
            let specs = self
                .s1
                .iter()
                .map(|(_, r, _)| {
                    let mut b = vec![Complex64::new(0.0, 0.0); m];
                    b[..r.len()].copy_from_slice(r);
                    fwd.process(&mut b);
                    b.iter().map(|v| v.conj() / m as f64).collect()
                })
                .collect();
            self.spectra = Some((m, fwd, inv, specs));
        }
        self.spectra.as_ref().unwrap()
    }
}

fn references(cfg: &DetectorConfig) -> References {
    let mut s1v = Vec::new();
    for &cp in &cfg.prefixes {
        for &ppm in &cfg.ppm_grid {
            let r = s1_reference(cp, ppm);
            let e: f64 = r.iter().map(|v| v.norm_sqr()).sum();
            s1v.push(((cp, ppm.to_bits()), r, e));
        }
    }
    let s2v = cfg.prefixes.iter().map(|&cp| (cp, s2_reference(cp))).collect();
    References { s1: s1v, s2: s2v, spectra: None }
}

/// out[u] = sum(v[u .. u + len]) via a running cumulative sum (as the sim).
fn moving_sum<T>(v: &[T], len: usize) -> Vec<T>
where
    T: Copy + Default + std::ops::Add<Output = T> + std::ops::Sub<Output = T>,
{
    if v.len() < len {
        return Vec::new();
    }
    let mut cs = Vec::with_capacity(v.len() + 1);
    let mut acc = T::default();
    cs.push(acc);
    for &x in v {
        acc = acc + x;
        cs.push(acc);
    }
    (0..=v.len() - len).map(|u| cs[u + len] - cs[u]).collect()
}

/// Sliding maximum over a centred window of odd length `w` (zero outside).
fn max_filter(x: &[f64], w: usize) -> Vec<f64> {
    let half = w / 2;
    let n = x.len();
    let mut out = vec![0.0; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    let mut next = 0;
    for (i, o) in out.iter_mut().enumerate() {
        let hi = (i + half).min(n - 1);
        while next <= hi {
            while dq.back().is_some_and(|&j| x[j] <= x[next]) {
                dq.pop_back();
            }
            dq.push_back(next);
            next += 1;
        }
        while dq.front().is_some_and(|&j| j + half < i) {
            dq.pop_front();
        }
        let m = x[*dq.front().unwrap()];
        // Window positions outside the signal count as zero.
        let pads = i < half || i + half >= n;
        *o = if pads { m.max(0.0) } else { m };
    }
    out
}

/// Per-position detector metrics of one scan, for measuring margins.
#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    /// Largest rho regardless of the autocorrelation gate.
    pub max_rho: f64,
    /// Largest rho where the gate passed.
    pub max_score: f64,
}

/// One-shot detector over a whole buffer ([`Detector`] streams it).
pub fn scan(x: &[f64], cfg: &DetectorConfig, stats: Option<&mut ScanStats>) -> Vec<Detection> {
    let mut refs = references(cfg);
    scan_with(x, cfg, &mut refs, stats)
}

fn scan_with(x: &[f64], cfg: &DetectorConfig, refs: &mut References, stats: Option<&mut ScanStats>) -> Vec<Detection> {
    let z = analytic_decimate(x);
    let zz: Vec<f64> = z.iter().map(|v| v.norm_sqr()).collect();
    let floor = (1e-6 * dsp::mean(&zz)).max(1e-30);
    let n_d = N_USEFUL / DECIM;
    let max_lag = cfg.prefixes.iter().map(|&cp| stride(cp) / DECIM).max().unwrap_or(0);
    let span = 2 * max_lag;
    if z.len() <= span + n_d {
        return Vec::new();
    }
    let n_u = z.len() - span - n_d;
    let mut score = vec![0.0; n_u];
    let mut best_cp = vec![0usize; n_u];
    let mut best_ppm = vec![0.0; n_u];
    let mut per_cp: Vec<(usize, Vec<f64>, Vec<f64>)> = Vec::new();
    let mut max_rho = 0.0f64;
    // Cross-correlation with every S1 reference through one FFT of z:
    // c[u] = sum_i z[u + i] conj(ref[i]), circular but unwrapped for u < n_u.
    let (m, fwd, inv, specs) = refs.spectra(z.len()).clone();
    let mut zf = vec![Complex64::new(0.0, 0.0); m];
    zf[..z.len()].copy_from_slice(&z);
    fwd.process(&mut zf);
    let mut corr_buf = vec![Complex64::new(0.0, 0.0); m];
    for &cp in &cfg.prefixes {
        let lag = stride(cp) / DECIM;
        let plateau = cp / DECIM + 1;
        let prod: Vec<Complex64> = (0..z.len() - lag).map(|i| z[i] * z[i + lag].conj()).collect();
        let p = moving_sum(&prod, n_d);
        let r1 = moving_sum(&zz[..zz.len() - lag], n_d);
        let r2 = moving_sum(&zz[lag..], n_d);
        let r: Vec<f64> = r1.iter().zip(&r2).map(|(a, b)| 0.5 * (a + b)).collect();
        let r_sum = moving_sum(&r, plateau);
        let p_sum = moving_sum(&p, plateau);
        let energy = moving_sum(&zz, 2 * lag);
        let m: Vec<f64> =
            (0..n_u).map(|u| p_sum[u].norm() / (r_sum[u].max(0.0) + (plateau * n_d) as f64 * floor)).collect();
        let mut rho = vec![0.0; n_u];
        let mut rho_ppm = vec![0.0; n_u];
        for &ppm in &cfg.ppm_grid {
            let idx = refs.s1.iter().position(|(key, _, _)| *key == (cp, ppm.to_bits())).expect("reference");
            let ref_e = refs.s1[idx].2;
            for ((o, a), b) in corr_buf.iter_mut().zip(&zf).zip(&specs[idx]) {
                *o = a * b;
            }
            inv.process(&mut corr_buf);
            for u in 0..n_u {
                let e = energy[u].max(0.0) + (2 * lag) as f64 * floor;
                let val = corr_buf[u].norm() / (e * ref_e).sqrt();
                if val > rho[u] {
                    rho[u] = val;
                    rho_ppm[u] = ppm;
                }
            }
        }
        for u in 0..n_u {
            max_rho = max_rho.max(rho[u]);
            let gated = if m[u] >= cfg.autocorr_threshold { rho[u] } else { 0.0 };
            if gated > score[u] {
                score[u] = gated;
                best_cp[u] = cp;
                best_ppm[u] = rho_ppm[u];
            }
        }
        per_cp.push((cp, rho, m));
    }
    if let Some(st) = stats {
        st.max_rho = st.max_rho.max(max_rho);
        st.max_score = st.max_score.max(score.iter().copied().fold(0.0, f64::max));
    }

    // +-3 symbol periods: the frame's own S2 pair also repeats.
    let window = 2 * 3 * max_lag + 1;
    let peak = max_filter(&score, window);
    let mut dets = Vec::new();
    let mut last: isize = -(window as isize);
    for u in 0..n_u {
        if !(score[u] >= cfg.xcorr_threshold && score[u] == peak[u]) {
            continue;
        }
        if (u as isize) - last < (window / 2) as isize {
            continue;
        }
        last = u as isize;
        let cp = best_cp[u];
        let (_, rho, m) = per_cp.iter().find(|(c, _, _)| *c == cp).unwrap();
        let mut frac = 0.0;
        if u > 0 && u < n_u - 1 {
            let (y0, y1, y2) = (rho[u - 1], rho[u], rho[u + 1]);
            let den = y0 - 2.0 * y1 + y2;
            if den < 0.0 {
                frac = 0.5 * (y0 - y2) / den;
            }
        }
        let coarse = DECIM as f64 * (u as f64 + frac.clamp(-0.5, 0.5));
        let s2ref = &refs.s2.iter().find(|(c, _)| *c == cp).unwrap().1;
        let Some(start) = fine_timing(x, coarse, cp, cfg.fine_search, s2ref) else {
            continue;
        };
        let q = rho[u];
        let snr = 10.0 * (q * q / (1.0 - q * q).max(1e-6)).log10();
        dets.push(Detection {
            start,
            cp,
            quality: q,
            autocorr: m[u],
            snr_db: snr,
            ppm: best_ppm[u],
            coarse_start: coarse,
        });
    }
    dets
}

fn fine_timing(x: &[f64], coarse: f64, cp: usize, search: usize, rf: &[Complex64]) -> Option<usize> {
    let st = stride(cp) as i64;
    let centre = coarse.round_ties_even() as i64 + 2 * st;
    let lo = centre - search as i64;
    let hi = lo + 2 * search as i64 + rf.len() as i64;
    if lo < 0 || hi > x.len() as i64 {
        return None;
    }
    let lo = lo as usize;
    let env: Vec<f64> = (0..=2 * search)
        .map(|i| {
            let mut acc = Complex64::new(0.0, 0.0);
            for (a, b) in x[lo + i..lo + i + rf.len()].iter().zip(rf) {
                acc += b.conj() * *a;
            }
            acc.norm()
        })
        .collect();
    let mut i = 0;
    for (j, &v) in env.iter().enumerate() {
        if v > env[i] {
            i = j;
        }
    }
    let mut frac = 0.0;
    if i > 0 && i < env.len() - 1 {
        let (y0, y1, y2) = (env[i - 1], env[i], env[i + 1]);
        let den = y0 - 2.0 * y1 + y2;
        if den < 0.0 {
            frac = (0.5 * (y0 - y2) / den).clamp(-0.5, 0.5);
        }
    }
    let s = (lo as f64 + i as f64 + frac).round_ties_even() as i64 - 2 * st;
    usize::try_from(s).ok()
}

/// Streaming detector that rescans a window around every call (4096 samples
/// of history, 8192 of lookahead): the original streaming form, kept as the
/// reference [`detect`] uses. [`Detector`] is the incremental one.
pub struct RescanDetector {
    cfg: DetectorConfig,
    refs: References,
    buf: Vec<f64>,
    buf_start: usize,
    owned: usize,
    /// Largest rho and gated rho seen so far.
    pub stats: ScanStats,
}

const HISTORY: usize = 4096;
const LOOKAHEAD: usize = 8192;

impl RescanDetector {
    /// A detector with `cfg`.
    pub fn new(cfg: DetectorConfig) -> Self {
        let refs = references(&cfg);
        RescanDetector { cfg, refs, buf: Vec::new(), buf_start: 0, owned: 0, stats: ScanStats::default() }
    }

    /// Feed samples; returns detections whose start is now final.
    pub fn process(&mut self, samples: &[f64]) -> Vec<Detection> {
        self.buf.extend_from_slice(samples);
        self.run()
    }

    /// End of input: report what remains.
    pub fn flush(&mut self) -> Vec<Detection> {
        self.buf.extend(std::iter::repeat_n(0.0, LOOKAHEAD));
        let d = self.run();
        self.buf.clear();
        d
    }

    fn run(&mut self) -> Vec<Detection> {
        let end = self.buf_start + self.buf.len();
        let own_hi = end.saturating_sub(LOOKAHEAD);
        if own_hi <= self.owned {
            return Vec::new();
        }
        let found = scan_with(&self.buf, &self.cfg, &mut self.refs, Some(&mut self.stats));
        let mut out = Vec::new();
        for mut d in found {
            let a = d.start + self.buf_start;
            if self.owned <= a && a < own_hi {
                d.start = a;
                d.coarse_start += self.buf_start as f64;
                out.push(d);
            }
        }
        self.owned = own_hi;
        let keep_from = own_hi as i64 - HISTORY as i64 - self.buf_start as i64;
        if keep_from > 0 {
            self.buf.drain(..keep_from as usize);
            self.buf_start += keep_from as usize;
        }
        out
    }
}

/// Detect every preamble in `samples` (real 48 kHz audio), as `preamble.detect`.
pub fn detect(samples: &[f64], cfg: &DetectorConfig) -> Vec<Detection> {
    let mut det = RescanDetector::new(cfg.clone());
    let mut out = Vec::new();
    for chunk in samples.chunks(1 << 21) {
        out.extend(det.process(chunk));
    }
    out.extend(det.flush());
    out
}

/// Front-end taps at 48 kHz: z[m] = sum_n taps[n] x[6 m + c - n], c = (LH - 1) / 2,
/// the same filter [`analytic_decimate`] applies through its FFT.
fn front_end_taps() -> &'static [Complex64] {
    static T: OnceLock<Vec<Complex64>> = OnceLock::new();
    T.get_or_init(|| {
        let h = dsp::firwin_kaiser(LH, HALF_BAND_HZ, KAISER_BETA, FS as f64);
        h.iter()
            .enumerate()
            .map(|(n, &v)| {
                let k = n as f64 - (LH as f64 - 1.0) / 2.0;
                Complex64::from_polar(2.0 * v, 2.0 * std::f64::consts::PI * BAND_CENTER_HZ * k / FS as f64)
            })
            .collect()
    })
}

/// Per-position metrics of the incremental detector, indexed from `start`.
#[derive(Default)]
struct Metrics {
    start: usize,
    score: Vec<f64>,
    best_cp: Vec<usize>,
    best_ppm: Vec<f64>,
    /// Per prefix hypothesis (in `cfg.prefixes` order): rho and M.
    rho: Vec<Vec<f64>>,
    m: Vec<Vec<f64>>,
}

impl Metrics {
    fn end(&self) -> usize {
        self.start + self.score.len()
    }

    fn drain_to(&mut self, from: usize) {
        if from <= self.start {
            return;
        }
        let n = (from - self.start).min(self.score.len());
        self.score.drain(..n);
        self.best_cp.drain(..n);
        self.best_ppm.drain(..n);
        for v in self.rho.iter_mut().chain(self.m.iter_mut()) {
            v.drain(..n);
        }
        self.start += n;
    }
}

/// Incremental streaming detector: the SPEC 6.1 detector of [`scan`]
/// computed position by position as audio arrives. Each call filters and
/// decimates only the new samples (direct FIR, no block FFT), computes the
/// autocorrelation plateau and the S1 cross-correlations only at the new
/// 8 kHz positions, and decides a position once the +-3 symbol peak window
/// around it is complete. A detection is final about 4,100 samples (85 ms)
/// after the frame's first sample whatever the chunk size, and the work per
/// sample is constant (no window is rescanned). It finds the same starts,
/// prefixes and grid points as [`scan`]; rho differs only through the
/// normalisation floor (a running mean instead of the whole buffer's).
pub struct Detector {
    cfg: DetectorConfig,
    refs: References,
    /// 48 kHz input from absolute index `x_start`.
    x: Vec<f64>,
    x_start: usize,
    /// Decimated analytic signal from absolute index `z_start`.
    z: Vec<Complex64>,
    z_start: usize,
    met: Metrics,
    /// Next position to decide.
    decide: usize,
    /// Last detection position (decimated), for the half-window spacing.
    last: Option<usize>,
    flushed: bool,
    /// Early confirmation not yet taken ([`Detector::take_early`]), 48 kHz.
    early: Option<usize>,
    /// Decimated position of the last early confirmation, and whether the
    /// previous position's gated score was at or above the threshold.
    early_last: Option<usize>,
    early_above: bool,
    /// Largest rho and gated rho seen so far.
    pub stats: ScanStats,
}

impl Detector {
    /// A detector with `cfg`.
    pub fn new(cfg: DetectorConfig) -> Self {
        let refs = references(&cfg);
        let n = cfg.prefixes.len();
        let met = Metrics { rho: vec![Vec::new(); n], m: vec![Vec::new(); n], ..Default::default() };
        Detector {
            cfg,
            refs,
            x: Vec::new(),
            x_start: 0,
            z: Vec::new(),
            z_start: 0,
            met,
            decide: 0,
            last: None,
            flushed: false,
            early: None,
            early_last: None,
            early_above: false,
            stats: ScanStats::default(),
        }
    }

    fn max_lag(&self) -> usize {
        self.cfg.prefixes.iter().map(|&cp| stride(cp) / DECIM).max().unwrap_or(0)
    }

    /// Half the peak window (+-3 symbol periods), decimated samples.
    fn half(&self) -> usize {
        3 * self.max_lag()
    }

    /// Feed samples; returns detections whose start is now final.
    pub fn process(&mut self, samples: &[f64]) -> Vec<Detection> {
        self.x.extend_from_slice(samples);
        self.run()
    }

    /// End of input: complete every pending position with silence.
    pub fn flush(&mut self) -> Vec<Detection> {
        let pad = 2 * LH + DECIM * (2 * self.half() + 4 * self.max_lag() + N_USEFUL) + 8 * stride(CP_LONG);
        self.x.extend(std::iter::repeat_n(0.0, pad));
        self.flushed = true;
        let d = self.run();
        self.x.clear();
        self.z.clear();
        d
    }

    /// Early confirmation for carrier detect (SPEC 7.2 preamble correlator):
    /// the absolute 48 kHz index (`DECIM * u`, the coarse start of a frame) of
    /// the first metric position computed since the last call where the gated
    /// score rose to the threshold (plateau M >= the autocorrelation threshold
    /// and S1 cross-correlation rho >= the cross-correlation threshold), taken
    /// once. It is available as soon as that position's metrics are, about
    /// 2,300 samples (47 ms) after the frame's first sample instead of the
    /// final [`Detection`]'s 85 ms. A crossing within six symbol periods of
    /// the previous one is not reported again. Detections are unaffected.
    pub fn take_early(&mut self) -> Option<usize> {
        self.early.take()
    }

    /// Absolute 48 kHz index one past the last sample fed.
    pub fn position(&self) -> usize {
        self.x_start + self.x.len()
    }

    /// Samples after a frame's first sample before its detection is final
    /// (the decision window and the S2 fine timing), for the slowest prefix.
    pub fn latency(&self) -> usize {
        let c = (LH - 1) / 2;
        let z_after = self.half() + 2 * self.max_lag() + N_USEFUL / DECIM + 1;
        (DECIM * z_after + c).max(4 * stride(CP_LONG) + 2 * self.cfg.fine_search + DECIM + 2)
    }

    fn run(&mut self) -> Vec<Detection> {
        self.filter();
        self.metrics();
        let out = self.peaks();
        self.trim();
        out
    }

    /// Decimated outputs whose inputs have all arrived.
    fn filter(&mut self) {
        let taps = front_end_taps();
        let c = (LH - 1) / 2;
        let x_end = self.x_start + self.x.len();
        loop {
            let m = self.z_start + self.z.len();
            let newest = DECIM * m + c;
            if newest >= x_end {
                break;
            }
            // x[newest - n] for n in 0..LH; zero before the stream start.
            let oldest = newest.saturating_sub(LH - 1).max(self.x_start);
            let mut acc = Complex64::new(0.0, 0.0);
            for i in oldest..=newest {
                acc += taps[newest - i] * self.x[i - self.x_start];
            }
            self.z.push(acc);
        }
    }

    /// Metrics for every new position with enough of z after it.
    fn metrics(&mut self) {
        let n_d = N_USEFUL / DECIM;
        let span = 2 * self.max_lag();
        let z_end = self.z_start + self.z.len();
        let first_u = self.met.end().max(self.z_start);
        if first_u + span + n_d > z_end {
            return;
        }
        let n_new = z_end - span - n_d + 1 - first_u;
        // Local window of z from the first new position, with prefix sums.
        let zl = &self.z[first_u - self.z_start..];
        let mut czz = Vec::with_capacity(zl.len() + 1);
        czz.push(0.0);
        let mut acc = 0.0;
        for v in zl {
            acc += v.norm_sqr();
            czz.push(acc);
        }
        let sum = |a: usize, b: usize| czz[b] - czz[a];
        // Normalisation floor: 1e-6 of the mean power of the retained z
        // (the one-shot scan takes its whole buffer).
        let all_mean = self.z.iter().map(|v| v.norm_sqr()).sum::<f64>() / self.z.len() as f64;
        let floor = (1e-6 * all_mean).max(1e-30);
        let n_cp = self.cfg.prefixes.len();
        let mut score = vec![0.0; n_new];
        let mut best_cp = vec![0usize; n_new];
        let mut best_ppm = vec![0.0; n_new];
        let mut rho_all = vec![vec![0.0; n_new]; n_cp];
        let mut m_all = vec![vec![0.0; n_new]; n_cp];
        let mut max_rho = 0.0f64;
        for (ci, &cp) in self.cfg.prefixes.iter().enumerate() {
            let lag = stride(cp) / DECIM;
            let plateau = cp / DECIM + 1;
            let n_prod = zl.len() - lag;
            let mut cprod = Vec::with_capacity(n_prod + 1);
            cprod.push(Complex64::new(0.0, 0.0));
            let mut a = Complex64::new(0.0, 0.0);
            for i in 0..n_prod {
                a += zl[i] * zl[i + lag].conj();
                cprod.push(a);
            }
            let refs: Vec<(usize, f64)> = self
                .cfg
                .ppm_grid
                .iter()
                .map(|&ppm| {
                    let idx =
                        self.refs.s1.iter().position(|(key, _, _)| *key == (cp, ppm.to_bits())).expect("reference");
                    (idx, ppm)
                })
                .collect();
            for u in 0..n_new {
                let mut p_sum = Complex64::new(0.0, 0.0);
                let mut r_sum = 0.0;
                for t in u..u + plateau {
                    p_sum += cprod[t + n_d] - cprod[t];
                    r_sum += 0.5 * (sum(t, t + n_d) + sum(t + lag, t + lag + n_d));
                }
                let m = p_sum.norm() / (r_sum.max(0.0) + (plateau * n_d) as f64 * floor);
                let e = sum(u, u + 2 * lag).max(0.0) + (2 * lag) as f64 * floor;
                let (mut rho, mut rho_ppm) = (0.0f64, 0.0);
                for &(idx, ppm) in &refs {
                    let (_, r, ref_e) = &self.refs.s1[idx];
                    let mut c = Complex64::new(0.0, 0.0);
                    for (zv, rv) in zl[u..u + r.len()].iter().zip(r) {
                        c += zv * rv.conj();
                    }
                    let val = c.norm() / (e * ref_e).sqrt();
                    if val > rho {
                        rho = val;
                        rho_ppm = ppm;
                    }
                }
                max_rho = max_rho.max(rho);
                rho_all[ci][u] = rho;
                m_all[ci][u] = m;
                let gated = if m >= self.cfg.autocorr_threshold { rho } else { 0.0 };
                if gated > score[u] {
                    score[u] = gated;
                    best_cp[u] = cp;
                    best_ppm[u] = rho_ppm;
                }
            }
        }
        self.stats.max_rho = self.stats.max_rho.max(max_rho);
        self.stats.max_score = self.stats.max_score.max(score.iter().copied().fold(0.0, f64::max));
        let holdoff = 2 * self.half();
        for (j, &s) in score.iter().enumerate() {
            let u = first_u + j;
            let above = s >= self.cfg.xcorr_threshold;
            if above && !self.early_above && self.early.is_none() && self.early_last.is_none_or(|l| u - l >= holdoff) {
                self.early = Some(DECIM * u);
                self.early_last = Some(u);
            }
            self.early_above = above;
        }
        if self.met.score.is_empty() {
            self.met.start = first_u;
        }
        self.met.score.extend(score);
        self.met.best_cp.extend(best_cp);
        self.met.best_ppm.extend(best_ppm);
        for ci in 0..n_cp {
            self.met.rho[ci].extend(&rho_all[ci]);
            self.met.m[ci].extend(&m_all[ci]);
        }
    }

    /// Decide every position whose peak window is complete.
    fn peaks(&mut self) -> Vec<Detection> {
        let half = self.half();
        let mut out = Vec::new();
        let x_end = self.x_start + self.x.len();
        while self.decide + half < self.met.end() {
            let u = self.decide;
            if u < self.met.start {
                self.decide = self.met.start;
                continue;
            }
            let i = u - self.met.start;
            let s = self.met.score[i];
            // The one-shot scan's rule: at least the threshold, the maximum of
            // its +-3 symbol window, and half a window after the last detection.
            let candidate = s >= self.cfg.xcorr_threshold
                && self.met.score[u.saturating_sub(half).max(self.met.start) - self.met.start..=i + half]
                    .iter()
                    .all(|&v| v <= s)
                && self.last.is_none_or(|l| u - l >= half);
            if candidate {
                let cp = self.met.best_cp[i];
                let ci = self.cfg.prefixes.iter().position(|&c| c == cp).expect("prefix");
                // Fine timing reads the 48 kHz input to the end of the S2 pair.
                let need = DECIM * (u + 1) + 4 * stride(cp) + 2 * self.cfg.fine_search + 2;
                if need > x_end && !self.flushed {
                    break;
                }
                self.last = Some(u);
                let rho = &self.met.rho[ci];
                let mut frac = 0.0;
                if i > 0 && i + 1 < rho.len() {
                    let (y0, y1, y2) = (rho[i - 1], rho[i], rho[i + 1]);
                    let den = y0 - 2.0 * y1 + y2;
                    if den < 0.0 {
                        frac = 0.5 * (y0 - y2) / den;
                    }
                }
                let coarse = DECIM as f64 * (u as f64 + frac.clamp(-0.5, 0.5));
                let s2ref = &self.refs.s2.iter().find(|(c, _)| *c == cp).unwrap().1;
                let local = coarse - self.x_start as f64;
                if let Some(st) = fine_timing(&self.x, local, cp, self.cfg.fine_search, s2ref) {
                    let q = rho[i];
                    out.push(Detection {
                        start: st + self.x_start,
                        cp,
                        quality: q,
                        autocorr: self.met.m[ci][i],
                        snr_db: 10.0 * (q * q / (1.0 - q * q).max(1e-6)).log10(),
                        ppm: self.met.best_ppm[i],
                        coarse_start: coarse,
                    });
                }
            }
            self.decide += 1;
        }
        out
    }

    fn trim(&mut self) {
        let half = self.half();
        self.met.drain_to(self.decide.saturating_sub(half + 2));
        // z: a second of history (the floor's mean) before the first position
        // still waiting for metrics.
        let keep_z = self.met.end().min(self.z_start + self.z.len()).saturating_sub(8000);
        if keep_z > self.z_start + 4000 {
            self.z.drain(..keep_z - self.z_start);
            self.z_start = keep_z;
        }
        // x: the FIR's history and the fine timing of undecided positions.
        let next_m = self.z_start + self.z.len();
        let fir_from = (DECIM * next_m + (LH - 1) / 2).saturating_sub(LH);
        let fine_from = (DECIM * self.decide.saturating_sub(1)).saturating_sub(self.cfg.fine_search + 8);
        let keep_x = fir_from.min(fine_from);
        if keep_x > self.x_start + 48_000 {
            self.x.drain(..keep_x - self.x_start);
            self.x_start = keep_x;
        }
    }
}
