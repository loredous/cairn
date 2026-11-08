//! The PROBE link test and BEACON request (SPEC §9.6; ADR 0012).

use std::time::Duration;

use crate::adapt::Thresholds;
use crate::capacity::{FrameModel, expected_net_kbps};
use crate::frame::{
    AckBlock, Address, ConnectBlock, ControlBlock, Flags, FrameType, LinkFrame, Measurement, RxFrame, TxParams,
    snr_from_field, snr_to_field,
};
use crate::params::{ALL_TONES, BACKOFF_MAX, BACKOFF_MIN, PROBE_REPLY_TIMEOUT, WIDTH_CAP_MIN, rung_frame_limit};
use crate::rng::Rng;
use crate::session::frame_time_units;
use crate::station::Config;
use crate::time::Instant;

/// What a PROBE asks for (ADR 0012 `probe-frame-contents`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeKind {
    /// A link test: one B.3 block; the target answers with a SOUND.
    LinkTest,
    /// A BEACON request: no control blocks; the target answers with a BEACON.
    Beacon,
}

/// One direction's link-test figures.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectionResult {
    /// Measured SNR (dB).
    pub snr_db: f64,
    /// Suggested rung (3 dB rule).
    pub mcs: u8,
    /// Expected net payload rate (kbps), as the host presents it (§9.6).
    pub net_kbps: f64,
    /// Usable tone mask.
    pub mask: u64,
    /// Distortion index seen by the receiving end.
    pub distortion: u8,
}

/// Both directions of a link test.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkTestResult {
    /// The station probed.
    pub target: Address,
    /// This station to the target (measured by the target).
    pub forward: DirectionResult,
    /// The target to this station (measured here on the SOUND).
    pub reverse: DirectionResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready(Instant),
    Awaiting(Instant),
}

/// An outstanding PROBE.
#[derive(Clone, Debug)]
pub struct Prober {
    target: Address,
    kind: ProbeKind,
    repeats_left: u8,
    state: State,
}

impl Prober {
    /// A probe to send now, repeated up to `repeats` times when unanswered.
    pub fn new(target: Address, kind: ProbeKind, repeats: u8, now: Instant) -> Self {
        Prober { target, kind, repeats_left: repeats, state: State::Ready(now) }
    }

    /// The target.
    pub fn target(&self) -> Address {
        self.target
    }

    /// The kind.
    pub fn kind(&self) -> ProbeKind {
        self.kind
    }

    /// Ready to send from this time.
    pub fn ready_at(&self) -> Option<Instant> {
        match self.state {
            State::Ready(t) => Some(t),
            State::Awaiting(_) => None,
        }
    }

    /// The reply deadline while awaiting.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Awaiting(t) => Some(t),
            State::Ready(_) => None,
        }
    }

    /// Build the PROBE with the next frame counter.
    pub fn build(&self, cfg: &Config, counter: u16) -> LinkFrame {
        let (controls, probe) = match self.kind {
            ProbeKind::LinkTest => (
                vec![ControlBlock::Connect(ConnectBlock {
                    capabilities: cfg.capabilities & !(crate::frame::caps::HARQ | crate::frame::caps::STREAM_MUX),
                    width_cap: cfg.width_cap,
                    max_frame_time: frame_time_units(cfg.max_frame_time),
                    mask: ALL_TONES,
                    ..Default::default()
                })],
                cfg.width_cap > WIDTH_CAP_MIN,
            ),
            ProbeKind::Beacon => (Vec::new(), false),
        };
        LinkFrame {
            ftype: FrameType::Probe,
            src: cfg.address,
            dst: self.target,
            flags: Flags { probe, ..Default::default() },
            seq: crate::seq::Seq(counter),
            mcs: 0,
            mask_id: 0,
            controls,
            data: Vec::new(),
            tx: TxParams { width: cfg.width_cap + 1, mask: ALL_TONES, long_prefix: cfg.repeater },
        }
    }

    /// PTT released after the PROBE.
    pub fn on_tx_done(&mut self, now: Instant) {
        self.state = State::Awaiting(now + PROBE_REPLY_TIMEOUT);
    }

    /// The deadline passed: `true` when the target is unreachable (no
    /// repeats left), otherwise a repeat waits a 1-5 s backoff.
    pub fn on_timeout(&mut self, now: Instant, rng: &mut Rng) -> bool {
        if self.repeats_left == 0 {
            return true;
        }
        self.repeats_left -= 1;
        self.state = State::Ready(now + rng.duration(BACKOFF_MIN, BACKOFF_MAX));
        false
    }

    /// The target's SOUND reply: both directions' figures.
    pub fn on_sound(&self, f: &RxFrame, cfg: &Config, model: &dyn FrameModel) -> Option<LinkTestResult> {
        if self.kind != ProbeKind::LinkTest || self.deadline().is_none() {
            return None;
        }
        let a = f.ack()?;
        let thr = Thresholds::with_margin(cfg.threshold_margin_db);
        let limit = |mcs: u8| cfg.max_frame_time.min(rung_frame_limit(mcs));
        let width = 24u8;
        let fwd_mcs = a.requested_mcs.min(6);
        let rev_snr = f.meas.snr_db;
        let rev_mcs = thr.reply_rule(rev_snr);
        let rev_mask = mask_from(&f.meas, width);
        Some(LinkTestResult {
            target: self.target,
            forward: DirectionResult {
                snr_db: snr_from_field(a.snr),
                mcs: fwd_mcs,
                net_kbps: expected_net_kbps(model, fwd_mcs, width, a.mask, limit(fwd_mcs)),
                mask: a.mask,
                distortion: a.distortion,
            },
            reverse: DirectionResult {
                snr_db: rev_snr,
                mcs: rev_mcs,
                net_kbps: expected_net_kbps(model, rev_mcs, width, rev_mask, limit(rev_mcs)),
                mask: rev_mask,
                distortion: f.meas.distortion,
            },
        })
    }
}

fn mask_from(meas: &Measurement, width: u8) -> u64 {
    let mut m = crate::adapt::MaskTracker::new(width, true);
    m.setup(meas.tone_snr_db.as_deref(), Instant::ZERO);
    m.reported().0
}

/// The target's SOUND reply to a link-test PROBE (ADR 0012
/// `probe-link-test-reply`): Sequence 0, a width probe to its own cap, one
/// ACK block with its measurements.
pub fn sound_reply(cfg: &Config, to: Address, meas: &Measurement) -> LinkFrame {
    let thr = Thresholds::with_margin(cfg.threshold_margin_db);
    let mask = if cfg.track_masks { mask_from(meas, 24) } else { ALL_TONES };
    LinkFrame {
        ftype: FrameType::Sound,
        src: cfg.address,
        dst: to,
        flags: Flags { probe: true, ..Default::default() },
        seq: crate::seq::Seq(0),
        mcs: 0,
        mask_id: 0,
        controls: vec![ControlBlock::Ack(AckBlock {
            mask_present: true,
            requested_mcs: thr.reply_rule(meas.snr_db),
            snr: snr_to_field(meas.snr_db),
            mask,
            distortion: meas.distortion.min(7),
            adc_clipped: meas.adc_clipped,
            ..Default::default()
        })],
        data: Vec::new(),
        tx: TxParams { width: cfg.width_cap + 1, mask: ALL_TONES, long_prefix: cfg.repeater },
    }
}

/// Air time of the two link-test transmissions (for checking the 3 s target).
pub fn exchange_air_time(model: &dyn FrameModel, probe: &LinkFrame, reply: &LinkFrame) -> Duration {
    let shape = |f: &LinkFrame| crate::capacity::FrameShape {
        mcs: f.mcs,
        width: f.tx.width,
        mask: f.tx.mask,
        controls: f.controls.len(),
        data: f.data.len(),
        probe: f.flags.probe,
        long_prefix: f.tx.long_prefix,
    };
    model.on_air_time(&shape(probe)) + model.on_air_time(&shape(reply))
}
