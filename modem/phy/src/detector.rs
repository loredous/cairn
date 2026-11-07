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

/// Streaming detector: feed audio with [`Detector::process`]; detections
/// carry absolute 48 kHz sample indices across calls.
pub struct Detector {
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

impl Detector {
    /// A detector with `cfg`.
    pub fn new(cfg: DetectorConfig) -> Self {
        let refs = references(&cfg);
        Detector { cfg, refs, buf: Vec::new(), buf_start: 0, owned: 0, stats: ScanStats::default() }
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
    let mut det = Detector::new(cfg.clone());
    let mut out = Vec::new();
    for chunk in samples.chunks(1 << 21) {
        out.extend(det.process(chunk));
    }
    out.extend(det.flush());
    out
}
