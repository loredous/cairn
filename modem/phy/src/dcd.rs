//! Carrier detect (SPEC 7.2): the OR of four detectors.
//!
//! - **Quieting.** The radio runs with squelch open, so its discriminator
//!   noise fills the band above the highest active tone; any FM carrier
//!   quiets it. The input goes through an 8th-order Butterworth band-pass on
//!   [`DcdConfig::quieting_band_hz`] (four biquads) and the mean square is
//!   taken per 10 ms hop. An open-squelch floor is learnt while the detector
//!   is clear and not muted, from the 200 ms mean: towards a higher level
//!   with a 0.4 s time constant, towards a lower one with 2 s; it is frozen
//!   while asserted. The detector asserts when the 50 ms mean is
//!   [`DcdConfig::assert_drop_db`] (6 dB) or more below the floor and the
//!   20 ms mean is below the release level. It releases when the 20 ms mean
//!   rises above the release level: [`DcdConfig::release_drop_db`] (3 dB)
//!   below the floor, or halfway in dB between the floor and the level seen
//!   while asserted if that is lower (a 20 dB carrier releases 10 dB below
//!   the floor, at once when the noise returns, while a shallow one does not
//!   chatter). Asserted continuously for [`DcdConfig::relearn_s`] it
//!   re-learns its floor and releases. It is unarmed (never asserts) until
//!   0.55 s of audio has been seen and while the floor is below
//!   [`DcdConfig::min_floor_dbfs`] (squelch closed, or no radio). Levels are
//!   band power in dB relative to a mean square of 1.0 (a full-scale DC
//!   level; a full-scale sine is -3 dB).
//!
//!   Hysteresis (from the first on-air session, 2026-11-07, where quieting
//!   rose 72 times in 3.5 minutes): after a release the detector decides on
//!   hops after it only, at least [`DcdConfig::min_off_s`] (50 ms), so the
//!   assert window never straddles the carrier that just ended, and for
//!   [`DcdConfig::reassert_window_s`] (250 ms) it needs a drop
//!   [`DcdConfig::reassert_extra_db`] (3 dB) deeper (the noise comes back a
//!   few dB low for about 100 ms after a carrier); once up it stays up at
//!   least [`DcdConfig::min_on_s`] (30 ms). Release latency is unchanged.
//!   After own PTT, the radio's own transmit tail reads as silence (the
//!   AIOC radios kept their carrier 0.64-0.73 s after release): quieting
//!   neither learns nor asserts until the open-squelch noise is back (one
//!   hop within the assert margin of the floor), at most
//!   [`DcdConfig::recovery_max_s`] (1.5 s); [`Dcd::take_recovery`] reports
//!   how long that took. [`DcdConfig::legacy`] restores the first version.
//! - **Preamble correlator.** Driven by the caller ([`Dcd::set_preamble`]),
//!   who runs the streaming preamble detector (see
//!   [`crate::detector::Detector::take_early`]) and knows frame lengths.
//! - **AFSK (Bell 202).** A sliding one-bit (40-sample) DFT at 1200 and
//!   2200 Hz. Per sample: tone dominance D = (|X1200|^2 + |X2200|^2) / (N E /
//!   2) (1 for a pure tone on a bin; E the window energy) and the log ratio r
//!   = ln(|X1200|^2 / |X2200|^2). Tone transitions are the crossings of r
//!   through +-1 (hysteresis), timed by interpolation; a transition counts
//!   only if the tone runs on both sides of it reach a normalised power of
//!   0.4 on their own bins (both tones really appear: a beat of two
//!   interferers in one bin, or a noise dip inside a run, does not count).
//!   Per hop, over the last 60 ms, it asserts on: mean D >= 0.8; at least 4
//!   rising and 4 falling transitions; their timing coherent on the
//!   40-sample bit grid, C = (|sum e^{j 2 pi t / 40}| over rising + the same
//!   over falling) / count >= 0.6 (rising and falling are summed apart
//!   because twist shifts them in opposite directions); no gap between
//!   transitions longer than 9 bits (HDLC NRZI changes tone at least every 7
//!   bits; this rejects pitch-synchronous voice below 133 Hz and keyed
//!   tones); and tone purity >= 0.8 (inside runs, the phase of the run's bin
//!   is stable over a bit, so the tones sit within about 120 Hz of 1200 and
//!   2200 Hz: rejects voice formants, whose pitch-synchronous transitions can
//!   be coherent on the grid). It holds while the last 30 ms keep mean D >=
//!   0.7, C >= 0.45 and a transition of each kind.
//! - **COS.** Optional radio squelch line ([`Dcd::set_cos`]).
//!
//! [`Dcd::process`] evaluates on fixed 10 ms (480-sample) hops counted from
//! the first sample, so its edges do not depend on how the input is
//! chunked. [`HeldDcd`] applies the SPEC's hold for display; the link layer
//! applies its own to the raw edges.
//!
//! False alarms (open-squelch noise, no carrier). The 50 ms band power of
//! Gaussian noise in a 400 Hz band is a chi-square variable with about 2 B T
//! = 40 degrees of freedom over its mean, and the tracker leaves the floor
//! within about 0.3 dB of the mean. A 6 dB drop is P(chi2_40 / 40 < 0.25) =
//! 5e-7 per hop: 0.03 per 10 minutes even counting all 60,000 hops as
//! independent (they overlap, so fewer). With a 5 dB threshold it would be
//! 1.5 per 10 minutes, too many. A downward step of the noise level eats
//! into the margin until the floor follows: a 2 dB step is absorbed, a 3 dB
//! one risks a false assert in the second or so the floor takes; slow drifts
//! are tracked. On noise whose quieting band sits on a radio's 300-3000 Hz
//! skirt the effective bandwidth, and so the margin, is somewhat smaller.
//! The measured counts are in `tests/dcd.rs`.
//!
//! Limits. Quieting sees whatever the carrier carries in its band: a Cairn
//! frame's own spectrum in 2700-3100 Hz is only about 19 dB below its mean
//! power, so on its own frames quieting asserts only where the open-squelch
//! band floor is well above that (the preamble correlator covers them), and
//! bright speech releases it briefly. Release is by the band power coming
//! back, so a squelch that closes (silence) reads as a carrier until
//! [`DcdConfig::relearn_s`]; quieting needs squelch open, as SPEC 7.2 says.

use std::collections::VecDeque;
use std::f64::consts::PI;

use num_complex::Complex64;

use crate::params::{BASE_TONES, FS, band_high_hz};

pub mod testsig;

/// Evaluation hop, samples (10 ms).
pub const HOP: usize = 480;

// ---------------------------------------------------------------------------
// Band-pass filter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Biquad {
    g: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// g (1 - z^-2) / (1 + a1 z^-1 + a2 z^-2), transposed direct form II.
    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.g * x + self.z1;
        self.z1 = -self.a1 * y + self.z2;
        self.z2 = -self.g * x - self.a2 * y;
        y
    }

    fn response(&self, w: f64) -> Complex64 {
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        self.g * (1.0 - z2) / (1.0 + self.a1 * z1 + self.a2 * z2)
    }
}

/// Butterworth band-pass as cascaded biquads (bilinear transform with
/// pre-warped edges), unity gain at the geometric centre.
#[derive(Clone, Debug)]
pub struct BandPass {
    sections: Vec<Biquad>,
}

impl BandPass {
    /// Band-pass from the order-`order` analog low-pass prototype (`order`
    /// biquads, filter order 2 `order`), -3 dB at `lo_hz` and `hi_hz`.
    pub fn butterworth(order: usize, lo_hz: f64, hi_hz: f64, fs: f64) -> Self {
        assert!(order >= 1 && lo_hz > 0.0 && lo_hz < hi_hz && hi_hz < fs / 2.0, "band-pass edges");
        let w1 = 2.0 * fs * (PI * lo_hz / fs).tan();
        let w2 = 2.0 * fs * (PI * hi_hz / fs).tan();
        let w0 = (w1 * w2).sqrt();
        let bw = w2 - w1;
        let mut poles = Vec::with_capacity(2 * order);
        for k in 0..order {
            let p = Complex64::from_polar(1.0, PI * (2 * k + order + 1) as f64 / (2 * order) as f64);
            let a = p * (bw / 2.0);
            let d = (a * a - w0 * w0).sqrt();
            for s in [a + d, a - d] {
                poles.push((2.0 * fs + s) / (2.0 * fs - s));
            }
        }
        let wc = 2.0 * (w0 / (2.0 * fs)).atan();
        let mut sections = Vec::with_capacity(order);
        let mut real = Vec::new();
        for z in &poles {
            if z.im > 1e-9 {
                sections.push(Biquad { g: 1.0, a1: -2.0 * z.re, a2: z.norm_sqr(), z1: 0.0, z2: 0.0 });
            } else if z.im.abs() <= 1e-9 {
                real.push(z.re);
            }
        }
        for pair in real.chunks(2) {
            let (p1, p2) = (pair[0], *pair.get(1).unwrap_or(&0.0));
            sections.push(Biquad { g: 1.0, a1: -(p1 + p2), a2: p1 * p2, z1: 0.0, z2: 0.0 });
        }
        assert_eq!(sections.len(), order, "pole pairing");
        for s in &mut sections {
            s.g = 1.0 / s.response(wc).norm();
        }
        BandPass { sections }
    }

    /// Filter one sample.
    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        self.sections.iter_mut().fold(x, |v, s| s.process(v))
    }

    /// |H(f)|^2.
    pub fn power_gain(&self, f_hz: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * f_hz / fs;
        self.sections.iter().map(|s| s.response(w).norm_sqr()).product()
    }

    /// Clear the filter state.
    pub fn reset(&mut self) {
        for s in &mut self.sections {
            s.z1 = 0.0;
            s.z2 = 0.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Carrier-detect settings.
#[derive(Clone, Debug, PartialEq)]
pub struct DcdConfig {
    /// Quieting measurement band (Hz), just above the highest active tone.
    pub quieting_band_hz: (f64, f64),
    /// Quieting asserts when the 50 ms band power is this far below the
    /// tracked open-squelch floor (dB).
    pub assert_drop_db: f64,
    /// It releases when the 20 ms band power is back within this of the
    /// floor (dB).
    pub release_drop_db: f64,
    /// A floor below this (band power, dB re a mean square of 1.0) means no
    /// open-squelch noise: quieting stays unarmed. Re-armed 3 dB above.
    pub min_floor_dbfs: f64,
    /// Run the Bell 202 detector.
    pub afsk: bool,
    /// After unmuting, audio ignored by quieting before it learns or decides
    /// (receiver recovery after own PTT), seconds.
    pub unmute_guard_s: f64,
    /// Quieting asserted continuously this long re-learns its floor from the
    /// current level and releases (a stuck state recovers), seconds.
    pub relearn_s: f64,
    /// Consecutive hops whose 20 ms mean is above the release level before
    /// quieting releases (1: the first; 2 rides out a 10 ms burst of a
    /// frame's own spectrum or of speech in the band).
    pub release_confirm_hops: usize,
    /// After a release (or the end of a recovery), quieting decides on
    /// hops after it only, and at least this long (seconds): the assert
    /// window never straddles the carrier that just ended, so the returning
    /// noise cannot re-assert it. 0: the window may straddle (legacy).
    pub min_off_s: f64,
    /// Once asserted, quieting stays up at least this long (seconds).
    pub min_on_s: f64,
    /// For this long after a release (seconds) quieting asserts only on a
    /// drop [`DcdConfig::reassert_extra_db`] deeper than usual: the noise
    /// comes back a few dB low after a carrier ends (the receiver's AGC and
    /// de-emphasis settling), and a real new carrier is far deeper.
    pub reassert_window_s: f64,
    /// The extra drop needed inside [`DcdConfig::reassert_window_s`] (dB).
    pub reassert_extra_db: f64,
    /// After own PTT (unmuted plus the guard), quieting neither learns nor
    /// asserts until the open-squelch noise is back (one hop within
    /// [`DcdConfig::assert_drop_db`] of the floor), for at most this long
    /// (seconds): the radio's own transmit tail and key-down transient read
    /// as silence, not as a carrier. 0: off (legacy).
    pub recovery_max_s: f64,
}

impl Default for DcdConfig {
    fn default() -> Self {
        Self::for_width(BASE_TONES)
    }
}

impl DcdConfig {
    /// Defaults for a link of `width` tones: the quieting band starts at the
    /// band edge (highest tone + 50 Hz; 100 Hz above it beyond 24 tones) and
    /// is 400 Hz wide at 24 tones, 1000 Hz at 56: 2700-3100 Hz at 24 tones,
    /// 6000-7000 Hz at 56 (SPEC 7.2).
    pub fn for_width(width: usize) -> Self {
        let w = width.max(BASE_TONES);
        let extra = (w - BASE_TONES) as f64;
        let lo = band_high_hz(w) + if w > BASE_TONES { 100.0 } else { 0.0 };
        let hi = lo + 400.0 + extra * 600.0 / 32.0;
        DcdConfig {
            quieting_band_hz: (lo, hi),
            assert_drop_db: 6.0,
            release_drop_db: 3.0,
            min_floor_dbfs: -60.0,
            afsk: true,
            unmute_guard_s: 0.05,
            relearn_s: 60.0,
            release_confirm_hops: 1,
            min_off_s: 0.05,
            min_on_s: 0.03,
            reassert_window_s: 0.25,
            reassert_extra_db: 3.0,
            recovery_max_s: 1.5,
        }
    }

    /// The detector as first built (before the on-air session of
    /// 2026-11-07): release on the first hop, an assert window that may
    /// straddle a release, no recovery after own PTT. For comparisons.
    pub fn legacy(mut self) -> Self {
        self.release_confirm_hops = 1;
        self.min_off_s = 0.0;
        self.min_on_s = 0.0;
        self.reassert_window_s = 0.0;
        self.recovery_max_s = 0.0;
        self
    }
}

/// Which detector an edge comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DcdSource {
    /// Band-noise quieting.
    Quieting,
    /// Preamble correlator (caller-driven).
    Preamble,
    /// Bell 202 AFSK.
    Afsk,
    /// Radio COS line.
    Cos,
}

/// A detector changed state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DcdEdge {
    /// Absolute 48 kHz sample index where the change was decided (the end of
    /// the hop for the audio detectors; the caller's index otherwise).
    pub at: usize,
    /// The detector.
    pub source: DcdSource,
    /// Asserted (true) or released.
    pub up: bool,
}

/// The four detectors' current states, without hold.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct DcdState {
    /// Quieting.
    pub quieting: bool,
    /// Preamble correlator.
    pub preamble: bool,
    /// Bell 202.
    pub afsk: bool,
    /// COS.
    pub cos: bool,
}

impl DcdState {
    /// SPEC 7.2: the OR of the detectors.
    pub fn any(&self) -> bool {
        self.quieting || self.preamble || self.afsk || self.cos
    }
}

/// Diagnostics of the last hop, for status displays and tuning.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DcdMetrics {
    /// Quieting band power over the last 50 ms, dB re a mean square of 1.0.
    pub band_dbfs: Option<f64>,
    /// AFSK: mean tone dominance over the last 60 ms.
    pub afsk_dominance: f64,
    /// AFSK: bit-grid coherence of the transitions over the last 60 ms.
    pub afsk_coherence: f64,
    /// AFSK: rising (to 1200 Hz) transitions in the last 60 ms.
    pub afsk_rising: usize,
    /// AFSK: falling (to 2200 Hz) transitions in the last 60 ms.
    pub afsk_falling: usize,
    /// AFSK: longest gap between transitions in the last 60 ms, samples.
    pub afsk_gap: usize,
    /// AFSK: tone purity over the last 60 ms: inside tone runs, the phase
    /// stability of the run's DFT bin over one bit (1 for tones exactly on
    /// 1200 and 2200 Hz, cos(2 pi df 40 / 48000) for a tone df off).
    pub afsk_purity: f64,
}

// ---------------------------------------------------------------------------
// Quieting
// ---------------------------------------------------------------------------

/// Hops in the assert window (50 ms).
const Q_ASSERT_HOPS: usize = 5;
/// Hops in the release window (20 ms).
const Q_RELEASE_HOPS: usize = 2;
/// Hops in the floor-learning mean (200 ms).
const Q_LONG_HOPS: usize = 20;
/// Learning hops before the detector arms.
const Q_ARM_HOPS: usize = 50;
/// Floor tracking per hop: up, time constant 0.4 s; down, 2 s.
const Q_ALPHA_UP: f64 = 0.025;
const Q_ALPHA_DOWN: f64 = 0.005;
/// Tracking of the quieted level while asserted, per hop (0.2 s).
const Q_ALPHA_QUIETED: f64 = 0.05;
/// Disarm this far below the minimum floor (dB).
const Q_ARM_HYST_DB: f64 = 3.0;

struct Quieting {
    bp: BandPass,
    acc: f64,
    /// Mean square per hop, newest last; only unmuted hops after the guard.
    hops: VecDeque<f64>,
    floor: f64,
    n_learn: usize,
    armed: bool,
    up: bool,
    up_hops: usize,
    /// Band power while asserted (tracked).
    quieted: f64,
    since_unmute: usize,
    assert_k: f64,
    release_k: f64,
    arm_floor: f64,
    disarm_floor: f64,
    guard_hops: usize,
    relearn_hops: usize,
    release_confirm: usize,
    /// Consecutive hops meeting the release condition.
    release_run: usize,
    min_off_hops: usize,
    min_on_hops: usize,
    reassert_hops: usize,
    reassert_k: f64,
    /// Hops pushed since the last release or recovery (saturating).
    fresh: usize,
    recovery_hops: usize,
    /// After own PTT: waiting for the open-squelch noise to return, and the
    /// hops waited so far.
    recovering: Option<usize>,
    /// The last recovery's length (hops after the guard), once.
    recovered: Option<usize>,
}

fn db(x: f64) -> f64 {
    10.0 * x.max(1e-30).log10()
}

fn undb(x: f64) -> f64 {
    10f64.powf(x / 10.0)
}

impl Quieting {
    fn new(cfg: &DcdConfig) -> Self {
        let (lo, hi) = cfg.quieting_band_hz;
        Quieting {
            bp: BandPass::butterworth(4, lo, hi, FS as f64),
            acc: 0.0,
            hops: VecDeque::with_capacity(Q_LONG_HOPS + 1),
            floor: 0.0,
            n_learn: 0,
            armed: false,
            up: false,
            up_hops: 0,
            quieted: 0.0,
            since_unmute: 0,
            assert_k: undb(-cfg.assert_drop_db),
            release_k: undb(-cfg.release_drop_db),
            arm_floor: undb(cfg.min_floor_dbfs),
            disarm_floor: undb(cfg.min_floor_dbfs - Q_ARM_HYST_DB),
            guard_hops: (cfg.unmute_guard_s * FS as f64 / HOP as f64).ceil() as usize,
            relearn_hops: (cfg.relearn_s * FS as f64 / HOP as f64).ceil() as usize,
            release_confirm: cfg.release_confirm_hops.max(1),
            release_run: 0,
            min_off_hops: (cfg.min_off_s * FS as f64 / HOP as f64).round() as usize,
            min_on_hops: (cfg.min_on_s * FS as f64 / HOP as f64).round() as usize,
            reassert_hops: (cfg.reassert_window_s * FS as f64 / HOP as f64).round() as usize,
            reassert_k: undb(-cfg.reassert_extra_db),
            fresh: usize::MAX,
            recovery_hops: (cfg.recovery_max_s * FS as f64 / HOP as f64).round() as usize,
            recovering: None,
            recovered: None,
        }
    }

    fn mean_last(&self, k: usize) -> f64 {
        self.hops.iter().rev().take(k).sum::<f64>() / k as f64
    }

    /// End of a hop; returns the new state on a change.
    fn end_hop(&mut self, muted: bool) -> Option<bool> {
        let p = self.acc / HOP as f64;
        self.acc = 0.0;
        if muted {
            self.since_unmute = 0;
            self.hops.clear();
            self.release_run = 0;
            self.fresh = 0;
            if self.recovery_hops > 0 && self.armed {
                self.recovering = Some(0);
            }
            return self.set(false);
        }
        self.since_unmute += 1;
        if self.since_unmute <= self.guard_hops {
            return None;
        }
        if let Some(k) = self.recovering {
            // The radio's own transmit tail: silence until its receiver is
            // back. One hop within the assert margin of the floor ends it.
            if p >= self.floor * self.assert_k || k + 1 >= self.recovery_hops || !self.armed {
                self.recovering = None;
                self.recovered = Some(self.guard_hops + k + 1);
                self.fresh = 0;
            } else {
                self.recovering = Some(k + 1);
                return None;
            }
        }
        self.hops.push_back(p);
        if self.hops.len() > Q_LONG_HOPS {
            self.hops.pop_front();
        }
        self.fresh = self.fresh.saturating_add(1);
        let n = self.hops.len();
        if self.up {
            self.up_hops += 1;
            if self.up_hops >= self.relearn_hops {
                self.floor = self.mean_last(n);
                self.fresh = 0;
                return self.set(false);
            }
        } else if n == Q_LONG_HOPS {
            self.learn(p);
        }
        if !self.armed || n < Q_ASSERT_HOPS {
            return None;
        }
        let p_a = self.mean_last(Q_ASSERT_HOPS);
        let p_r = self.mean_last(Q_RELEASE_HOPS);
        if !self.up {
            // Decide on hops after the last release only (the window must
            // not straddle the carrier that just ended).
            if self.min_off_hops > 0 && self.fresh < Q_ASSERT_HOPS.max(self.min_off_hops) {
                return None;
            }
            let k = if self.fresh < self.reassert_hops { self.assert_k * self.reassert_k } else { self.assert_k };
            if p_a <= self.floor * k && p_r <= self.floor * self.release_k {
                self.quieted = p_a;
                self.release_run = 0;
                return self.set(true);
            }
            return None;
        }
        // Release at the release level, or halfway (in dB) between the
        // quieted level and the floor if that is lower: a deep carrier's
        // end is seen at once, a shallow one does not chatter.
        let release_level = (self.floor * self.release_k).min((self.floor * self.quieted).sqrt());
        if p_r > release_level {
            self.release_run += 1;
            if self.release_run >= self.release_confirm && self.up_hops >= self.min_on_hops {
                self.fresh = 0;
                return self.set(false);
            }
            return None;
        }
        self.release_run = 0;
        self.quieted += Q_ALPHA_QUIETED * (p - self.quieted);
        None
    }

    fn learn(&mut self, p: f64) {
        if self.n_learn == 0 {
            self.floor = self.mean_last(Q_LONG_HOPS);
            self.n_learn = Q_LONG_HOPS;
        } else if self.n_learn < Q_ARM_HOPS {
            self.floor += (p - self.floor) / (self.n_learn + 1) as f64;
            self.n_learn += 1;
        } else {
            let l = self.mean_last(Q_LONG_HOPS);
            let a = if l > self.floor { Q_ALPHA_UP } else { Q_ALPHA_DOWN };
            self.floor += a * (l - self.floor);
        }
        if self.n_learn >= Q_ARM_HOPS {
            if self.armed && self.floor < self.disarm_floor {
                self.armed = false;
            } else if !self.armed && self.floor >= self.arm_floor {
                self.armed = true;
            }
        }
    }

    fn set(&mut self, up: bool) -> Option<bool> {
        if self.up == up {
            return None;
        }
        self.up = up;
        self.up_hops = 0;
        self.release_run = 0;
        Some(up)
    }
}

// ---------------------------------------------------------------------------
// AFSK
// ---------------------------------------------------------------------------

/// Sliding DFT length: one bit at 1200 baud.
const A_N: usize = 40;
/// 2200 Hz repeats every 240 samples at 48 kHz.
const A_TAB2: usize = 240;
/// Log-ratio hysteresis for a tone transition.
const A_H: f64 = 1.0;
/// Peak normalised tone power an excursion needs for its transition to count.
const A_TMIN: f64 = 0.4;
/// Assert window (60 ms) and hold window (30 ms), hops.
const A_ASSERT_HOPS: usize = 6;
const A_HOLD_HOPS: usize = 3;
const A_D_ASSERT: f64 = 0.80;
const A_D_HOLD: f64 = 0.70;
const A_C_ASSERT: f64 = 0.60;
const A_C_HOLD: f64 = 0.45;
const A_Q_ASSERT: f64 = 0.8;
const A_N_ASSERT: usize = 4;
const A_N_HOLD: usize = 1;
/// Consecutive failing hops that release.
const A_HOLD_FAILS: usize = 1;
/// Longest allowed gap between transitions: 9 bits.
const A_GAP_MAX: usize = 9 * A_N;
/// Minimum mean power for a decision (-90 dB).
const A_P_MIN: f64 = 1e-9;
/// Recompute the sliding sums exactly this often (rounding drift).
const A_REFRESH: usize = 1 << 15;
/// Excursion age from which the purity is accumulated (both DFT windows
/// inside one tone run).
const A_PURITY_AGE: usize = 60;

#[derive(Clone, Copy, Debug)]
struct Transition {
    t: usize,
    phasor: Complex64,
    rising: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowStats {
    rising: usize,
    falling: usize,
    coherence: f64,
    gap: usize,
    dominance: f64,
    power: f64,
    purity: f64,
}

struct Afsk {
    buf: [f64; A_N],
    tab1: [Complex64; A_N],
    tab2: [Complex64; A_TAB2],
    s1: Complex64,
    s2: Complex64,
    e: f64,
    /// The sliding DFTs one bit ago (indexed by sample index mod 40).
    hist1: [Complex64; A_N],
    hist2: [Complex64; A_N],
    /// Purity sums, Re(X(n) X*(n-40)) and |X(n)| |X(n-40)| of the bin of
    /// the current tone run, this hop and per hop (newest last).
    q_num: f64,
    q_den: f64,
    hop_q: VecDeque<(f64, f64)>,
    since_refresh: usize,
    p1_prev: f64,
    p2_prev: f64,
    /// Excursion: +1 (1200 Hz above +h), -1 (2200 Hz below -h), 0 none yet.
    state: i8,
    peak: f64,
    /// Samples since the current excursion began.
    age: usize,
    pending: Option<Transition>,
    trans: VecDeque<Transition>,
    d_acc: f64,
    p_acc: f64,
    /// Per hop (newest last): mean dominance, mean power.
    hop_d: VecDeque<f64>,
    hop_p: VecDeque<f64>,
    valid_hops: usize,
    up: bool,
    /// Consecutive hops failing the hold rule.
    fails: usize,
    last: WindowStats,
}

impl Afsk {
    fn new() -> Self {
        let mut tab1 = [Complex64::new(0.0, 0.0); A_N];
        for (m, v) in tab1.iter_mut().enumerate() {
            *v = Complex64::from_polar(1.0, -2.0 * PI * m as f64 / A_N as f64);
        }
        let mut tab2 = [Complex64::new(0.0, 0.0); A_TAB2];
        for (m, v) in tab2.iter_mut().enumerate() {
            *v = Complex64::from_polar(1.0, -2.0 * PI * 11.0 * m as f64 / A_TAB2 as f64);
        }
        Afsk {
            buf: [0.0; A_N],
            tab1,
            tab2,
            s1: Complex64::new(0.0, 0.0),
            s2: Complex64::new(0.0, 0.0),
            e: 0.0,
            hist1: [Complex64::new(0.0, 0.0); A_N],
            hist2: [Complex64::new(0.0, 0.0); A_N],
            q_num: 0.0,
            q_den: 0.0,
            hop_q: VecDeque::with_capacity(A_ASSERT_HOPS + 1),
            since_refresh: 0,
            p1_prev: 0.0,
            p2_prev: 0.0,
            state: 0,
            peak: 0.0,
            age: 0,
            pending: None,
            trans: VecDeque::new(),
            d_acc: 0.0,
            p_acc: 0.0,
            hop_d: VecDeque::with_capacity(A_ASSERT_HOPS + 1),
            hop_p: VecDeque::with_capacity(A_ASSERT_HOPS + 1),
            valid_hops: 0,
            up: false,
            fails: 0,
            last: WindowStats::default(),
        }
    }

    /// Sample `x` at absolute index `i`.
    #[inline]
    fn push(&mut self, x: f64, i: usize) {
        let k = i % A_N;
        let old = self.buf[k];
        self.buf[k] = x;
        let m2 = i % A_TAB2;
        let m2_old = (m2 + A_TAB2 - A_N) % A_TAB2;
        self.s1 += self.tab1[k] * (x - old);
        self.s2 += self.tab2[m2] * x - self.tab2[m2_old] * old;
        self.e += x * x - old * old;
        self.since_refresh += 1;
        if self.since_refresh >= A_REFRESH {
            self.refresh(i);
        }
        self.p_acc += x * x;
        let (h1, h2) = (self.hist1[k], self.hist2[k]);
        self.hist1[k] = self.s1;
        self.hist2[k] = self.s2;
        let (cur1, cur2) = (self.s1, self.s2);
        let e = self.e.max(0.0);
        if e <= 1e-15 {
            self.p1_prev = 0.0;
            self.p2_prev = 0.0;
            return;
        }
        let p1 = self.s1.norm_sqr();
        let p2 = self.s2.norm_sqr();
        let norm = 2.0 / (A_N as f64 * e);
        let (t1, t2) = (p1 * norm, p2 * norm);
        self.d_acc += t1 + t2;
        let eh = A_H.exp();
        match self.state {
            1 => {
                self.peak = self.peak.max(t1);
                if p2 * eh > p1 {
                    self.cross(i, -A_H, p1, p2, false);
                    self.state = -1;
                    self.age = 0;
                    self.peak = t2;
                }
                self.confirm();
            }
            -1 => {
                self.peak = self.peak.max(t2);
                if p1 > p2 * eh {
                    self.cross(i, A_H, p1, p2, true);
                    self.state = 1;
                    self.age = 0;
                    self.peak = t1;
                }
                self.confirm();
            }
            _ => {
                if p1 > p2 * eh {
                    self.state = 1;
                    self.age = 0;
                    self.peak = t1;
                } else if p2 * eh > p1 {
                    self.state = -1;
                    self.age = 0;
                    self.peak = t2;
                }
            }
        }
        self.p1_prev = p1;
        self.p2_prev = p2;
        self.age += 1;
        if self.age >= A_PURITY_AGE && self.state != 0 {
            let (c, h) = if self.state == 1 { (cur1, h1) } else { (cur2, h2) };
            self.q_num += (c * h.conj()).re;
            self.q_den += (c.norm_sqr() * h.norm_sqr()).sqrt();
        }
    }

    /// A crossing of r through `level` between samples i-1 and i: pending
    /// if the excursion it ends reached [`A_TMIN`] on its tone, counted once
    /// the excursion it starts does too ([`Afsk::confirm`]).
    fn cross(&mut self, i: usize, level: f64, p1: f64, p2: f64, rising: bool) {
        self.pending = None;
        if self.peak < A_TMIN {
            return;
        }
        let tiny = 1e-30;
        let r = ((p1 + tiny) / (p2 + tiny)).ln();
        let r_prev = if self.p1_prev > 0.0 || self.p2_prev > 0.0 {
            ((self.p1_prev + tiny) / (self.p2_prev + tiny)).ln()
        } else {
            r
        };
        let frac = if (r - r_prev).abs() > 1e-12 { ((level - r_prev) / (r - r_prev)).clamp(0.0, 1.0) } else { 1.0 };
        let ph = ((i % A_N) as f64 - 1.0 + frac) / A_N as f64;
        self.pending = Some(Transition { t: i, phasor: Complex64::from_polar(1.0, 2.0 * PI * ph), rising });
    }

    fn confirm(&mut self) {
        if self.peak >= A_TMIN
            && let Some(tr) = self.pending.take()
        {
            self.trans.push_back(tr);
        }
    }

    fn refresh(&mut self, i: usize) {
        self.since_refresh = 0;
        let (mut s1, mut s2, mut e) = (Complex64::new(0.0, 0.0), Complex64::new(0.0, 0.0), 0.0);
        for d in 0..A_N.min(i + 1) {
            let idx = i - d;
            let x = self.buf[idx % A_N];
            s1 += self.tab1[idx % A_N] * x;
            s2 += self.tab2[idx % A_TAB2] * x;
            e += x * x;
        }
        self.s1 = s1;
        self.s2 = s2;
        self.e = e;
    }

    fn stats(&self, end: usize, hops: usize) -> WindowStats {
        let from = end.saturating_sub(hops * HOP);
        let (mut sr, mut sf) = (Complex64::new(0.0, 0.0), Complex64::new(0.0, 0.0));
        let (mut nr, mut nf) = (0, 0);
        let mut prev = from;
        let mut gap = 0;
        for tr in self.trans.iter().filter(|t| t.t >= from && t.t < end) {
            if tr.rising {
                sr += tr.phasor;
                nr += 1;
            } else {
                sf += tr.phasor;
                nf += 1;
            }
            gap = gap.max(tr.t - prev);
            prev = tr.t;
        }
        gap = gap.max(end - prev);
        let n = (nr + nf).max(1) as f64;
        let k = hops.min(self.hop_d.len()).max(1);
        WindowStats {
            rising: nr,
            falling: nf,
            coherence: (sr.norm() + sf.norm()) / n,
            gap,
            dominance: self.hop_d.iter().rev().take(k).sum::<f64>() / k as f64,
            power: self.hop_p.iter().rev().take(k).sum::<f64>() / k as f64,
            purity: self.hop_q.iter().rev().take(k).map(|v| v.0).sum::<f64>()
                / self.hop_q.iter().rev().take(k).map(|v| v.1).sum::<f64>().max(1e-300),
        }
    }

    /// End of the hop ending at absolute index `end`.
    fn end_hop(&mut self, end: usize, muted: bool, enabled: bool) -> Option<bool> {
        self.hop_d.push_back(self.d_acc / HOP as f64);
        self.hop_p.push_back(self.p_acc / HOP as f64);
        self.hop_q.push_back((self.q_num, self.q_den));
        self.d_acc = 0.0;
        self.p_acc = 0.0;
        self.q_num = 0.0;
        self.q_den = 0.0;
        if self.hop_d.len() > A_ASSERT_HOPS {
            self.hop_d.pop_front();
            self.hop_p.pop_front();
            self.hop_q.pop_front();
        }
        let from = end.saturating_sub(A_ASSERT_HOPS * HOP);
        while self.trans.front().is_some_and(|t| t.t < from) {
            self.trans.pop_front();
        }
        if muted {
            self.valid_hops = 0;
            self.trans.clear();
            self.pending = None;
            self.hop_d.clear();
            self.hop_p.clear();
            self.hop_q.clear();
            return self.set(false);
        }
        self.valid_hops += 1;
        self.last = self.stats(end, A_ASSERT_HOPS);
        if !enabled {
            return self.set(false);
        }
        if !self.up {
            let s = self.last;
            if self.valid_hops >= A_ASSERT_HOPS
                && s.power >= A_P_MIN
                && s.dominance >= A_D_ASSERT
                && s.rising >= A_N_ASSERT
                && s.falling >= A_N_ASSERT
                && s.coherence >= A_C_ASSERT
                && s.purity >= A_Q_ASSERT
                && s.gap <= A_GAP_MAX
            {
                return self.set(true);
            }
            None
        } else {
            let s = self.stats(end, A_HOLD_HOPS);
            let hold = s.power >= A_P_MIN
                && s.dominance >= A_D_HOLD
                && s.rising >= A_N_HOLD
                && s.falling >= A_N_HOLD
                && s.coherence >= A_C_HOLD;
            self.fails = if hold { 0 } else { self.fails + 1 };
            if self.fails >= A_HOLD_FAILS { self.set(false) } else { None }
        }
    }

    fn set(&mut self, up: bool) -> Option<bool> {
        if self.up == up {
            return None;
        }
        self.up = up;
        self.fails = 0;
        Some(up)
    }
}

// ---------------------------------------------------------------------------
// Dcd
// ---------------------------------------------------------------------------

/// Carrier detect: quieting and Bell 202 from the receive audio, the
/// preamble correlator and COS from the caller.
pub struct Dcd {
    cfg: DcdConfig,
    pos: usize,
    hop_fill: usize,
    muted: bool,
    muted_in_hop: bool,
    state: DcdState,
    q: Quieting,
    afsk: Afsk,
}

impl Dcd {
    /// A detector with `cfg`.
    pub fn new(cfg: DcdConfig) -> Self {
        let q = Quieting::new(&cfg);
        Dcd {
            cfg,
            pos: 0,
            hop_fill: 0,
            muted: false,
            muted_in_hop: false,
            state: DcdState::default(),
            q,
            afsk: Afsk::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &DcdConfig {
        &self.cfg
    }

    /// Feed receive audio (any length). Decisions are taken on fixed 10 ms
    /// hops counted from the first sample, independent of the chunking.
    /// Returns every detector edge decided in this call, in order.
    pub fn process(&mut self, x: &[f64]) -> Vec<DcdEdge> {
        let mut edges = Vec::new();
        for &v in x {
            let y = self.q.bp.process(v);
            self.q.acc += y * y;
            self.afsk.push(v, self.pos);
            self.pos += 1;
            self.hop_fill += 1;
            if self.hop_fill == HOP {
                self.hop_fill = 0;
                self.end_hop(&mut edges);
            }
        }
        edges
    }

    fn end_hop(&mut self, edges: &mut Vec<DcdEdge>) {
        let muted = self.muted || self.muted_in_hop;
        self.muted_in_hop = self.muted;
        let at = self.pos;
        if let Some(up) = self.q.end_hop(muted) {
            self.state.quieting = up;
            edges.push(DcdEdge { at, source: DcdSource::Quieting, up });
        }
        if let Some(up) = self.afsk.end_hop(at, muted, self.cfg.afsk) {
            self.state.afsk = up;
            edges.push(DcdEdge { at, source: DcdSource::Afsk, up });
        }
    }

    /// Set the preamble correlator's state at sample index `at` (the caller
    /// runs the correlator and knows frame lengths from headers). Returns the
    /// edge if the state changed.
    pub fn set_preamble(&mut self, up: bool, at: usize) -> Option<DcdEdge> {
        if self.state.preamble == up {
            return None;
        }
        self.state.preamble = up;
        Some(DcdEdge { at, source: DcdSource::Preamble, up })
    }

    /// Set the radio COS line (None: not wired, reads as clear). Returns the
    /// edge if the state changed.
    pub fn set_cos(&mut self, cos: Option<bool>, at: usize) -> Option<DcdEdge> {
        let up = cos.unwrap_or(false);
        if self.state.cos == up {
            return None;
        }
        self.state.cos = up;
        Some(DcdEdge { at, source: DcdSource::Cos, up })
    }

    /// Own PTT keyed (half duplex): quieting and AFSK report clear and
    /// nothing is learnt from the audio until unmuted, plus the unmute guard
    /// for quieting. Muting takes effect for the whole current hop. Returns
    /// the release edges (at the current position) of detectors that were up.
    pub fn set_muted(&mut self, muted: bool) -> Vec<DcdEdge> {
        let mut edges = Vec::new();
        self.muted = muted;
        if muted {
            self.muted_in_hop = true;
            if self.q.set(false).is_some() {
                self.state.quieting = false;
                edges.push(DcdEdge { at: self.pos, source: DcdSource::Quieting, up: false });
            }
            if self.afsk.set(false).is_some() {
                self.state.afsk = false;
                edges.push(DcdEdge { at: self.pos, source: DcdSource::Afsk, up: false });
            }
        }
        edges
    }

    /// After own PTT: how long quieting waited for the open-squelch noise
    /// to return (from unmuting, the guard included), seconds, once per
    /// unmute; `None` until it has returned (or when recovery is off).
    pub fn take_recovery(&mut self) -> Option<f64> {
        self.q.recovered.take().map(|h| h as f64 * HOP as f64 / FS as f64)
    }

    /// Quieting is waiting for the open-squelch noise to return after own PTT.
    pub fn recovering(&self) -> bool {
        self.q.recovering.is_some()
    }

    /// Whether muted.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// The detectors' states (no hold).
    pub fn state(&self) -> DcdState {
        self.state
    }

    /// OR of the detectors, no hold.
    pub fn raw(&self) -> bool {
        self.state.any()
    }

    /// Samples fed so far (the absolute index of the next sample).
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Tracked open-squelch floor in the quieting band (dB re a mean square
    /// of 1.0), None while quieting is unarmed.
    pub fn floor_dbfs(&self) -> Option<f64> {
        self.q.armed.then(|| db(self.q.floor))
    }

    /// Diagnostics of the last hop.
    pub fn metrics(&self) -> DcdMetrics {
        let s = self.afsk.last;
        DcdMetrics {
            band_dbfs: (self.q.hops.len() >= Q_ASSERT_HOPS).then(|| db(self.q.mean_last(Q_ASSERT_HOPS))),
            afsk_dominance: s.dominance,
            afsk_coherence: s.coherence,
            afsk_rising: s.rising,
            afsk_falling: s.falling,
            afsk_gap: s.gap,
            afsk_purity: s.purity,
        }
    }
}

/// The SPEC 7.2 OR with a hold (default 100 ms, range 50-300 ms), for
/// display and status; the link layer applies its own hold to raw edges.
#[derive(Clone, Debug)]
pub struct HeldDcd {
    hold: usize,
    current: bool,
    /// Last index at which the OR was true (or went false).
    last: Option<usize>,
}

impl HeldDcd {
    /// Default hold, samples (100 ms).
    pub const DEFAULT_HOLD: usize = 4800;

    /// A held OR with `hold_samples` of hold.
    pub fn new(hold_samples: usize) -> Self {
        HeldDcd { hold: hold_samples, current: false, last: None }
    }

    /// The detectors' state as of sample index `at`.
    pub fn update(&mut self, state: DcdState, at: usize) {
        let any = state.any();
        if any || self.current {
            self.last = Some(at);
        }
        self.current = any;
    }

    /// DCD with hold at sample index `at`.
    pub fn held(&self, at: usize) -> bool {
        self.current || self.last.is_some_and(|t| at < t + self.hold)
    }
}

impl Default for HeldDcd {
    fn default() -> Self {
        Self::new(Self::DEFAULT_HOLD)
    }
}
