//! Receive AGC (SPEC 7.1 receive side, 13.5): the capture gain, moved in
//! 3 dB steps between frames only.
//!
//! Targets (13.5): open-squelch noise near -20 dBFS RMS, signal peaks never
//! above -3 dBFS, no clipping; the data's RMS stays inside the receive
//! window of 7.1 (-30 to -3 dBFS). The controller is pure: the caller feeds
//! level windows ([`LevelObs`]: idle noise between frames, and each frame's
//! own levels) and asks, whenever the receiver is idle, whether to step
//! ([`RxAgc::decide`]); the caller sets the mixer and tells carrier detect
//! (`Dcd::level_step`).
//!
//! Rules, in order:
//! 1. Clipping anywhere, or a frame's peaks above -3 dBFS: one step down,
//!    allowed 2 s after the previous step.
//! 2. With an open-squelch noise reference (idle windows louder than
//!    -45 dBFS; quieter means the squelch is closed or no radio): noise RMS
//!    outside -20 +- 3 dB in two windows running steps it back, down
//!    unless that would put the data below -30 dBFS RMS, up unless the last
//!    frames' peaks (or the noise's own peaks) would then pass -3 dBFS.
//! 3. Without a noise reference: the frames' mean RMS outside -18 +- 4 dB
//!    steps it back, with the same peak check.
//!
//! Hysteresis: the dead band (6-8 dB wide) is wider than a step, so a step
//! never lands outside it on the other side; two windows must agree;
//! measurements taken before a step and in the second after it are
//! discarded; at most one step every 10 s except for clipping; and no step
//! within 300 ms of a frame's end (the peer may be about to answer).

use std::collections::VecDeque;

/// Controller settings.
#[derive(Clone, Debug, PartialEq)]
pub struct AgcConfig {
    /// Open-squelch noise target, dBFS RMS (SPEC 13.5: -20).
    pub noise_target_dbfs: f64,
    /// Dead band either side of the noise target, dB.
    pub noise_band_db: f64,
    /// Data RMS target without a noise reference, dBFS.
    pub data_target_dbfs: f64,
    /// Dead band either side of the data target, dB.
    pub data_band_db: f64,
    /// Peaks above this are too hot (SPEC 7.1, 13.5: -3 dBFS).
    pub peak_max_dbfs: f64,
    /// Bottom of the receive window (SPEC 7.1: -30 dBFS).
    pub data_floor_dbfs: f64,
    /// Idle windows quieter than this are no noise reference (squelch closed).
    pub squelch_floor_dbfs: f64,
    /// Gain step, dB (SPEC 13.5: 3 dB).
    pub step_db: f64,
    /// Shortest time between steps, s.
    pub min_interval_s: f64,
    /// Shortest time between steps for clipping or hot peaks, s.
    pub clip_interval_s: f64,
    /// After a step, measurements are discarded for this long, s.
    pub settle_s: f64,
    /// Out-of-band noise windows needed in a row.
    pub confirm: usize,
    /// No step this soon after a frame ended, s.
    pub after_frame_s: f64,
    /// Frames remembered for the data level and peak checks.
    pub frames_kept: usize,
    /// Frames older than this are forgotten, s.
    pub frame_memory_s: f64,
}

impl Default for AgcConfig {
    fn default() -> Self {
        AgcConfig {
            noise_target_dbfs: -20.0,
            noise_band_db: 3.0,
            data_target_dbfs: -18.0,
            data_band_db: 4.0,
            peak_max_dbfs: -3.0,
            data_floor_dbfs: -30.0,
            squelch_floor_dbfs: -45.0,
            step_db: 3.0,
            min_interval_s: 10.0,
            clip_interval_s: 2.0,
            settle_s: 1.0,
            confirm: 2,
            after_frame_s: 0.3,
            frames_kept: 4,
            frame_memory_s: 120.0,
        }
    }
}

/// What a level window measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelKind {
    /// Between frames, with the receiver idle (open-squelch noise or silence).
    Idle,
    /// One received frame.
    Frame,
}

/// One level window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelObs {
    /// What it measured.
    pub kind: LevelKind,
    /// When it ended, s (the caller's clock).
    pub at_s: f64,
    /// RMS about the mean, dBFS.
    pub rms_dbfs: f64,
    /// Peak, dBFS.
    pub peak_dbfs: f64,
    /// Any sample clipped.
    pub clipped: bool,
}

/// A gain change to make.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// Gain before, dB.
    pub from_db: f64,
    /// Gain to set, dB.
    pub to_db: f64,
    /// Why, for the log.
    pub reason: String,
}

/// The receive-gain controller.
#[derive(Clone, Debug)]
pub struct RxAgc {
    cfg: AgcConfig,
    last_step_s: Option<f64>,
    settle_until_s: f64,
    noise: Option<LevelObs>,
    noise_high: usize,
    noise_low: usize,
    noise_peak_dbfs: f64,
    frames: VecDeque<LevelObs>,
    urgent: Option<String>,
    last_frame_s: Option<f64>,
    /// Steps taken.
    pub changes: u64,
    /// Why the last step was taken.
    pub last_reason: Option<String>,
}

impl RxAgc {
    /// A controller with `cfg`.
    pub fn new(cfg: AgcConfig) -> Self {
        RxAgc {
            cfg,
            last_step_s: None,
            settle_until_s: f64::NEG_INFINITY,
            noise: None,
            noise_high: 0,
            noise_low: 0,
            noise_peak_dbfs: f64::NEG_INFINITY,
            frames: VecDeque::new(),
            urgent: None,
            last_frame_s: None,
            changes: 0,
            last_reason: None,
        }
    }

    /// The settings.
    pub fn config(&self) -> &AgcConfig {
        &self.cfg
    }

    /// The last idle window's RMS, if it is a noise reference.
    pub fn noise_dbfs(&self) -> Option<f64> {
        self.noise.map(|n| n.rms_dbfs)
    }

    /// A level window.
    pub fn observe(&mut self, o: LevelObs) {
        if o.kind == LevelKind::Frame {
            self.last_frame_s = Some(self.last_frame_s.map_or(o.at_s, |t| t.max(o.at_s)));
        }
        if o.at_s < self.settle_until_s {
            return;
        }
        let c = &self.cfg;
        if o.clipped {
            self.urgent = Some(format!(
                "input clipped ({})",
                if o.kind == LevelKind::Frame { "during a frame" } else { "between frames" }
            ));
        }
        match o.kind {
            LevelKind::Frame => {
                if !o.clipped && o.peak_dbfs > c.peak_max_dbfs {
                    self.urgent = Some(format!("frame peaks {:.1} dBFS, above {:.0}", o.peak_dbfs, c.peak_max_dbfs));
                }
                self.frames.push_back(o);
                while self.frames.len() > c.frames_kept {
                    self.frames.pop_front();
                }
            }
            LevelKind::Idle => {
                if o.rms_dbfs < c.squelch_floor_dbfs {
                    self.noise = None;
                    self.noise_high = 0;
                    self.noise_low = 0;
                } else {
                    self.noise = Some(o);
                    self.noise_peak_dbfs = o.peak_dbfs;
                    if o.rms_dbfs > c.noise_target_dbfs + c.noise_band_db {
                        self.noise_high += 1;
                        self.noise_low = 0;
                    } else if o.rms_dbfs < c.noise_target_dbfs - c.noise_band_db {
                        self.noise_low += 1;
                        self.noise_high = 0;
                    } else {
                        self.noise_high = 0;
                        self.noise_low = 0;
                    }
                }
            }
        }
    }

    fn recent_frames(&self, now_s: f64) -> impl Iterator<Item = &LevelObs> {
        let mem = self.cfg.frame_memory_s;
        self.frames.iter().filter(move |f| now_s - f.at_s <= mem)
    }

    /// The step to take now, if any: call while the receiver is `idle` (no
    /// carrier, no frame detected or being decoded, not transmitting) with
    /// the gain now in effect and the control's range.
    pub fn decide(&mut self, now_s: f64, idle: bool, gain_db: f64, range: (f64, f64)) -> Option<Step> {
        let c = self.cfg.clone();
        if !idle || now_s < self.settle_until_s {
            return None;
        }
        if self.last_frame_s.is_some_and(|t| now_s - t < c.after_frame_s) {
            return None;
        }
        let since = self.last_step_s.map_or(f64::INFINITY, |t| now_s - t);
        let peak = self.recent_frames(now_s).map(|f| f.peak_dbfs).fold(f64::NEG_INFINITY, f64::max);
        let data_rms: Vec<f64> = self.recent_frames(now_s).map(|f| f.rms_dbfs).collect();
        let data_min = data_rms.iter().copied().fold(f64::INFINITY, f64::min);
        // Peaks after a step up: the frames', and the noise's own (it must not clip).
        let peak_after_up = peak.max(if self.noise.is_some() { self.noise_peak_dbfs } else { f64::NEG_INFINITY });
        let up_ok = |d: f64| peak_after_up + d <= c.peak_max_dbfs - 0.5;
        let down_ok = |d: f64| data_rms.is_empty() || data_min - d >= c.data_floor_dbfs;
        let want: Option<(f64, String)> = if let Some(r) = self.urgent.clone() {
            (since >= c.clip_interval_s).then(|| (-c.step_db, r))
        } else if since < c.min_interval_s {
            None
        } else if let Some(n) = self.noise {
            if self.noise_high >= c.confirm && down_ok(c.step_db) {
                Some((
                    -c.step_db,
                    format!(
                        "open-squelch noise {:.1} dBFS, above {:.0}",
                        n.rms_dbfs,
                        c.noise_target_dbfs + c.noise_band_db
                    ),
                ))
            } else if self.noise_low >= c.confirm && up_ok(c.step_db) {
                Some((
                    c.step_db,
                    format!(
                        "open-squelch noise {:.1} dBFS, below {:.0}",
                        n.rms_dbfs,
                        c.noise_target_dbfs - c.noise_band_db
                    ),
                ))
            } else {
                None
            }
        } else if !data_rms.is_empty() {
            let mean = data_rms.iter().sum::<f64>() / data_rms.len() as f64;
            if mean > c.data_target_dbfs + c.data_band_db {
                Some((-c.step_db, format!("data {mean:.1} dBFS RMS, above {:.0}", c.data_target_dbfs + c.data_band_db)))
            } else if mean < c.data_target_dbfs - c.data_band_db && up_ok(c.step_db) {
                Some((c.step_db, format!("data {mean:.1} dBFS RMS, below {:.0}", c.data_target_dbfs - c.data_band_db)))
            } else {
                None
            }
        } else {
            None
        };
        let (d, reason) = want?;
        let to = (gain_db + d).clamp(range.0, range.1);
        if (to - gain_db).abs() < 0.01 {
            // At the end of the range: nothing to do; forget the reason.
            self.urgent = None;
            self.noise_high = 0;
            self.noise_low = 0;
            return None;
        }
        self.last_step_s = Some(now_s);
        self.settle_until_s = now_s + c.settle_s;
        self.urgent = None;
        self.noise = None;
        self.noise_high = 0;
        self.noise_low = 0;
        self.frames.clear();
        self.changes += 1;
        self.last_reason = Some(reason.clone());
        Some(Step { from_db: gain_db, to_db: to, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: (f64, f64) = (-24.0, 12.0);

    fn idle(at_s: f64, rms: f64) -> LevelObs {
        LevelObs { kind: LevelKind::Idle, at_s, rms_dbfs: rms, peak_dbfs: rms + 11.0, clipped: false }
    }

    fn frame(at_s: f64, rms: f64, peak: f64) -> LevelObs {
        LevelObs { kind: LevelKind::Frame, at_s, rms_dbfs: rms, peak_dbfs: peak, clipped: peak >= -0.01 }
    }

    /// Run a channel whose input-referred levels are `noise` (open squelch)
    /// and `data` (frame RMS, peaks 9 dB above), from gain `g`, for
    /// `seconds`, with a frame every `frame_every` s (0: none); returns the
    /// final gain and the steps.
    fn simulate(noise: Option<f64>, data: f64, mut g: f64, seconds: f64, frame_every: f64) -> (f64, Vec<Step>) {
        let mut agc = RxAgc::new(AgcConfig::default());
        let mut steps = Vec::new();
        let mut t = 0.0;
        let mut next_frame = frame_every;
        while t < seconds {
            t += 0.5;
            let mut busy = false;
            if frame_every > 0.0 && t >= next_frame {
                next_frame += frame_every;
                let rms = data + g;
                agc.observe(frame(t, rms, (rms + 9.0).min(0.0)));
                busy = true;
            } else {
                let rms = noise.map_or(-70.0, |n| n + g);
                let mut o = idle(t, rms);
                o.clipped = o.peak_dbfs >= 0.0;
                agc.observe(o);
            }
            if let Some(s) = agc.decide(t + 0.4, !busy, g, RANGE) {
                g = s.to_db;
                steps.push(s);
            }
        }
        (g, steps)
    }

    #[test]
    fn loud_noise_steps_down_to_the_band_and_stays() {
        // The bench: open-squelch noise at -12 dBFS, data at -14 dBFS RMS.
        let (g, steps) = simulate(Some(-12.0), -14.0, 0.0, 120.0, 6.0);
        assert!(!steps.is_empty());
        let noise = -12.0 + g;
        assert!((-23.0..=-17.0).contains(&noise), "noise {noise} dBFS in the band; steps {steps:?}");
        assert!(steps.iter().all(|s| (s.to_db - s.from_db).abs() <= 3.0 + 1e-9), "3 dB steps");
        // Steps at least 10 s apart (no clipping here).
        let n = steps.len();
        let (g2, more) = simulate(Some(-12.0 + g), -14.0 + g, 0.0, 120.0, 6.0);
        assert!(more.is_empty(), "already in the band: no step ({more:?}, {g2})");
        assert!(n <= 3, "{steps:?}");
    }

    #[test]
    fn quiet_noise_steps_up_but_not_past_the_peaks() {
        // Noise at -32 dBFS: wants +12 dB, but frames peak at -8 dBFS and
        // may only come up to -3.5.
        let (g, steps) = simulate(Some(-32.0), -17.0, 0.0, 200.0, 7.0);
        assert!(!steps.is_empty());
        assert!(steps.iter().all(|s| s.to_db > s.from_db), "only up: {steps:?}");
        let peak = -17.0 + 9.0 + g;
        assert!(peak <= -3.0, "peaks {peak} dBFS stay at or below -3");
        assert!((g - 3.0).abs() < 1e-9, "one step up fits under the peaks ({g})");
    }

    #[test]
    fn clipping_steps_down_fast() {
        let mut agc = RxAgc::new(AgcConfig::default());
        agc.observe(frame(1.0, -6.0, 0.0));
        assert!(agc.decide(1.1, true, 0.0, RANGE).is_none(), "not within 300 ms of a frame");
        let s = agc.decide(1.5, true, 0.0, RANGE).expect("step down");
        assert_eq!((s.from_db, s.to_db), (0.0, -3.0));
        assert!(s.reason.contains("clipped"), "{}", s.reason);
        // Clipping again 2.5 s later: another step at once (2 s rule).
        agc.observe(frame(3.5, -9.0, 0.0));
        assert!(agc.decide(3.6, true, -3.0, RANGE).is_none());
        let s = agc.decide(4.0, true, -3.0, RANGE).expect("second step");
        assert_eq!(s.to_db, -6.0);
        // Hot (not clipped) peaks count too.
        let mut agc = RxAgc::new(AgcConfig::default());
        agc.observe(frame(1.0, -12.0, -2.0));
        assert!(agc.decide(2.0, true, 0.0, RANGE).unwrap().reason.contains("peaks -2.0"));
    }

    #[test]
    fn never_while_busy() {
        let mut agc = RxAgc::new(AgcConfig::default());
        agc.observe(frame(1.0, -6.0, 0.0));
        for k in 0..20 {
            assert!(agc.decide(2.0 + k as f64 * 0.5, false, 0.0, RANGE).is_none(), "DCD up or a frame in progress");
        }
        assert!(agc.decide(20.0, true, 0.0, RANGE).is_some(), "as soon as the receiver is idle");
    }

    #[test]
    fn stable_input_never_moves() {
        let (g, steps) = simulate(Some(-20.0), -18.0, 0.0, 300.0, 5.0);
        assert!(steps.is_empty(), "{steps:?}");
        assert_eq!(g, 0.0);
        // Noise wobbling inside the dead band either.
        let mut agc = RxAgc::new(AgcConfig::default());
        for k in 0..200 {
            let t = k as f64 * 0.5;
            agc.observe(idle(t, -20.0 + 2.5 * (k as f64 * 0.7).sin()));
            assert!(agc.decide(t + 0.1, true, 0.0, RANGE).is_none());
        }
    }

    #[test]
    fn one_odd_window_is_not_enough() {
        let mut agc = RxAgc::new(AgcConfig::default());
        agc.observe(idle(11.0, -12.0));
        assert!(agc.decide(11.1, true, 0.0, RANGE).is_none(), "one loud window");
        agc.observe(idle(11.5, -20.0));
        agc.observe(idle(12.0, -12.0));
        assert!(agc.decide(12.1, true, 0.0, RANGE).is_none(), "not two in a row");
        agc.observe(idle(12.5, -12.0));
        assert!(agc.decide(12.6, true, 0.0, RANGE).is_some());
    }

    #[test]
    fn squelch_closed_uses_the_data() {
        // No noise reference (squelch closed): frames at -28 dBFS RMS come up.
        let (g, steps) = simulate(None, -28.0, 0.0, 200.0, 5.0);
        assert!(!steps.is_empty());
        let rms = -28.0 + g;
        assert!((-22.0..=-14.0).contains(&rms), "data {rms} dBFS in the band; {steps:?}");
        // Silence alone never moves the gain.
        let (g, steps) = simulate(None, 0.0, 0.0, 200.0, 0.0);
        assert!(steps.is_empty() && g == 0.0);
    }

    #[test]
    fn down_steps_keep_the_data_in_the_window() {
        // Loud noise but weak data (frames seen from the start): stepping down
        // would push the data under -30.
        let (g, steps) = simulate(Some(-13.0), -29.0, 0.0, 120.0, 1.0);
        assert!(steps.is_empty(), "{steps:?} -> {g}");
    }

    #[test]
    fn at_the_end_of_the_range_nothing_happens() {
        let (g, steps) = simulate(Some(-40.0), -30.0, 12.0, 60.0, 0.0);
        assert!(steps.is_empty());
        assert_eq!(g, 12.0);
    }

    #[test]
    fn measurements_from_before_a_step_are_discarded() {
        let mut agc = RxAgc::new(AgcConfig::default());
        agc.observe(idle(10.0, -12.0));
        agc.observe(idle(10.5, -12.0));
        let s = agc.decide(10.6, true, 0.0, RANGE).unwrap();
        assert_eq!(s.to_db, -3.0);
        // Windows inside the settle second do not count.
        agc.observe(idle(11.0, -15.0));
        agc.observe(idle(11.5, -15.0));
        assert!(agc.noise_dbfs().is_none());
        // Even confirmed later, the 10 s interval holds.
        agc.observe(idle(12.0, -15.0));
        agc.observe(idle(12.5, -15.0));
        assert!(agc.decide(12.6, true, -3.0, RANGE).is_none());
        assert!(agc.decide(20.7, true, -3.0, RANGE).is_some());
        assert_eq!(agc.changes, 2);
    }
}
