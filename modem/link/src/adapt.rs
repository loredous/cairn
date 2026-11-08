//! Link adaptation (SPEC §6.2, §10.1, §10.3; ADR 0010
//! `adaptation-rules-revised`, ADR 0011 `mask-id`,
//! `mask-in-every-session-ack`): the receiver's choice of requested rung,
//! and its tone-mask tracker with mask IDs.

use std::time::Duration;

use crate::params::{
    ALL_TONES, DISTORTION_EXCLUDE_INDEX, MASK_DROP_DB, MASK_DROP_FRAMES, MASK_ID_MODULUS, READING_REPROBE_INTERVAL,
    REPLY_RULE_MARGIN_DB, RUNG_THRESHOLDS_DB, RUNGS, STEP_DOWN_FRACTION, STEP_DOWN_TWO_FRACTION, STEP_UP_MARGIN_DB,
    STEP_UP_RUN, UNMASKABLE_TONES,
};
use crate::time::Instant;

/// Per-rung switching thresholds: §6.2 plus a margin (§10.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds(pub [f64; RUNGS]);

impl Thresholds {
    /// §6.2 thresholds plus `margin_db`.
    pub fn with_margin(margin_db: f64) -> Self {
        let mut t = RUNG_THRESHOLDS_DB;
        for v in &mut t {
            *v += margin_db;
        }
        Thresholds(t)
    }

    /// The highest rung whose threshold is at least `REPLY_RULE_MARGIN_DB`
    /// below `snr_db` (0 if none): the rule for a reply to CONNECT,
    /// CONNECT-ACK or SOUND and the link-test suggestion.
    pub fn reply_rule(&self, snr_db: f64) -> u8 {
        (0..RUNGS).rev().find(|&m| self.0[m] <= snr_db - REPLY_RULE_MARGIN_DB).unwrap_or(0) as u8
    }
}

/// One received frame's statistics for the adaptation rules.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    /// Data blocks the frame carried.
    pub data: usize,
    /// Data blocks that failed.
    pub failed: usize,
    /// Measured SNR (dB).
    pub snr_db: f64,
    /// Distortion index measured here.
    pub distortion: u8,
    /// This receiver's ADC clipped.
    pub adc_clipped: bool,
}

/// What a frame did to the requested rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Unchanged.
    Hold,
    /// Stepped down this many rungs.
    Down(u8),
    /// Stepped up one rung.
    Up,
    /// Held because this receiver clipped: warn the operator.
    ClippedHold,
}

/// The receiver's rung choice for one direction (§10.1 as revised by ADR 0010).
#[derive(Clone, Debug, PartialEq)]
pub struct RateAdapter {
    thr: Thresholds,
    requested: u8,
    clean_run: u8,
}

impl RateAdapter {
    /// An adapter starting at `initial`.
    pub fn new(thr: Thresholds, initial: u8) -> Self {
        RateAdapter { thr, requested: initial.min(RUNGS as u8 - 1), clean_run: 0 }
    }

    /// The rung to request.
    pub fn requested(&self) -> u8 {
        self.requested
    }

    /// The thresholds in use.
    pub fn thresholds(&self) -> &Thresholds {
        &self.thr
    }

    /// This station is replying to a CONNECT, CONNECT-ACK or SOUND measured
    /// at `snr_db`: request by the 3 dB rule.
    pub fn on_reply_rule(&mut self, snr_db: f64) {
        self.requested = self.thr.reply_rule(snr_db);
        self.clean_run = 0;
    }

    fn down(&mut self, n: u8) -> Decision {
        let before = self.requested;
        self.requested = self.requested.saturating_sub(n);
        self.clean_run = 0;
        if before == self.requested { Decision::Hold } else { Decision::Down(before - self.requested) }
    }

    /// A preamble while awaiting the peer, then a header that failed.
    pub fn on_lost_header(&mut self) -> Decision {
        self.down(1)
    }

    /// A frame from the peer whose header decoded.
    pub fn on_frame(&mut self, s: &FrameStats) -> Decision {
        if s.adc_clipped {
            // Hold and warn; the run restarts (READING: a clipped frame's
            // measurements are unreliable, so it does not count toward a step up).
            self.clean_run = 0;
            return Decision::ClippedHold;
        }
        if s.data == 0 {
            return Decision::Hold; // neither counts toward the run nor breaks it
        }
        let frac = s.failed as f64 / s.data as f64;
        if s.distortion >= DISTORTION_EXCLUDE_INDEX {
            // Excluded from the step-down count; READING: nor does it count
            // toward a step up, since the peer is expected to change its drive.
            self.clean_run = 0;
            return Decision::Hold;
        }
        if frac > STEP_DOWN_TWO_FRACTION {
            return self.down(2);
        }
        if frac > STEP_DOWN_FRACTION {
            return self.down(1);
        }
        if s.failed > 0 {
            self.clean_run = 0;
            return Decision::Hold;
        }
        let next = self.requested as usize + 1;
        // READING: every frame of the run must meet the SNR condition.
        if next >= RUNGS || s.snr_db < self.thr.0[next] + STEP_UP_MARGIN_DB {
            self.clean_run = 0;
            return Decision::Hold;
        }
        self.clean_run += 1;
        if self.clean_run >= STEP_UP_RUN {
            self.requested += 1;
            self.clean_run = 0;
            return Decision::Up;
        }
        Decision::Hold
    }
}

/// The receiver's tone mask for its direction, with mask IDs (ADR 0011).
#[derive(Clone, Debug, PartialEq)]
pub struct MaskTracker {
    current: (u64, u8),
    pending: Option<(u64, u8)>,
    below: [u8; 56],
    last_probe: Option<Instant>,
    width: u8,
    enabled: bool,
}

/// Tones a mask may drop: below the width, not pilots 3 and 20.
fn maskable(t: u8, width: u8) -> bool {
    t < width && !UNMASKABLE_TONES.contains(&t)
}

impl MaskTracker {
    /// A tracker for a direction whose sender transmits `width` tones.
    /// Disabled, it reports the all-ones mask with ID 0 throughout, which a
    /// v0.5 station may do (ADR 0011 `v0-5-mask-obligations`).
    pub fn new(width: u8, enabled: bool) -> Self {
        MaskTracker { current: (ALL_TONES, 0), pending: None, below: [0; 56], last_probe: None, width, enabled }
    }

    /// The setup mask (ID 0) from one measurement: tones more than 6 dB
    /// below the average are unusable.
    pub fn setup(&mut self, tone_snr: Option<&[f64]>, now: Instant) {
        self.current = (ALL_TONES, 0);
        self.pending = None;
        self.below = [0; 56];
        self.last_probe = Some(now);
        if let (true, Some(snr)) = (self.enabled, tone_snr) {
            let avg = average(snr, ALL_TONES, self.width);
            for (t, &v) in snr.iter().enumerate().take(56) {
                if maskable(t as u8, self.width) && v < avg - MASK_DROP_DB {
                    self.current.0 &= !(1u64 << t);
                }
            }
        }
    }

    /// The mask and ID to report in every ACK block: the pending change
    /// until the sender confirms it.
    pub fn reported(&self) -> (u64, u8) {
        self.pending.unwrap_or(self.current)
    }

    /// The mask a header's Mask ID names, if it is the current or pending
    /// one; `None` means the frame's control and data blocks count as failed.
    pub fn mask_for_id(&self, id: u8) -> Option<u64> {
        if self.current.1 == id { Some(self.current.0) } else { self.pending.filter(|p| p.1 == id).map(|p| p.0) }
    }

    /// A header from the sender named `id`: it confirms a pending change.
    pub fn on_header(&mut self, id: u8) {
        if let Some(p) = self.pending {
            if p.1 == id {
                self.current = p;
                self.pending = None;
            }
        }
    }

    /// A decoded frame's per-tone SNR. Base tones are measured from the
    /// preamble; tones above 23 only when the frame carried a probe.
    pub fn on_measurement(&mut self, tone_snr: Option<&[f64]>, probe: bool, now: Instant) {
        if probe {
            self.last_probe = Some(now);
        }
        let (true, Some(snr)) = (self.enabled, tone_snr) else { return };
        let (mask, _) = self.reported();
        let avg = average(snr, mask, self.width);
        let mut drop = 0u64;
        let mut restore = 0u64;
        for (t, &v) in snr.iter().enumerate().take(56) {
            let t8 = t as u8;
            if !maskable(t8, self.width) || (t8 >= 24 && !probe) {
                continue;
            }
            let active = mask >> t & 1 == 1;
            if active {
                if v < avg - MASK_DROP_DB {
                    self.below[t] = self.below[t].saturating_add(1);
                    if self.below[t] >= MASK_DROP_FRAMES {
                        drop |= 1 << t;
                    }
                } else {
                    self.below[t] = 0;
                }
            } else if probe && v >= avg - MASK_DROP_DB {
                // Restored only after a width probe shows it recovered (§10.3).
                restore |= 1 << t;
            }
        }
        if self.pending.is_none() && (drop | restore) != 0 {
            let new = (self.current.0 & !drop) | restore;
            if new != self.current.0 {
                self.pending = Some((new, (self.current.1 + 1) % MASK_ID_MODULUS));
                for t in 0..56 {
                    if (drop | restore) >> t & 1 == 1 {
                        self.below[t] = 0;
                    }
                }
            }
        }
    }

    /// Ask for a re-probe (ACK flag bit 2): a tone is dropped and no probe
    /// has been seen for [`READING_REPROBE_INTERVAL`].
    pub fn wants_reprobe(&self, now: Instant) -> bool {
        let (mask, _) = self.reported();
        let any_dropped = (0..self.width.min(56)).any(|t| maskable(t, self.width) && mask >> t & 1 == 0);
        any_dropped && self.last_probe.is_none_or(|p| now.saturating_since(p) >= READING_REPROBE_INTERVAL)
    }

    /// When a re-probe will next be wanted, if a tone is dropped.
    pub fn reprobe_due(&self) -> Option<Instant> {
        let (mask, _) = self.reported();
        let any_dropped = (0..self.width.min(56)).any(|t| maskable(t, self.width) && mask >> t & 1 == 0);
        if any_dropped { self.last_probe.map(|p| p + READING_REPROBE_INTERVAL) } else { None }
    }

    /// Interval constant re-exported for callers.
    pub const REPROBE_INTERVAL: Duration = READING_REPROBE_INTERVAL;
}

fn average(snr: &[f64], mask: u64, width: u8) -> f64 {
    let v: Vec<f64> =
        snr.iter().enumerate().take(width as usize).filter(|(t, _)| mask >> t & 1 == 1).map(|(_, &v)| v).collect();
    if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(data: usize, failed: usize, snr: f64) -> FrameStats {
        FrameStats { data, failed, snr_db: snr, distortion: 0, adc_clipped: false }
    }

    #[test]
    fn reply_rule_three_db() {
        let t = Thresholds::with_margin(1.0);
        // MCS 3 threshold 13.5: needs 16.5 dB.
        assert_eq!(t.reply_rule(16.5), 3);
        assert_eq!(t.reply_rule(16.4), 2);
        assert_eq!(t.reply_rule(0.0), 0);
        assert_eq!(t.reply_rule(60.0), 6);
    }

    #[test]
    fn step_rules() {
        let mut a = RateAdapter::new(Thresholds::with_margin(1.0), 4);
        assert_eq!(a.on_frame(&stats(10, 3, 20.0)), Decision::Down(1));
        assert_eq!(a.on_frame(&stats(10, 6, 20.0)), Decision::Down(2));
        assert_eq!(a.requested(), 1);
        assert_eq!(a.on_frame(&stats(10, 2, 30.0)), Decision::Hold);
        // MCS 2 threshold 9.5: needs 11.5 dB for three clean frames.
        assert_eq!(a.on_frame(&stats(10, 0, 12.0)), Decision::Hold);
        assert_eq!(a.on_frame(&stats(0, 0, 12.0)), Decision::Hold); // no data: neither counts nor breaks
        assert_eq!(a.on_frame(&stats(10, 0, 12.0)), Decision::Hold);
        assert_eq!(a.on_frame(&stats(10, 0, 12.0)), Decision::Up);
        assert_eq!(a.requested(), 2);
        let mut d = stats(10, 9, 5.0);
        d.distortion = 3;
        assert_eq!(a.on_frame(&d), Decision::Hold);
        assert_eq!(a.on_lost_header(), Decision::Down(1));
        let mut c = stats(10, 9, 5.0);
        c.adc_clipped = true;
        assert_eq!(a.on_frame(&c), Decision::ClippedHold);
        assert_eq!(a.requested(), 1);
    }

    #[test]
    fn mask_one_outstanding_change() {
        let mut m = MaskTracker::new(24, true);
        m.setup(Some(&[20.0; 24]), Instant::ZERO);
        assert_eq!(m.reported(), (ALL_TONES, 0));
        let mut snr = [20.0; 24];
        snr[7] = 5.0;
        m.on_measurement(Some(&snr), false, Instant::ZERO);
        assert_eq!(m.reported().1, 0, "one frame is not enough");
        m.on_measurement(Some(&snr), false, Instant::ZERO);
        let (mask, id) = m.reported();
        assert_eq!(id, 1);
        assert_eq!(mask & (1 << 7), 0);
        // A second change waits for confirmation.
        snr[9] = 5.0;
        m.on_measurement(Some(&snr), false, Instant::ZERO);
        m.on_measurement(Some(&snr), false, Instant::ZERO);
        assert_eq!(m.reported(), (mask, 1));
        assert_eq!(m.mask_for_id(0), Some(ALL_TONES));
        assert_eq!(m.mask_for_id(1), Some(mask));
        assert_eq!(m.mask_for_id(2), None);
        m.on_header(1);
        assert_eq!(m.mask_for_id(0), None);
        // Pilots 3 and 20 are never dropped.
        let mut p = [20.0; 24];
        p[3] = 0.0;
        let mut m2 = MaskTracker::new(24, true);
        m2.on_measurement(Some(&p), false, Instant::ZERO);
        m2.on_measurement(Some(&p), false, Instant::ZERO);
        assert_eq!(m2.reported(), (ALL_TONES, 0));
        // Restored only with a probe.
        snr = [20.0; 24];
        m.on_measurement(Some(&snr), false, Instant::ZERO);
        assert_eq!(m.reported().1, 1);
        assert!(m.wants_reprobe(Instant::from_secs_f64(11.0)));
        m.on_measurement(Some(&snr), true, Instant::from_secs_f64(11.0));
        assert_eq!(m.reported(), (ALL_TONES, 2));
    }
}
