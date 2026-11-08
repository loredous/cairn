//! The session state machine (SPEC §9.2, §9.3, §9.5; ADR 0010): setup with
//! retries and refusal, reply-required frames and turn-taking, the ACK timer
//! and its escalation, keepalives, idle disconnect, the DISCONNECT
//! handshake, the yield clock and the ID cadence, with the ARQ (ADR 0008)
//! and adaptation (ADR 0010, ADR 0011) state of one session.

use std::time::Duration;

use crate::access::{Mode, Trigger};
use crate::adapt::{Decision, FrameStats, MaskTracker, RateAdapter, Thresholds};
use crate::arq::{Receiver, Sender};
use crate::capacity::{FrameModel, FrameShape};
use crate::frame::{
    AckBlock, Address, ConnectBlock, ControlBlock, Flags, FrameType, LinkFrame, Measurement, RxFrame, TxParams, caps,
    snr_to_field,
};
use crate::params::{
    ACK_EXPIRY_LAST_RESEND, ACK_EXPIRY_LAST_SOUND, ACK_EXPIRY_LINK_LOST, ACK_TIMER_BASE, ACK_TIMER_D_REPEATER_EXTRA,
    ACK_TIMER_D_SIMPLEX, ACK_TIMER_JITTER_MAX, ACK_TIMER_MARGIN, ACK_TIMER_RESPONDER_OFFSET, ALL_TONES, BACKOFF_MAX,
    BACKOFF_MIN, BASE_WIDTH, CONNECT_ACK_MAX_RESENDS, CONNECT_MAX_RETRIES, CONNECT_WAIT, CONNECT_WAIT_REPEATER,
    DISCONNECT_MAX_RESENDS, DISTORTION_EXCLUDE_INDEX, FRAME_TIME_UNIT, ID_INTERVAL, KEEPALIVE_INITIATOR,
    KEEPALIVE_RESPONDER, KEEPALIVE_RESPONDER_JITTER_MAX, LEGACY_FRAME_CAP, YIELD_INTERVAL, YIELD_WINDOW,
    YIELD_WINDOW_REPEATER, rung_frame_limit,
};
use crate::resume::Retained;
use crate::rng::Rng;
use crate::station::{Action, Config, ConnectFailure, HostEvent, Warning};
use crate::time::{Instant, PausableTimer};

/// A station's role in a session's setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Sent the CONNECT.
    Initiator,
    /// Answered it.
    Responder,
}

/// Where a session is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Initiator before CONNECT-ACK: `attempt` 0 is the first CONNECT,
    /// 1-3 the retries.
    Connecting {
        /// Attempt number.
        attempt: u8,
        /// Waiting to send (from this time) or awaiting the answer (until it).
        awaiting: Option<Instant>,
        /// Earliest time to (re)send.
        not_before: Instant,
    },
    /// Responder after CONNECT, before the initiator's first frame.
    HalfOpen {
        /// CONNECT-ACK resends on ACK-timer expiry so far.
        resends: u8,
    },
    /// Established.
    Open,
    /// This station sent DISCONNECT and awaits the reply.
    Closing {
        /// Resends so far.
        resends: u8,
    },
}

/// The session transmissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessTx {
    /// CONNECT (carrier sense).
    Connect,
    /// CONNECT-ACK (turn-holder).
    ConnectAck,
    /// The initiator's first frame (turn-holder).
    FirstFrame,
    /// A reply to a frame that requires one, or to an ACK frame when there
    /// is data to send (turn-holder).
    Reply,
    /// A resend on the 1st or 2nd ACK-timer expiry (turn-holder).
    Resend,
    /// SOUND on the 3rd-7th expiry (turn-holder).
    Sound,
    /// Keepalive SOUND from idle (carrier sense).
    Keepalive,
    /// New data from idle (carrier sense).
    FromIdle,
    /// This station's DISCONNECT.
    Disconnect,
    /// The single DISCONNECT that answers the peer's.
    DisconnectReply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owed {
    what: SessTx,
    trigger: Trigger,
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Closed by the DISCONNECT handshake (or after the resends ran out).
    /// `linger` holds the reply to repeat when this station replied.
    Closed {
        /// The DISCONNECT this station replied with, if it replied.
        linger: Option<LinkFrame>,
    },
    /// The 8th ACK-timer expiry.
    LinkLost,
    /// Setup failed.
    ConnectFailed(ConnectFailure),
}

/// Everything a session method needs from the station.
pub(crate) struct Ctx<'a> {
    pub cfg: &'a Config,
    pub rng: &'a mut Rng,
    pub model: &'a dyn FrameModel,
    pub now: Instant,
    pub dcd_held: bool,
    pub legacy: bool,
    pub last_id: &'a mut Option<Instant>,
    pub out: &'a mut Vec<Action>,
}

impl Ctx<'_> {
    fn host(&mut self, ev: HostEvent) {
        self.out.push(Action::Host(ev));
    }

    /// An ID block is due on the next session frame (ADR 0010 `session-id-cadence`).
    pub fn id_due(&self) -> bool {
        self.last_id.is_none_or(|t| self.now.saturating_since(t) >= ID_INTERVAL)
    }
}

/// B.3 max frame time units for a duration.
pub(crate) fn frame_time_units(d: Duration) -> u8 {
    (d.as_millis() / FRAME_TIME_UNIT.as_millis()).clamp(5, 40) as u8
}

/// One session (live, or being set up).
#[derive(Clone, Debug)]
pub struct Session {
    pub(crate) peer: Address,
    pub(crate) role: Role,
    pub(crate) session_id: u16,
    pub(crate) phase: Phase,
    /// Initiator: the CONNECT asks to resume. Responder: the CONNECT-ACK echoes a resume.
    pub(crate) resume: bool,
    /// The session existed before this setup (resume of retained or live state).
    pub(crate) had_state: bool,
    pub(crate) peer_caps: u16,
    pub(crate) peer_width_cap: u8,
    pub(crate) peer_max_frame: Duration,
    pub(crate) tx: Sender,
    pub(crate) rx: Receiver,
    rate: RateAdapter,
    mask: MaskTracker,
    tx_mcs: u8,
    tx_mask: (u64, u8),
    tx_long_prefix: bool,
    peer_reprobe: bool,
    owed: Option<Owed>,
    ack_timer: Option<PausableTimer>,
    expiries: u8,
    last_ack: Option<AckBlock>,
    fresh: bool,
    last_meas: Measurement,
    last_block_errors: u8,
    last_sent: FrameType,
    replying_to_sound: bool,
    idle: PausableTimer,
    pub(crate) yield_start: Option<Instant>,
    last_data: Instant,
    disconnect: Option<bool>,
    peer_distortion: u8,
    /// The peer's latest ACK block: its report of this station's frames.
    peer_ack: Option<AckBlock>,
    last_frame_from_peer: Option<Instant>,
    /// When the link was lost, for state resumed from the retained store.
    pub(crate) lost_at: Option<Instant>,
}

impl Session {
    fn base(cfg: &Config, peer: Address, role: Role, session_id: u16, tx: Sender, rx: Receiver, now: Instant) -> Self {
        Session {
            peer,
            role,
            session_id,
            phase: Phase::Open,
            resume: false,
            had_state: false,
            peer_caps: 0,
            peer_width_cap: BASE_WIDTH - 1,
            peer_max_frame: cfg.max_frame_time,
            tx,
            rx,
            rate: RateAdapter::new(Thresholds::with_margin(cfg.threshold_margin_db), 0),
            mask: MaskTracker::new(BASE_WIDTH, cfg.track_masks),
            tx_mcs: 0,
            tx_mask: (ALL_TONES, 0),
            tx_long_prefix: cfg.repeater,
            peer_reprobe: false,
            owed: None,
            ack_timer: None,
            expiries: 0,
            last_ack: None,
            fresh: true,
            last_meas: Measurement::default(),
            last_block_errors: 0,
            last_sent: FrameType::Ack,
            replying_to_sound: false,
            idle: PausableTimer::start(KEEPALIVE_INITIATOR, now, false),
            yield_start: None,
            last_data: now,
            disconnect: None,
            peer_distortion: 0,
            peer_ack: None,
            last_frame_from_peer: None,
            lost_at: None,
        }
    }

    /// An initiator, fresh or resuming retained state.
    pub(crate) fn new_initiator(peer: Address, retained: Option<Retained>, ctx: &mut Ctx) -> Session {
        let now = ctx.now;
        let init = ctx.cfg.initial_sequence as u64;
        let mut s = match retained {
            Some(r) => {
                // ADR 0013 `resume-keeps-roles`: the original setup's roles.
                let mut s = Session::base(ctx.cfg, peer, r.role, r.session_id, r.tx, r.rx, now);
                s.resume = true;
                s.had_state = true;
                s.peer_caps = r.peer_caps;
                s.peer_width_cap = r.peer_width_cap;
                s.peer_max_frame = r.peer_max_frame;
                s.lost_at = Some(r.lost_at);
                s
            }
            None => {
                let id = ctx.rng.u16();
                Session::base(ctx.cfg, peer, Role::Initiator, id, Sender::new(init), Receiver::new(init), now)
            }
        };
        s.phase = Phase::Connecting { attempt: 0, awaiting: None, not_before: now };
        s
    }

    /// A responder to a CONNECT, fresh or resuming (`state`).
    pub(crate) fn new_responder(f: &RxFrame, b3: &ConnectBlock, state: Option<Retained>, ctx: &mut Ctx) -> Session {
        let init = ctx.cfg.initial_sequence as u64;
        let mut s = match state {
            Some(r) => {
                // ADR 0013 `resume-keeps-roles`: the original setup's roles.
                let mut s = Session::base(ctx.cfg, f.header.src, r.role, r.session_id, r.tx, r.rx, ctx.now);
                s.resume = true;
                s.had_state = true;
                s.peer_caps = r.peer_caps;
                s.peer_width_cap = r.peer_width_cap;
                s.peer_max_frame = r.peer_max_frame;
                s.lost_at = Some(r.lost_at);
                s
            }
            None => {
                let mut s = Session::base(
                    ctx.cfg,
                    f.header.src,
                    Role::Responder,
                    b3.session_id,
                    Sender::new(init),
                    Receiver::new(init),
                    ctx.now,
                );
                s.peer_caps = b3.capabilities;
                s.peer_width_cap = b3.width_cap;
                s.peer_max_frame = b3.max_frame_duration();
                s
            }
        };
        s.answer_connect(f, ctx);
        s
    }

    /// (Re-)answer a CONNECT: measure it, owe CONNECT-ACK.
    pub(crate) fn answer_connect(&mut self, f: &RxFrame, ctx: &mut Ctx) {
        self.rate.on_reply_rule(f.meas.snr_db);
        self.mask = MaskTracker::new(self.rx_width(ctx.cfg), ctx.cfg.track_masks);
        self.mask.setup(f.meas.tone_snr_db.as_deref(), ctx.now);
        self.tx_mask = (ALL_TONES, 0);
        self.tx_mcs = 0;
        self.tx_long_prefix = ctx.cfg.repeater;
        self.peer_reprobe = false;
        self.last_meas = f.meas.clone();
        self.last_block_errors = 0;
        self.last_ack = None;
        self.fresh = true;
        self.ack_timer = None;
        self.expiries = 0;
        self.yield_start = None;
        if !matches!(self.phase, Phase::HalfOpen { .. }) {
            self.phase = Phase::HalfOpen { resends: 0 };
        }
        self.owed = Some(Owed { what: SessTx::ConnectAck, trigger: Trigger::PeerFrame(f.ended_at) });
    }

    /// Accept a resume CONNECT for this live session (ADR 0008
    /// `resume-by-either-station`): keep sequence state, re-measure.
    pub(crate) fn accept_resume(&mut self, f: &RxFrame, ctx: &mut Ctx) {
        self.resume = true;
        self.had_state = true;
        self.phase = Phase::HalfOpen { resends: 0 };
        self.answer_connect(f, ctx);
    }

    /// The peer.
    pub fn peer(&self) -> Address {
        self.peer
    }

    /// The session ID.
    pub fn session_id(&self) -> u16 {
        self.session_id
    }

    /// The phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// This station's role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The rung this station requests of its peer.
    pub fn requested_mcs(&self) -> u8 {
        self.rate.requested()
    }

    /// The rung the peer asked this station to send at.
    pub fn tx_mcs(&self) -> u8 {
        self.tx_mcs
    }

    /// The mask (and ID) this station applies to what it sends.
    pub fn tx_mask(&self) -> (u64, u8) {
        self.tx_mask
    }

    /// The mask (and ID) this station reports for its receive direction.
    pub fn rx_mask(&self) -> (u64, u8) {
        self.mask.reported()
    }

    /// The mask a header's Mask ID names, for demapping.
    pub fn rx_mask_for(&self, id: u8) -> Option<u64> {
        self.mask.mask_for_id(id)
    }

    /// ACK-timer expirations since the last decoded peer header.
    pub fn expiries(&self) -> u8 {
        self.expiries
    }

    /// Neither station owes a reply (ADR 0010 `reply-required-frames`).
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Open && self.owed.is_none() && self.ack_timer.is_none()
    }

    /// ADR 0011 `session-width`: cap + 1 when the peer advertised
    /// capability bit 0, else 24 (a station without the bit advertises cap 23).
    pub(crate) fn tx_width(&self, cfg: &Config) -> u8 {
        if self.peer_caps & caps::WIDE != 0 { cfg.width_cap + 1 } else { BASE_WIDTH }
    }

    /// The peer's session width: its cap + 1 when this station advertised
    /// capability bit 0.
    pub(crate) fn rx_width(&self, cfg: &Config) -> u8 {
        if cfg.capabilities & caps::WIDE != 0 { self.peer_width_cap + 1 } else { BASE_WIDTH }
    }

    /// The yield window is due before the next transmission.
    pub(crate) fn yield_due(&self, now: Instant) -> bool {
        matches!(self.phase, Phase::Open | Phase::Closing { .. })
            && self.yield_start.is_some_and(|s| now.saturating_since(s) >= YIELD_INTERVAL)
    }

    /// The on-air limit of this station's frames at a rung (§7.3, §10.2,
    /// §11.3, B.3).
    fn tx_limit(&self, ctx: &Ctx, mcs: u8) -> Duration {
        let mut l = ctx.cfg.max_frame_time.min(self.peer_max_frame).min(rung_frame_limit(mcs));
        if ctx.legacy {
            l = l.min(LEGACY_FRAME_CAP);
        }
        l
    }

    /// The ACK timer length (ADR 0010 `ack-timer-rules`).
    fn ack_timeout(&self, ctx: &mut Ctx) -> Duration {
        let cfg = ctx.cfg;
        let d = ACK_TIMER_D_SIMPLEX + if cfg.repeater { ACK_TIMER_D_REPEATER_EXTRA } else { Duration::ZERO };
        let y = if cfg.repeater { YIELD_WINDOW_REPEATER } else { YIELD_WINDOW };
        let f = cfg.max_frame_time.min(self.peer_max_frame).min(rung_frame_limit(self.rate.requested()));
        let offset = if self.role == Role::Responder { ACK_TIMER_RESPONDER_OFFSET } else { Duration::ZERO };
        ACK_TIMER_BASE + d + y + f + ACK_TIMER_MARGIN + offset + ctx.rng.duration(Duration::ZERO, ACK_TIMER_JITTER_MAX)
    }

    fn start_ack_timer(&mut self, ctx: &mut Ctx) {
        let t = self.ack_timeout(ctx);
        self.ack_timer = Some(PausableTimer::start(t, ctx.now, ctx.dcd_held));
    }

    fn restart_idle(&mut self, ctx: &mut Ctx) {
        let total = match self.role {
            Role::Initiator => KEEPALIVE_INITIATOR,
            Role::Responder => KEEPALIVE_RESPONDER + ctx.rng.duration(Duration::ZERO, KEEPALIVE_RESPONDER_JITTER_MAX),
        };
        self.idle = PausableTimer::start(total, ctx.now, ctx.dcd_held);
    }

    /// Held DCD changed: pause or resume the ACK timer and the idle clock.
    pub(crate) fn on_dcd(&mut self, held: bool, at: Instant) {
        for t in self.ack_timer.iter_mut().chain(std::iter::once(&mut self.idle)) {
            if held { t.pause(at) } else { t.resume(at) }
        }
    }

    /// The host queued a PDU.
    pub(crate) fn push(&mut self, id: u64, pdu: &crate::pdu::Pdu) -> Result<(), crate::pdu::PduError> {
        self.tx.push(id, pdu)
    }

    /// The host asked to disconnect.
    pub(crate) fn request_disconnect(&mut self, graceful: bool) {
        self.disconnect = Some(graceful);
    }

    fn disconnect_ready(&self) -> bool {
        match self.disconnect {
            Some(true) => self.tx.is_drained(),
            Some(false) => true,
            None => false,
        }
    }

    /// The next transmission this session wants, and how it gets the channel.
    pub(crate) fn next_tx(&self, cfg: &Config, now: Instant) -> Option<(SessTx, Mode)> {
        if let Phase::Connecting { awaiting: None, not_before, .. } = self.phase {
            return Some((SessTx::Connect, Mode::Csma { not_before }));
        }
        if let Some(o) = self.owed {
            return Some((o.what, Mode::TurnHolder(o.trigger)));
        }
        if !self.is_idle() {
            return None;
        }
        let csma = Mode::Csma { not_before: Instant::ZERO };
        if self.disconnect_ready() || self.idle_disconnect_due(cfg, now) {
            return Some((SessTx::Disconnect, csma));
        }
        if self.tx.has_data_to_send() && self.disconnect != Some(false) {
            return Some((SessTx::FromIdle, csma));
        }
        if self.idle.expired(now) {
            return Some((SessTx::Keepalive, csma));
        }
        None
    }

    fn idle_disconnect_due(&self, cfg: &Config, now: Instant) -> bool {
        cfg.idle_disconnect.is_some_and(|d| now >= self.last_data + d)
    }

    /// The earliest time this session needs attention.
    pub(crate) fn next_deadline(&self, cfg: &Config, now: Instant) -> Option<Instant> {
        let mut v: Vec<Instant> = Vec::new();
        if let Phase::Connecting { awaiting: Some(d), .. } = self.phase {
            v.push(d);
        }
        if let Some(d) = self.ack_timer.and_then(|t| t.deadline()) {
            v.push(d);
        }
        if self.is_idle() {
            v.extend(self.idle.deadline());
            if let Some(d) = cfg.idle_disconnect {
                v.push(self.last_data + d);
            }
        }
        // Level-triggered conditions already due (a keepalive or idle
        // disconnect waiting for the channel) are not deadlines.
        v.into_iter().filter(|&d| d > now).min()
    }

    /// Fire expired timers.
    pub(crate) fn poll_timers(&mut self, ctx: &mut Ctx) -> Option<End> {
        let now = ctx.now;
        if let Phase::Connecting { attempt, awaiting: Some(d), .. } = self.phase {
            if now >= d {
                if attempt >= CONNECT_MAX_RETRIES {
                    return Some(End::ConnectFailed(ConnectFailure::NoAnswer));
                }
                let k = attempt + 1;
                let backoff = ctx.rng.duration(BACKOFF_MIN, BACKOFF_MAX) * k as u32;
                self.phase = Phase::Connecting { attempt: k, awaiting: None, not_before: now + backoff };
            }
        }
        if let Some(t) = self.ack_timer {
            if t.expired(now) {
                self.ack_timer = None;
                let trigger = Trigger::Expiry(now);
                match self.phase {
                    Phase::HalfOpen { resends } => {
                        if resends >= CONNECT_ACK_MAX_RESENDS {
                            return Some(End::ConnectFailed(ConnectFailure::HalfOpenTimeout));
                        }
                        self.phase = Phase::HalfOpen { resends: resends + 1 };
                        self.owed = Some(Owed { what: SessTx::ConnectAck, trigger });
                    }
                    Phase::Closing { resends } => {
                        if resends >= DISCONNECT_MAX_RESENDS {
                            return Some(End::Closed { linger: None });
                        }
                        self.phase = Phase::Closing { resends: resends + 1 };
                        self.owed = Some(Owed { what: SessTx::Disconnect, trigger });
                    }
                    Phase::Open => {
                        self.expiries += 1;
                        let what = match self.expiries {
                            n if n <= ACK_EXPIRY_LAST_RESEND => SessTx::Resend,
                            n if n <= ACK_EXPIRY_LAST_SOUND => SessTx::Sound,
                            n if n >= ACK_EXPIRY_LINK_LOST => return Some(End::LinkLost),
                            _ => unreachable!(),
                        };
                        self.owed = Some(Owed { what, trigger });
                    }
                    Phase::Connecting { .. } => {}
                }
            }
        }
        None
    }

    /// A header from the peer failed while this station awaits it.
    pub(crate) fn on_header_failed(&mut self) {
        if self.phase == Phase::Open && self.ack_timer.is_some() {
            self.rate.on_lost_header();
        }
    }

    /// A non-session DISCONNECT from the peer while connecting: refusal.
    pub(crate) fn on_refusal(&self) -> Option<End> {
        matches!(self.phase, Phase::Connecting { awaiting: Some(_), .. })
            .then_some(End::ConnectFailed(ConnectFailure::Refused))
    }

    /// A session frame (flag set) from the peer, addressed to this station.
    pub(crate) fn on_session_frame(&mut self, f: &RxFrame, ctx: &mut Ctx) -> Option<End> {
        let ftype = f.header.ftype;
        match self.phase {
            Phase::Connecting { .. } => {
                return if ftype == FrameType::ConnectAck { self.on_connect_ack(f, ctx) } else { None };
            }
            Phase::HalfOpen { .. } => {
                if matches!(ftype, FrameType::ConnectAck | FrameType::Connect) {
                    return None;
                }
                self.phase = Phase::Open;
                self.yield_start = Some(ctx.now);
                self.lost_at = None;
                ctx.host(HostEvent::Connected {
                    peer: self.peer,
                    session_id: self.session_id,
                    resumed: self.resume,
                    initiator: false,
                });
            }
            Phase::Open | Phase::Closing { .. } => {
                if ftype == FrameType::ConnectAck {
                    // The initiator's first frame was lost: send it again.
                    if self.role == Role::Initiator && self.phase == Phase::Open {
                        self.ack_timer = None;
                        self.owed = Some(Owed { what: SessTx::FirstFrame, trigger: Trigger::PeerFrame(f.ended_at) });
                    }
                    return None;
                }
            }
        }
        self.ack_timer = None;
        self.expiries = 0;
        self.last_frame_from_peer = Some(ctx.now);
        // Mask ID: demap with the mask the header names (ADR 0011 `mask-id`).
        let known = self.mask.mask_for_id(f.header.mask_id).is_some();
        let (controls, data): (Vec<Option<ControlBlock>>, Vec<Option<crate::frame::Block>>) = if known {
            self.mask.on_header(f.header.mask_id);
            (f.controls.clone(), f.data.clone())
        } else {
            (vec![None; f.controls.len()], vec![None; f.data.len()])
        };
        if let Some(Some(ControlBlock::Ack(a))) = controls.first() {
            self.apply_peer_ack(a, ctx);
        }
        let seqs = if known { f.block_seqs() } else { None };
        let outcome = self.rx.on_blocks(seqs.as_deref(), &data);
        for p in outcome.pdus {
            match p {
                Ok(pdu) => ctx.out.push(Action::Deliver { from: self.peer, pdu }),
                Err(e) => ctx.host(HostEvent::PduError { peer: self.peer, error: e }),
            }
        }
        self.last_meas = f.meas.clone();
        self.last_block_errors = outcome.failed.min(255) as u8;
        self.fresh = true;
        self.mask.on_measurement(f.meas.tone_snr_db.as_deref(), f.meas.probe, ctx.now);
        if ftype == FrameType::Sound {
            self.rate.on_reply_rule(f.meas.snr_db);
            self.replying_to_sound = true;
        } else {
            let stats = FrameStats {
                data: f.data.len(),
                failed: outcome.failed,
                snr_db: f.meas.snr_db,
                distortion: f.meas.distortion,
                adc_clipped: f.meas.adc_clipped,
            };
            if self.rate.on_frame(&stats) == Decision::ClippedHold {
                ctx.host(HostEvent::Warning(Warning::ReceiveClipping { peer: self.peer }));
            }
        }
        if !f.data.is_empty() {
            self.last_data = ctx.now;
        }
        self.restart_idle(ctx);
        let trigger = Trigger::PeerFrame(f.ended_at);
        match ftype {
            FrameType::Data | FrameType::Sound | FrameType::CalRequest => {
                self.owed = Some(Owed { what: SessTx::Reply, trigger });
            }
            FrameType::Ack => {
                if self.tx.has_data_to_send() && self.disconnect != Some(false) {
                    self.owed = Some(Owed { what: SessTx::Reply, trigger });
                } else if self.disconnect_ready() && self.phase == Phase::Open {
                    // READING: a pending DISCONNECT goes out as soon as the
                    // session would otherwise go idle, from idle (carrier sense).
                }
            }
            FrameType::Disconnect => {
                if matches!(self.phase, Phase::Closing { .. }) {
                    return Some(End::Closed { linger: None });
                }
                self.owed = Some(Owed { what: SessTx::DisconnectReply, trigger });
            }
            _ => {}
        }
        None
    }

    fn apply_peer_ack(&mut self, a: &AckBlock, ctx: &mut Ctx) {
        let done = self.tx.on_ack(a.base, a.bitmap);
        if !done.is_empty() {
            ctx.host(HostEvent::Acked { peer: self.peer, ids: done });
        }
        self.tx_mcs = a.requested_mcs.min(6);
        if a.mask_present {
            self.tx_mask = (a.mask, a.mask_id);
        }
        self.peer_reprobe = a.reprobe;
        self.tx_long_prefix = a.long_prefix || ctx.cfg.repeater;
        if a.distortion >= DISTORTION_EXCLUDE_INDEX && self.peer_distortion < DISTORTION_EXCLUDE_INDEX {
            ctx.host(HostEvent::Warning(Warning::Distortion { peer: self.peer, index: a.distortion }));
        }
        self.peer_distortion = a.distortion;
        self.peer_ack = Some(*a);
    }

    /// The peer's latest ACK block (its report of this station's last
    /// decoded frame: SNR, block errors, distortion, requested rung, mask).
    pub fn peer_ack(&self) -> Option<AckBlock> {
        self.peer_ack
    }

    /// This station owes the next frame (a turn-holder reply).
    pub fn owes_reply(&self) -> bool {
        self.owed.is_some()
    }

    /// When the next yield window falls due (SPEC 11.1), in an open session.
    pub fn next_yield(&self) -> Option<Instant> {
        if !matches!(self.phase, Phase::Open | Phase::Closing { .. }) {
            return None;
        }
        self.yield_start.map(|s| s + YIELD_INTERVAL)
    }

    fn on_connect_ack(&mut self, f: &RxFrame, ctx: &mut Ctx) -> Option<End> {
        let Some(Some(ControlBlock::Connect(b3))) = f.controls.first() else { return None };
        if b3.session_id != self.session_id {
            return None;
        }
        self.peer_caps = b3.capabilities;
        self.peer_width_cap = b3.width_cap;
        self.peer_max_frame = b3.max_frame_duration();
        self.tx_mcs = b3.requested_mcs.min(6);
        self.tx_mask = (b3.mask, 0);
        self.tx_long_prefix = ctx.cfg.repeater;
        self.rate.on_reply_rule(f.meas.snr_db);
        self.mask = MaskTracker::new(self.rx_width(ctx.cfg), ctx.cfg.track_masks);
        self.mask.setup(f.meas.tone_snr_db.as_deref(), ctx.now);
        let mut resumed = false;
        if self.resume {
            if b3.resume {
                resumed = true;
                if let Some(Some(ControlBlock::Ack(a))) = f.controls.get(2) {
                    let done = self.tx.on_ack(a.base, a.bitmap);
                    if !done.is_empty() {
                        ctx.host(HostEvent::Acked { peer: self.peer, ids: done });
                    }
                }
            } else {
                // A new session (ADR 0008 `resume-flag-echo`).
                let init = ctx.cfg.initial_sequence as u64;
                let ids = self.tx.reset(init);
                self.rx = Receiver::new(init);
                if !ids.is_empty() {
                    ctx.host(HostEvent::Undelivered { peer: self.peer, ids });
                }
                self.role = Role::Initiator;
            }
        }
        self.resume = resumed;
        self.lost_at = None;
        self.last_meas = f.meas.clone();
        self.last_block_errors = 0;
        self.last_ack = None;
        self.fresh = true;
        self.phase = Phase::Open;
        self.expiries = 0;
        // The idle clock by the original role (ADR 0013 `resume-keeps-roles`):
        // a resuming original responder keeps the 35 s fallback.
        self.restart_idle(ctx);
        self.owed = Some(Owed { what: SessTx::FirstFrame, trigger: Trigger::PeerFrame(f.ended_at) });
        ctx.host(HostEvent::Connected { peer: self.peer, session_id: self.session_id, resumed, initiator: true });
        None
    }

    /// The leading ACK block (ADR 0008 `cumulative-ack`, `ack-after-lost-header`;
    /// ADR 0011 `mask-in-every-session-ack`).
    fn make_ack(&mut self, ctx: &Ctx) -> AckBlock {
        let ack = match self.last_ack {
            Some(prev) if !self.fresh => AckBlock { requested_mcs: self.rate.requested(), ..prev },
            _ => {
                let (base, bitmap) = self.rx.ack_fields();
                let (mask, mask_id) = self.mask.reported();
                AckBlock {
                    mask_present: true,
                    reprobe: self.mask.wants_reprobe(ctx.now),
                    long_prefix: false,
                    base,
                    bitmap,
                    requested_mcs: self.rate.requested(),
                    snr: snr_to_field(self.last_meas.snr_db),
                    block_errors: self.last_block_errors,
                    mask,
                    distortion: self.last_meas.distortion.min(7),
                    adc_clipped: self.last_meas.adc_clipped,
                    mask_id,
                }
            }
        };
        self.last_ack = Some(ack);
        self.fresh = false;
        ack
    }

    fn my_b3(&self, ctx: &Ctx, requested_mcs: u8, mask: u64, resume: bool) -> ConnectBlock {
        let mut ids = [0u32; 4];
        for (i, d) in ctx.cfg.dict_ids.iter().take(4).enumerate() {
            ids[i] = *d;
        }
        ConnectBlock {
            more_dicts: false,
            resume,
            session_id: self.session_id,
            capabilities: ctx.cfg.capabilities & !(caps::HARQ | caps::STREAM_MUX),
            width_cap: ctx.cfg.width_cap,
            requested_mcs,
            max_frame_time: frame_time_units(ctx.cfg.max_frame_time),
            mask,
            dict_ids: ids,
        }
    }

    fn frame(&self, ctx: &Ctx, ftype: FrameType, flags: Flags, controls: Vec<ControlBlock>) -> LinkFrame {
        LinkFrame {
            ftype,
            src: ctx.cfg.address,
            dst: self.peer,
            flags,
            seq: self.tx.next_seq(),
            mcs: 0,
            mask_id: 0,
            controls,
            data: Vec::new(),
            tx: TxParams { width: self.tx_width(ctx.cfg), mask: ALL_TONES, long_prefix: self.tx_long_prefix },
        }
    }

    fn id_block(&self, ctx: &mut Ctx) -> ControlBlock {
        *ctx.last_id = Some(ctx.now);
        ControlBlock::Id(ctx.cfg.id.clone())
    }

    /// Build the frame for a transmission the station was granted. Clears
    /// what was owed.
    pub(crate) fn build(&mut self, what: SessTx, ctx: &mut Ctx) -> LinkFrame {
        self.owed = None;
        let mut what = what;
        if matches!(what, SessTx::Reply | SessTx::FromIdle | SessTx::Keepalive)
            && self.phase == Phase::Open
            && (self.disconnect_ready() || self.idle_disconnect_due(ctx.cfg, ctx.now))
        {
            what = SessTx::Disconnect;
        }
        match what {
            SessTx::Connect => {
                let b3 = self.my_b3(ctx, 0, ALL_TONES, self.resume);
                let id = self.id_block(ctx);
                let mut f = self.frame(
                    ctx,
                    FrameType::Connect,
                    Flags { probe: true, ..Default::default() },
                    vec![ControlBlock::Connect(b3), id],
                );
                f.tx.width = ctx.cfg.width_cap + 1;
                f.tx.long_prefix = ctx.cfg.repeater;
                f
            }
            SessTx::ConnectAck => {
                let (mask, _) = self.mask.reported();
                let req = self.rate.requested();
                let b3 = self.my_b3(ctx, req, mask, self.resume);
                let id = self.id_block(ctx);
                let mut controls = vec![ControlBlock::Connect(b3), id];
                if self.resume {
                    let (base, bitmap) = self.rx.ack_fields();
                    controls.push(ControlBlock::Ack(AckBlock {
                        mask_present: true,
                        base,
                        bitmap,
                        requested_mcs: req,
                        snr: snr_to_field(self.last_meas.snr_db),
                        mask,
                        distortion: self.last_meas.distortion.min(7),
                        adc_clipped: self.last_meas.adc_clipped,
                        ..Default::default()
                    }));
                }
                self.last_sent = FrameType::ConnectAck;
                self.frame(
                    ctx,
                    FrameType::ConnectAck,
                    Flags { session: true, probe: true, ..Default::default() },
                    controls,
                )
            }
            SessTx::Disconnect | SessTx::DisconnectReply => {
                let ack = self.make_ack(ctx);
                let id = self.id_block(ctx);
                if what == SessTx::Disconnect && !matches!(self.phase, Phase::Closing { .. }) {
                    self.phase = Phase::Closing { resends: 0 };
                }
                self.last_sent = FrameType::Disconnect;
                let mut f = self.frame(
                    ctx,
                    FrameType::Disconnect,
                    Flags { session: true, ..Default::default() },
                    vec![ControlBlock::Ack(ack), id],
                );
                f.mask_id = self.tx_mask.1;
                f.tx.mask = self.tx_mask.0;
                f
            }
            SessTx::Sound | SessTx::Keepalive => self.build_sound(ctx),
            SessTx::FirstFrame | SessTx::Reply | SessTx::Resend | SessTx::FromIdle => self.build_data(what, ctx),
        }
    }

    fn build_sound(&mut self, ctx: &mut Ctx) -> LinkFrame {
        let ack = self.make_ack(ctx);
        let mut controls = vec![ControlBlock::Ack(ack)];
        if ctx.id_due() {
            controls.push(self.id_block(ctx));
        }
        self.peer_reprobe = false;
        self.last_sent = FrameType::Sound;
        let mut f =
            self.frame(ctx, FrameType::Sound, Flags { session: true, probe: true, ..Default::default() }, controls);
        f.mask_id = self.tx_mask.1;
        f.tx.mask = self.tx_mask.0;
        f
    }

    fn build_data(&mut self, what: SessTx, ctx: &mut Ctx) -> LinkFrame {
        let w = self.tx_width(ctx.cfg);
        let reply_to_sound = self.replying_to_sound && matches!(what, SessTx::Reply | SessTx::FirstFrame);
        self.replying_to_sound = false;
        let base_probe = self.peer_reprobe || (reply_to_sound && w > BASE_WIDTH);
        let id_due = ctx.id_due();
        let n_ctrl = 1 + id_due as usize;
        let limit = self.tx_limit(ctx, self.tx_mcs);
        let shape = FrameShape {
            mcs: self.tx_mcs,
            width: w,
            mask: self.tx_mask.0,
            controls: n_ctrl,
            data: 0,
            probe: base_probe || w > BASE_WIDTH,
            long_prefix: self.tx_long_prefix,
        };
        let cap_plain = ctx.model.max_data_blocks(&shape, limit);
        let cap_mapped = ctx.model.max_data_blocks(&FrameShape { controls: n_ctrl + 1, ..shape }, limit);
        let sel = if self.disconnect == Some(false) {
            crate::arq::Selection { seq: self.tx.next_seq(), blocks: vec![], abs: vec![], map: None }
        } else {
            self.tx.select(cap_plain, cap_mapped)
        };
        let ack = self.make_ack(ctx);
        let mut controls = vec![ControlBlock::Ack(ack)];
        if let Some(m) = sel.map {
            controls.push(ControlBlock::BlockMap(m));
        }
        if id_due {
            controls.push(self.id_block(ctx));
        }
        let has_data = !sel.blocks.is_empty();
        let ftype = if has_data {
            FrameType::Data
        } else if what == SessTx::Resend && matches!(self.last_sent, FrameType::Sound | FrameType::Data) {
            // A resend without data repeats its type; a DATA frame without
            // data cannot exist, so it becomes a SOUND (READING).
            FrameType::Sound
        } else {
            FrameType::Ack
        };
        let probe = if has_data { shape.probe } else { base_probe || ftype == FrameType::Sound };
        if probe {
            self.peer_reprobe = false;
        }
        self.last_sent = ftype;
        LinkFrame {
            ftype,
            src: ctx.cfg.address,
            dst: self.peer,
            flags: Flags { session: true, probe, ..Default::default() },
            seq: sel.seq,
            mcs: if has_data { self.tx_mcs } else { 0 },
            mask_id: self.tx_mask.1,
            controls,
            data: sel.blocks,
            tx: TxParams { width: w, mask: self.tx_mask.0, long_prefix: self.tx_long_prefix },
        }
    }

    /// The station released PTT after a frame this session sent.
    pub(crate) fn on_tx_done(&mut self, what: SessTx, f: &LinkFrame, ctx: &mut Ctx) -> Option<End> {
        if !f.data.is_empty() {
            self.last_data = ctx.now;
        }
        if self.role == Role::Initiator && self.phase == Phase::Open {
            self.restart_idle(ctx);
        }
        match what {
            SessTx::Connect => {
                if let Phase::Connecting { attempt, not_before, .. } = self.phase {
                    let wait = if ctx.cfg.repeater { CONNECT_WAIT_REPEATER } else { CONNECT_WAIT };
                    self.phase = Phase::Connecting { attempt, awaiting: Some(ctx.now + wait), not_before };
                }
            }
            SessTx::ConnectAck => self.start_ack_timer(ctx),
            SessTx::DisconnectReply if f.ftype == FrameType::Disconnect => {
                return Some(End::Closed { linger: Some(f.clone()) });
            }
            _ => {
                if what == SessTx::FirstFrame && self.yield_start.is_none() {
                    self.yield_start = Some(ctx.now);
                }
                if f.ftype.requires_reply_in_session() {
                    self.start_ack_timer(ctx);
                } else {
                    self.ack_timer = None;
                }
            }
        }
        None
    }

    /// Hand the session's kept state to the retained store.
    pub(crate) fn into_retained(self, now: Instant) -> Retained {
        Retained {
            peer: self.peer,
            role: self.role,
            session_id: self.session_id,
            tx: self.tx,
            rx: self.rx,
            peer_caps: self.peer_caps,
            peer_width_cap: self.peer_width_cap,
            peer_max_frame: self.peer_max_frame,
            lost_at: self.lost_at.unwrap_or(now),
        }
    }
}
