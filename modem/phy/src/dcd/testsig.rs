//! Synthetic test signals for carrier-detect tests and the two-station
//! simulator: open-squelch receiver noise, AX.25/HDLC framing with Bell 202
//! AFSK, a speech-like signal and the simulator's interferers (courtesy beep,
//! 1750 Hz tone, DTMF, CW). These are test fixtures, not modem functions; all
//! at 48 kHz.

use std::f64::consts::PI;

use num_complex::Complex64;

use super::BandPass;
use crate::channel::Rng;
use crate::params::FS;

const FS_F: f64 = FS as f64;

/// Spectral shape of [`OpenSquelchNoise`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseKind {
    /// White Gaussian noise through the band-pass: the discriminator noise of
    /// a de-emphasized port.
    White,
    /// The same after a first difference (|H|^2 = 4 sin^2(pi f / fs) ~ f^2):
    /// the discriminator noise of a flat (unde-emphasized) port.
    Tilted,
}

/// Open-squelch FM receiver noise as the Stage 1 simulator's `NoiseSource`
/// (`sim/cairn_sim/preamble_eval.py`): Gaussian noise ([`NoiseKind`])
/// through an 8th-order Butterworth band-pass (four biquads), generated in
/// chunks with the filter state carried over, at unit mean power (scale it
/// to the level wanted).
#[derive(Clone, Debug)]
pub struct OpenSquelchNoise {
    kind: NoiseKind,
    bp: BandPass,
    rng: Rng,
    prev: f64,
    gain: f64,
}

impl OpenSquelchNoise {
    /// Receive band of a typical FM radio (the simulator's `RX_BAND_HZ`). The
    /// 2800-3200 Hz quieting band then sits on the filter's upper skirt.
    pub const RX_BAND_HZ: (f64, f64) = (300.0, 3000.0);
    /// A wider receive band, with the quieting band well inside it.
    pub const WIDE_BAND_HZ: (f64, f64) = (200.0, 4000.0);

    /// Noise over [`Self::RX_BAND_HZ`].
    pub fn new(kind: NoiseKind, rng: Rng) -> Self {
        Self::with_band(kind, Self::RX_BAND_HZ, rng)
    }

    /// Noise over [`Self::WIDE_BAND_HZ`].
    pub fn wide(kind: NoiseKind, rng: Rng) -> Self {
        Self::with_band(kind, Self::WIDE_BAND_HZ, rng)
    }

    /// Noise over `band` (Hz).
    pub fn with_band(kind: NoiseKind, band: (f64, f64), rng: Rng) -> Self {
        let bp = BandPass::butterworth(4, band.0, band.1, FS_F);
        // Output power for unit-variance input: (1/pi) int_0^pi |H|^2 S(w) dw.
        let steps = 48_000;
        let mut p = 0.0;
        for i in 0..steps {
            let f = (i as f64 + 0.5) * (FS_F / 2.0) / steps as f64;
            let shape = match kind {
                NoiseKind::White => 1.0,
                NoiseKind::Tilted => 4.0 * (PI * f / FS_F).sin().powi(2),
            };
            p += bp.power_gain(f, FS_F) * shape;
        }
        p /= steps as f64;
        OpenSquelchNoise { kind, bp, rng, prev: 0.0, gain: 1.0 / p.sqrt() }
    }

    /// The next sample.
    pub fn sample(&mut self) -> f64 {
        let w = self.rng.normal();
        let v = match self.kind {
            NoiseKind::White => w,
            NoiseKind::Tilted => {
                let d = w - self.prev;
                self.prev = w;
                d
            }
        };
        self.bp.process(v) * self.gain
    }

    /// Fill `out` with the next samples.
    pub fn fill(&mut self, out: &mut [f64]) {
        for v in out {
            *v = self.sample();
        }
    }

    /// The next `n` samples.
    pub fn generate(&mut self, n: usize) -> Vec<f64> {
        (0..n).map(|_| self.sample()).collect()
    }
}

/// CRC-16/X.25 (the AX.25 FCS): reflected polynomial 0x1021, init 0xFFFF,
/// final complement.
pub fn crc16_x25(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    !crc
}

/// AX.25/HDLC bit stream (before NRZI): `flags_before` flags (0x7E), the
/// payload and its FCS (CRC-16/X.25, low byte first), every byte LSB first,
/// with a 0 stuffed after five consecutive 1s, then `flags_after` flags.
pub fn hdlc_bits(payload: &[u8], flags_before: usize, flags_after: usize) -> Vec<u8> {
    const FLAG: [u8; 8] = [0, 1, 1, 1, 1, 1, 1, 0];
    let fcs = crc16_x25(payload);
    let mut bits = Vec::with_capacity(8 * (payload.len() + 2 + flags_before + flags_after) * 6 / 5 + 8);
    for _ in 0..flags_before {
        bits.extend_from_slice(&FLAG);
    }
    let mut ones = 0;
    for &byte in payload.iter().chain(&fcs.to_le_bytes()) {
        for i in 0..8 {
            let b = (byte >> i) & 1;
            bits.push(b);
            if b == 1 {
                ones += 1;
                if ones == 5 {
                    bits.push(0);
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
    }
    for _ in 0..flags_after {
        bits.extend_from_slice(&FLAG);
    }
    bits
}

/// Samples per Bell 202 bit at 48 kHz (1200 baud).
pub const BELL202_SAMPLES_PER_BIT: usize = 40;
/// Bell 202 mark tone (Hz).
pub const BELL202_MARK_HZ: f64 = 1200.0;
/// Bell 202 space tone (Hz).
pub const BELL202_SPACE_HZ: f64 = 2200.0;

/// Bell 202 AFSK of `bits` as AX.25 sends it: NRZI (a 0 changes the tone, a 1
/// keeps it), starting on mark, continuous-phase FSK with mark 1200 Hz and
/// space 2200 Hz at 1200 baud (40 samples per bit). The mark tone has
/// `amplitude`; the space tone `amplitude * 10^(twist_db / 20)` (positive
/// twist: space louder, as pre-emphasis without de-emphasis does).
pub fn bell202(bits: &[u8], amplitude: f64, twist_db: f64) -> Vec<f64> {
    let space_amp = amplitude * 10f64.powf(twist_db / 20.0);
    let mut mark = true;
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(bits.len() * BELL202_SAMPLES_PER_BIT);
    for &b in bits {
        if b == 0 {
            mark = !mark;
        }
        let (f, a) = if mark { (BELL202_MARK_HZ, amplitude) } else { (BELL202_SPACE_HZ, space_amp) };
        let dphi = 2.0 * PI * f / FS_F;
        for _ in 0..BELL202_SAMPLES_PER_BIT {
            phase += dphi;
            if phase > 2.0 * PI {
                phase -= 2.0 * PI;
            }
            out.push(a * phase.sin());
        }
    }
    out
}

/// A two-pole resonator (formant) with unity gain at its centre.
#[derive(Clone, Copy, Debug, Default)]
struct Resonator {
    y1: f64,
    y2: f64,
}

impl Resonator {
    fn step(&mut self, x: f64, f: f64, bw: f64) -> f64 {
        let r = (-PI * bw / FS_F).exp();
        let w = 2.0 * PI * f / FS_F;
        let c = 2.0 * r * w.cos();
        let e1 = Complex64::from_polar(1.0, -w);
        let g = (1.0 - c * e1 + r * r * e1 * e1).norm();
        let y = g * x + c * self.y1 - r * r * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// A speech-like test signal of `n` samples: a glottal pulse train whose
/// fundamental wanders over 90-220 Hz (with cycle-to-cycle jitter), through
/// three moving formant resonators (F1 300-850 Hz, F2 850-2400 Hz, F3
/// 2300-3100 Hz, a new target every 60-180 ms), plus breath noise, with a
/// syllabic envelope at 3-6 Hz and pauses of 0.15-0.6 s, band-limited to
/// 300-3000 Hz and scaled to unit RMS over the whole signal. Not speech, but
/// it has speech's pitch structure, formant ringing and pauses.
pub fn voice_like(n: usize, rng: &mut Rng) -> Vec<f64> {
    let mut out = Vec::with_capacity(n);
    let mut f0_log = (140.0f64).ln();
    let mut f0_target = (140.0f64).ln();
    let mut phase = 0.0f64;
    let mut period_jitter = 1.0;
    let mut glottal = 0.0f64; // one-pole low-passed pulse train
    let mut formants = [500.0f64, 1500.0, 2600.0];
    let mut targets = formants;
    let bands = [80.0, 120.0, 180.0];
    let mut res = [Resonator::default(); 3];
    let mut breath_res = [Resonator::default(); 2];
    let mut next_phone = 0usize;
    let mut next_f0 = 0usize;
    // Syllable state.
    let mut syl_left = 0usize;
    let mut syl_len = 1usize;
    let mut pause = false;
    let mut prev = 0.0;
    let mut bp = BandPass::butterworth(3, 300.0, 3000.0, FS_F);
    let a_form = 1.0 - (-1.0 / (0.02 * FS_F)).exp();
    let a_f0 = 1.0 - (-1.0 / (0.08 * FS_F)).exp();
    let a_glot = (-2.0 * PI * 300.0 / FS_F).exp();
    for i in 0..n {
        if i >= next_phone {
            next_phone = i + (0.06 * FS_F + rng.uniform() * 0.12 * FS_F) as usize;
            targets = [300.0 + 550.0 * rng.uniform(), 850.0 + 1550.0 * rng.uniform(), 2300.0 + 800.0 * rng.uniform()];
        }
        if i >= next_f0 {
            next_f0 = i + (0.15 * FS_F + rng.uniform() * 0.35 * FS_F) as usize;
            f0_target = (90.0 + 130.0 * rng.uniform()).ln();
        }
        if syl_left == 0 {
            pause = !pause && rng.uniform() < 0.2;
            syl_len = if pause {
                ((0.15 + 0.45 * rng.uniform()) * FS_F) as usize
            } else {
                (FS_F / (3.0 + 3.0 * rng.uniform())) as usize
            };
            syl_left = syl_len;
        }
        let env = if pause {
            0.0
        } else {
            let t = 1.0 - syl_left as f64 / syl_len as f64;
            (PI * t).sin().powf(1.5)
        };
        syl_left -= 1;
        for k in 0..3 {
            formants[k] += a_form * (targets[k] - formants[k]);
        }
        f0_log += a_f0 * (f0_target - f0_log) + 0.0004 * rng.normal();
        let f0 = f0_log.exp() * period_jitter;
        phase += f0 / FS_F;
        let mut pulse = 0.0;
        if phase >= 1.0 {
            phase -= 1.0;
            pulse = 1.0;
            period_jitter = 1.0 + 0.01 * rng.normal();
        }
        glottal = a_glot * glottal + pulse;
        let mut v = glottal;
        for k in 0..3 {
            v = res[k].step(v, formants[k], bands[k]);
        }
        let mut b = 0.03 * rng.normal();
        b = breath_res[0].step(b, formants[1], 300.0);
        b = breath_res[1].step(b, formants[2], 400.0);
        // Lip radiation (first difference), envelope, band limit.
        let s = v - prev;
        prev = v;
        out.push(bp.process(env * (s + b)));
    }
    let rms = (out.iter().map(|v| v * v).sum::<f64>() / n.max(1) as f64).sqrt();
    if rms > 0.0 {
        for v in &mut out {
            *v /= rms;
        }
    }
    out
}

/// A sine tone of `n` samples at `freq_hz`, peak `amplitude`, phase 0 at sample 0.
pub fn tone(freq_hz: f64, amplitude: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| amplitude * (2.0 * PI * freq_hz * i as f64 / FS_F).sin()).collect()
}

/// The Stage 1 simulator's interferers (`preamble_eval.py`), periodic, at
/// unit RMS while on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interferer {
    /// Repeater courtesy beep: 1000 Hz, 250 ms on, 750 ms off, 5 ms ramps.
    CourtesyBeep,
    /// 1750 Hz tone burst, continuous.
    Tone1750,
    /// The 16 DTMF digits, 100 ms on, 100 ms off, 2 ms ramps.
    Dtmf,
    /// "DE CAIRN" at 20 wpm, 800 Hz, hard keying.
    Cw,
}

impl Interferer {
    /// Every interferer.
    pub const ALL: [Interferer; 4] = [Interferer::CourtesyBeep, Interferer::Tone1750, Interferer::Dtmf, Interferer::Cw];
}

/// Raised-cosine on/off envelope of `on` with `ramp` samples per edge.
fn ramp_envelope(on: &[bool], ramp: usize) -> Vec<f64> {
    let mut env = vec![0.0; on.len()];
    let mut level = 0.0f64;
    let step = 1.0 / ramp.max(1) as f64;
    for (i, &o) in on.iter().enumerate() {
        level = if o { (level + step).min(1.0) } else { (level - step).max(0.0) };
        env[i] = 0.5 - 0.5 * (PI * level).cos();
    }
    env
}

fn interferer_period(kind: Interferer) -> Vec<f64> {
    let fs = FS as usize;
    let s2 = std::f64::consts::SQRT_2;
    match kind {
        Interferer::CourtesyBeep => {
            let on: Vec<bool> = (0..fs).map(|i| i < fs / 4).collect();
            let env = ramp_envelope(&on, (0.005 * FS_F) as usize);
            (0..fs).map(|i| s2 * (2.0 * PI * 1000.0 * i as f64 / FS_F).sin() * env[i]).collect()
        }
        Interferer::Tone1750 => tone(1750.0, s2, fs),
        Interferer::Dtmf => {
            const ROWS: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
            const COLS: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
            let seg = fs / 10;
            let mut x = Vec::with_capacity(32 * seg);
            let mut on = Vec::with_capacity(32 * seg);
            for r in ROWS {
                for c in COLS {
                    for i in 0..seg {
                        let t = i as f64 / FS_F;
                        x.push((2.0 * PI * r * t).sin() + (2.0 * PI * c * t).sin());
                        on.push(true);
                    }
                    x.extend(std::iter::repeat_n(0.0, seg));
                    on.extend(std::iter::repeat_n(false, seg));
                }
            }
            let env = ramp_envelope(&on, (0.002 * FS_F) as usize);
            x.iter().zip(&env).map(|(v, e)| v * e).collect()
        }
        Interferer::Cw => {
            let morse = |c: char| match c {
                'D' => "-..",
                'E' => ".",
                'C' => "-.-.",
                'A' => ".-",
                'I' => "..",
                'R' => ".-.",
                'N' => "-.",
                _ => "",
            };
            let dot = (1.2 / 20.0 * FS_F) as usize;
            let mut keys = Vec::new();
            for word in ["DE", "CAIRN"] {
                for ch in word.chars() {
                    for el in morse(ch).chars() {
                        keys.extend(std::iter::repeat_n(true, if el == '.' { 1 } else { 3 }));
                        keys.push(false);
                    }
                    keys.extend([false, false]);
                }
                keys.extend([false; 4]);
            }
            let mut x = Vec::with_capacity(keys.len() * dot);
            for (k, &on) in keys.iter().enumerate() {
                for j in 0..dot {
                    let i = k * dot + j;
                    x.push(if on { s2 * (2.0 * PI * 800.0 * i as f64 / FS_F).sin() } else { 0.0 });
                }
            }
            x
        }
    }
}

/// `n` samples of interferer `kind` from sample `start` of its period.
pub fn interferer(kind: Interferer, start: usize, n: usize) -> Vec<f64> {
    let p = interferer_period(kind);
    (0..n).map(|i| p[(start + i) % p.len()]).collect()
}
