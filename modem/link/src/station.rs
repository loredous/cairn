//! The sans-IO entry point: [`Station::handle`] takes one event at the
//! caller's `now` and returns the resulting actions.
//!
//! Contract with the caller (the modem, or the simulator):
//!
//! - **Time.** Every call carries `now` from one monotonic clock. The last
//!   [`Action::Wakeup`] returned is the time to call again with
//!   [`Event::Tick`] (a later call for another reason is fine; earlier wakeups
//!   are superseded).
//! - **Transmit.** [`Action::Transmit`] means "key PTT now and send this
//!   frame". The caller reports [`Event::TxDone`] when PTT is released. The
//!   station never asks for a second transmission before that.
//! - **DCD.** [`Event::Dcd`] reports edges of the raw detectors (no hold).
//!   The station applies the configured hold.
//! - **Receive.** After decoding a header the caller may ask
//!   [`Station::rx_mask`] which tone mask demaps the frame's blocks, then
//!   reports [`Event::Frame`] with every block's result, or
//!   [`Event::HeaderFailed`] when a preamble was detected but the header
//!   failed.

use std::collections::VecDeque;
use std::time::Duration;

use crate::access::{AccessParams, Attempt, Dcd, Mode, Poll, Trigger};
use crate::broadcast::{BroadcastTx, Message};
use crate::capacity::{FrameModel, FrameShape};
use crate::frame::{
    AckBlock, Address, ControlBlock, Flags, FrameType, IdBlock, LinkFrame, Measurement, RxFrame, RxHeader, TxParams,
};
use crate::linktest::{LinkTestResult, ProbeKind, Prober, sound_reply};
use crate::params::{
    ACK_TIMER_BASE, ACK_TIMER_D_REPEATER_EXTRA, ACK_TIMER_D_SIMPLEX, ACK_TIMER_MARGIN, ALL_TONES, BASE_WIDTH,
    BROADCAST_MCS_DEFAULT, DCD_HOLD_DEFAULT, LEGACY_FRAME_CAP, LEGACY_WINDOW, MAX_FRAME_TIME, PERSISTENCE_DEFAULT,
    READING_RUNG_LIMIT_APPLIES_TO_BROADCAST, SLOT_TIME_DEFAULT, THRESHOLD_MARGIN_DEFAULT_DB, WIDTH_CAP_MIN,
    YIELD_WINDOW, YIELD_WINDOW_REPEATER, rung_frame_limit,
};
use crate::pdu::{FragmentRole, Pdu, PduError, Reassembler, unpack_frame};
use crate::resume::RetainedStore;
use crate::rng::Rng;
use crate::seq::Seq;
use crate::session::{Ctx, End, Phase, Role, SessTx, Session};
use crate::time::Instant;

/// Station configuration (SPEC §7.3, §11, §13.4).
#[derive(Clone, Debug)]
pub struct Config {
    /// This station's address.
    pub address: Address,
    /// This station's ID block (B.6).
    pub id: IdBlock,
    /// Seed of the protocol's random draws.
    pub seed: u64,
    /// Repeater mode (§11.5).
    pub repeater: bool,
    /// Configured maximum frame on-air time (0.5-4.0 s).
    pub max_frame_time: Duration,
    /// DCD hold.
    pub dcd_hold: Duration,
    /// Persistence p (§11.1).
    pub persistence: f64,
    /// Slot time (§11.1).
    pub slot_time: Duration,
    /// Margin added to the §6.2 thresholds (§10.1).
    pub threshold_margin_db: f64,
    /// Width cap (23 for a v0.5 station).
    pub width_cap: u8,
    /// B.3 capability bits.
    pub capabilities: u16,
    /// Dictionary IDs offered in B.3 (up to four are sent).
    pub dict_ids: Vec<u32>,
    /// Broadcast rung (0-3).
    pub broadcast_mcs: u8,
    /// Broadcast width.
    pub broadcast_width: u8,
    /// Track and report tone masks; off, the all-ones mask is reported with
    /// ID 0 throughout (allowed for v0.5, ADR 0011 `v0-5-mask-obligations`).
    pub track_masks: bool,
    /// Accept incoming CONNECTs (the host's policy).
    pub accept_connects: bool,
    /// Stations whose CONNECTs are refused.
    pub deny: Vec<Address>,
    /// Close an idle session after this long without data (off by default,
    /// ADR 0010 `idle-disconnect-policy`).
    pub idle_disconnect: Option<Duration>,
    /// Answer PROBEs when not in a session.
    pub answer_probes: bool,
    /// First block sequence of a new session. The protocol fixes 0 (ADR 0008
    /// `block-sequence-rules`); other values are a test hook for wraparound
    /// and are not conformant on air.
    #[doc(hidden)]
    pub initial_sequence: u16,
}

impl Config {
    /// Defaults for a v0.5 station.
    pub fn new(address: Address, id: IdBlock) -> Config {
        Config {
            address,
            id,
            seed: address.raw(),
            repeater: false,
            max_frame_time: MAX_FRAME_TIME,
            dcd_hold: DCD_HOLD_DEFAULT,
            persistence: PERSISTENCE_DEFAULT,
            slot_time: SLOT_TIME_DEFAULT,
            threshold_margin_db: THRESHOLD_MARGIN_DEFAULT_DB,
            width_cap: WIDTH_CAP_MIN,
            capabilities: 0,
            dict_ids: Vec::new(),
            broadcast_mcs: BROADCAST_MCS_DEFAULT,
            broadcast_width: BASE_WIDTH,
            track_masks: false,
            accept_connects: true,
            deny: Vec::new(),
            idle_disconnect: None,
            answer_probes: true,
            initial_sequence: 0,
        }
    }

    /// A config from a short callsign (ID block from the same text).
    pub fn for_callsign(call: &str, ssid: u8) -> Config {
        let a = Address::callsign(call, ssid).expect("valid callsign");
        Config::new(a, IdBlock::new(call, ssid, None).expect("valid callsign"))
    }
}

/// An input to the station.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A frame whose header decoded.
    Frame(RxFrame),
    /// A preamble was detected but the header failed.
    HeaderFailed,
    /// The raw DCD detectors' OR changed (no hold).
    Dcd(bool),
    /// The AFSK detector asserted or a legacy frame was decoded (§11.3).
    LegacyActivity,
    /// PTT was released after the requested transmission.
    TxDone,
    /// A host command.
    Host(HostCommand),
    /// A wakeup.
    Tick,
}

/// A host command.
#[derive(Clone, Debug, PartialEq)]
pub enum HostCommand {
    /// Open a session; resumes retained state with the peer unless
    /// `new_session`.
    Connect {
        /// The peer.
        peer: Address,
        /// Discard any retained state and start fresh.
        new_session: bool,
    },
    /// Close the session; `graceful` waits until everything queued is acknowledged.
    Disconnect {
        /// Wait for the send queue to drain.
        graceful: bool,
    },
    /// Queue a session PDU under a host id.
    Send {
        /// Host id, echoed in acknowledgment and undelivered reports.
        id: u64,
        /// The PDU.
        pdu: Pdu,
    },
    /// Queue a UI message (several may share a frame; large ones are fragmented).
    Broadcast {
        /// Host id.
        id: u64,
        /// The PDU.
        pdu: Pdu,
        /// Identical repeats of each frame.
        repeats: u8,
    },
    /// Queue a BEACON.
    Beacon {
        /// Host id.
        id: u64,
        /// Optional status or position PDU.
        pdu: Option<Pdu>,
        /// Identical repeats.
        repeats: u8,
    },
    /// Send a PROBE.
    Probe {
        /// The station probed.
        target: Address,
        /// Link test or BEACON request.
        kind: ProbeKind,
        /// Repeats when unanswered.
        repeats: u8,
    },
}

/// Why a session setup failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectFailure {
    /// The peer answered DISCONNECT (ADR 0010 `connect-refusal`).
    Refused,
    /// No CONNECT-ACK after three retries.
    NoAnswer,
    /// Responder: no first frame after three CONNECT-ACK resends.
    HalfOpenTimeout,
    /// This station already has a session.
    Busy,
    /// The host cancelled.
    Cancelled,
}

/// Operator warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// This receiver's input clipped (§10.1: hold and warn).
    ReceiveClipping {
        /// The peer.
        peer: Address,
    },
    /// The peer reports a distortion index of 3 or more: lower the drive.
    Distortion {
        /// The peer.
        peer: Address,
        /// The index.
        index: u8,
    },
}

/// A report to the host.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    /// A CONNECT attempt started.
    Connecting {
        /// The peer.
        peer: Address,
    },
    /// The session is up.
    Connected {
        /// The peer.
        peer: Address,
        /// Session ID.
        session_id: u16,
        /// A resume kept the old session's state.
        resumed: bool,
        /// This station initiated.
        initiator: bool,
    },
    /// Setup failed.
    ConnectFailed {
        /// The peer.
        peer: Address,
        /// Why.
        reason: ConnectFailure,
    },
    /// An incoming CONNECT was refused.
    Refused {
        /// The caller.
        peer: Address,
    },
    /// The link was declared lost; state is kept 10 minutes for resume.
    LinkLost {
        /// The peer.
        peer: Address,
    },
    /// The session closed by DISCONNECT.
    Disconnected {
        /// The peer.
        peer: Address,
    },
    /// PDUs not delivered to the peer (ADR 0010 `disconnect-handshake`).
    Undelivered {
        /// The peer.
        peer: Address,
        /// Host ids.
        ids: Vec<u64>,
    },
    /// PDUs the peer has delivered (wholly below its cumulative ACK).
    Acked {
        /// The peer.
        peer: Address,
        /// Host ids.
        ids: Vec<u64>,
    },
    /// Retained state expired without a resume.
    SessionExpired {
        /// The peer.
        peer: Address,
    },
    /// A Send was refused (no session, or a bad PDU).
    SendRejected {
        /// Host id.
        id: u64,
    },
    /// Broadcast messages completely sent.
    BroadcastSent {
        /// Host ids.
        ids: Vec<u64>,
    },
    /// A broadcast could not be sent.
    BroadcastRejected {
        /// Host id.
        id: u64,
        /// Why.
        error: PduError,
    },
    /// A link test completed.
    LinkTest(LinkTestResult),
    /// A PROBE went unanswered.
    Unreachable {
        /// The target.
        target: Address,
    },
    /// A BEACON was heard (first copy).
    BeaconHeard {
        /// The source.
        from: Address,
        /// Its ID block.
        id: Option<IdBlock>,
    },
    /// A session PDU failed to parse or check.
    PduError {
        /// The peer.
        peer: Address,
        /// The error.
        error: PduError,
    },
    /// An operator warning.
    Warning(Warning),
}

/// An output of the station.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Key PTT now and send this frame.
    Transmit {
        /// The frame.
        frame: LinkFrame,
        /// Sent as a turn-holder reply (no p-persistence).
        turn_holder: bool,
    },
    /// Deliver a session PDU, in order, exactly once.
    Deliver {
        /// The peer.
        from: Address,
        /// The PDU.
        pdu: Pdu,
    },
    /// Deliver a broadcast PDU.
    DeliverBroadcast {
        /// The source.
        from: Address,
        /// BEACON or UI.
        ftype: FrameType,
        /// The PDU.
        pdu: Pdu,
    },
    /// Report to the host.
    Host(HostEvent),
    /// Call again with [`Event::Tick`] at this time.
    Wakeup(Instant),
}

#[derive(Clone, Debug, PartialEq)]
enum OwedReply {
    Refusal { to: Address, trigger: Trigger },
    ProbeSound { to: Address, meas: Measurement, trigger: Trigger },
    ProbeBeacon { trigger: Trigger },
    Linger { trigger: Trigger },
}

impl OwedReply {
    fn trigger(&self) -> Trigger {
        match self {
            OwedReply::Refusal { trigger, .. }
            | OwedReply::ProbeSound { trigger, .. }
            | OwedReply::ProbeBeacon { trigger }
            | OwedReply::Linger { trigger } => *trigger,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    Session(SessTx),
    Owed,
    Broadcast,
    Probe,
}

#[derive(Clone, Debug)]
struct Linger {
    peer: Address,
    until: Instant,
    /// L (ADR 0013 `disconnect-handshake-linger`), restarted after each reply.
    period: Duration,
    frame: LinkFrame,
}

/// How a session frame is decoded ([`Station::rx_link`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxLink {
    /// The sender's session width in tones (its probe reaches the same).
    pub width: u8,
    /// The tone mask the header's Mask ID names; `None` when it names
    /// neither the current nor the pending mask.
    pub mask: Option<u64>,
}

/// A snapshot of the session for monitoring and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// The peer.
    pub peer: Address,
    /// The phase.
    pub phase: Phase,
    /// This station's role.
    pub role: Role,
    /// Session ID.
    pub session_id: u16,
    /// Rung this station requests.
    pub requested_mcs: u8,
    /// Rung this station sends at.
    pub tx_mcs: u8,
    /// Mask and ID this station applies.
    pub tx_mask: (u64, u8),
    /// Mask and ID this station reports.
    pub rx_mask: (u64, u8),
    /// This station's session width (ADR 0011 `session-width`: its cap + 1
    /// when the peer advertised capability bit 0, else 24).
    pub tx_width: u8,
    /// The peer's session width, as this station receives it.
    pub rx_width: u8,
    /// The peer's B.3 width cap.
    pub peer_width_cap: u8,
    /// The peer's B.3 capability bits.
    pub peer_capabilities: u16,
    /// Neither station owes a reply.
    pub idle: bool,
    /// ACK-timer expirations since the last decoded peer header.
    pub expiries: u8,
    /// Next unsent sequence.
    pub next_seq: Seq,
    /// Receiver's next in-order sequence.
    pub rx_next: Seq,
    /// Everything queued has been acknowledged.
    pub drained: bool,
    /// This station owes the next frame.
    pub owes_reply: bool,
    /// When the next yield window falls due (open sessions).
    pub next_yield: Option<Instant>,
    /// The peer's latest ACK block (its report of this station's frames).
    pub peer_report: Option<AckBlock>,
    /// Data blocks sent so far, retransmissions included.
    pub sent_blocks: u64,
    /// Data blocks sent again.
    pub resent_blocks: u64,
}

/// What a pending transmission is (SPEC 13.6 etiquette status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitFor {
    /// A CONNECT.
    Connect,
    /// A turn-holder session frame (CONNECT-ACK, first frame, reply, resend, SOUND, DISCONNECT answer).
    SessionReply,
    /// New session data from idle.
    SessionData,
    /// A keepalive from idle.
    Keepalive,
    /// This station's DISCONNECT.
    Disconnect,
    /// A reply owed outside the session (refusal, link-test SOUND, BEACON, linger).
    OwedReply,
    /// A broadcast (UI or BEACON).
    Broadcast,
    /// A PROBE.
    Probe,
}

/// Why a pending transmission waits (SPEC 13.6 etiquette status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// A timer: retry spacing or a broadcast's not-before time.
    Backoff,
    /// DCD (with hold) is up: the channel is busy.
    Carrier,
    /// A turn-holder reply waiting for the peer's carrier to drop.
    CarrierGone,
    /// The raw-DCD check window before a turn-holder reply (ADR 0010).
    TurnCheck,
    /// The yield window (SPEC 11.1).
    YieldWindow,
    /// p-persistence slots (SPEC 11.1).
    Persistence,
    /// Nothing in the way: about to be granted.
    Clear,
}

/// A pending transmission and what it waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// What.
    pub what: WaitFor,
    /// Why.
    pub reason: WaitReason,
    /// When the wait ends at the latest, when known.
    pub until: Option<Instant>,
}

/// A snapshot of channel access (SPEC 13.6 etiquette status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
    /// The raw detectors' OR.
    pub dcd_raw: bool,
    /// DCD with hold.
    pub dcd_held: bool,
    /// A transmission is in progress.
    pub transmitting: bool,
    /// Legacy activity in the last 10 minutes (SPEC 11.3).
    pub legacy: bool,
    /// The transmission waiting for the channel, if any.
    pub waiting: Option<Waiting>,
}

/// One station's link layer.
pub struct Station {
    cfg: Config,
    model: Box<dyn FrameModel + Send>,
    rng: Rng,
    now: Instant,
    dcd: Dcd,
    held: bool,
    legacy_until: Option<Instant>,
    transmitting: Option<(Intent, LinkFrame)>,
    attempt: Option<Attempt>,
    attempt_wake: Option<Instant>,
    session: Option<Session>,
    retained: RetainedStore,
    bcast: BroadcastTx,
    reasm: Reassembler,
    prober: Option<Prober>,
    owed: VecDeque<OwedReply>,
    linger: Option<Linger>,
    counter: u16,
    last_id: Option<Instant>,
    out: Vec<Action>,
}

macro_rules! ctx {
    ($s:ident) => {
        Ctx {
            cfg: &$s.cfg,
            rng: &mut $s.rng,
            model: &*$s.model,
            now: $s.now,
            dcd_held: $s.held,
            legacy: $s.legacy_until.is_some_and(|t| $s.now < t),
            last_id: &mut $s.last_id,
            out: &mut $s.out,
        }
    };
}

impl Station {
    /// A station with a frame model (the PHY's budget, or [`crate::AppendixA`]).
    pub fn new(cfg: Config, model: Box<dyn FrameModel + Send>) -> Station {
        let mut rng = Rng::new(cfg.seed);
        let counter = rng.u16();
        Station {
            dcd: Dcd::new(cfg.dcd_hold),
            cfg,
            model,
            rng,
            now: Instant::ZERO,
            held: false,
            legacy_until: None,
            transmitting: None,
            attempt: None,
            attempt_wake: None,
            session: None,
            retained: RetainedStore::default(),
            bcast: BroadcastTx::default(),
            reasm: Reassembler::default(),
            prober: None,
            owed: VecDeque::new(),
            linger: None,
            counter,
            last_id: None,
            out: Vec::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// This station's address.
    pub fn address(&self) -> Address {
        self.cfg.address
    }

    /// A snapshot of the session, if any.
    pub fn session_info(&self) -> Option<SessionInfo> {
        self.session.as_ref().map(|s| SessionInfo {
            peer: s.peer(),
            phase: s.phase(),
            role: s.role(),
            session_id: s.session_id(),
            requested_mcs: s.requested_mcs(),
            tx_mcs: s.tx_mcs(),
            tx_mask: s.tx_mask(),
            rx_mask: s.rx_mask(),
            tx_width: s.tx_width(&self.cfg),
            rx_width: s.rx_width(&self.cfg),
            peer_width_cap: s.peer_width_cap,
            peer_capabilities: s.peer_caps,
            idle: s.is_idle(),
            expiries: s.expiries(),
            next_seq: s.tx.next_seq(),
            rx_next: s.rx.next_seq(),
            drained: s.tx.is_drained(),
            owes_reply: s.owes_reply(),
            next_yield: s.next_yield(),
            peer_report: s.peer_ack(),
            sent_blocks: s.tx.sent_blocks(),
            resent_blocks: s.tx.resent_blocks(),
        })
    }

    /// Channel access as of the last event (SPEC 13.6 etiquette status).
    pub fn channel_info(&self) -> ChannelInfo {
        let waiting = if self.transmitting.is_some() {
            None
        } else {
            self.desired().map(|(intent, mode)| {
                let what = match intent {
                    Intent::Session(SessTx::Connect) => WaitFor::Connect,
                    Intent::Session(SessTx::FromIdle) => WaitFor::SessionData,
                    Intent::Session(SessTx::Keepalive) => WaitFor::Keepalive,
                    Intent::Session(SessTx::Disconnect) => WaitFor::Disconnect,
                    Intent::Session(_) => WaitFor::SessionReply,
                    Intent::Owed => WaitFor::OwedReply,
                    Intent::Broadcast => WaitFor::Broadcast,
                    Intent::Probe => WaitFor::Probe,
                };
                let (reason, until) = match &self.attempt {
                    Some(a) if a.origin() == mode => a.wait_state(&self.dcd, self.now),
                    _ => match mode {
                        Mode::Csma { not_before } if not_before > self.now => (WaitReason::Backoff, Some(not_before)),
                        _ => (WaitReason::Clear, None),
                    },
                };
                Waiting { what, reason, until }
            })
        };
        ChannelInfo {
            dcd_raw: self.dcd.raw(),
            dcd_held: self.held,
            transmitting: self.transmitting.is_some(),
            legacy: self.legacy(),
            waiting,
        }
    }

    /// Whether state is retained for resume with a peer.
    pub fn has_retained(&self, peer: Address) -> bool {
        self.retained.get(peer).is_some()
    }

    /// The tone mask that demaps a frame's control and data blocks, from its
    /// decoded header (ADR 0011 `mask-id`; `cairn_phy::LinkState::mask_for`).
    /// CONNECT, CONNECT-ACK and every frame outside a session are unmasked,
    /// whatever their Mask ID field says. `None`: a session frame from the
    /// peer whose header names neither the current nor the pending mask, so
    /// every control and data block counts as failed.
    pub fn rx_mask(&self, header: &RxHeader) -> Option<u64> {
        if matches!(header.ftype, FrameType::Connect | FrameType::ConnectAck) || !header.flags.session {
            return Some(ALL_TONES);
        }
        match &self.session {
            Some(s) if s.peer() == header.src && !matches!(s.phase(), Phase::Connecting { .. }) => {
                s.rx_mask_for(header.mask_id)
            }
            _ => Some(ALL_TONES),
        }
    }

    /// How the receiver should decode the frame this header describes: a
    /// session frame from the peer at the session's receive width with the
    /// mask its Mask ID names (`mask` `None`: an ID neither current nor
    /// pending, so every control and data block counts as failed, ADR 0011),
    /// or, for CONNECT, CONNECT-ACK and every frame outside a session,
    /// `None`: the receiver takes the width from the frame's own probe
    /// (ADR 0007) and decodes unmasked. The `cairn_phy::LinkSource` a modem
    /// hands its receiver is built from this.
    pub fn rx_link(&self, header: &RxHeader) -> Option<RxLink> {
        if matches!(header.ftype, FrameType::Connect | FrameType::ConnectAck) || !header.flags.session {
            return None;
        }
        match &self.session {
            Some(s) if s.peer() == header.src && !matches!(s.phase(), Phase::Connecting { .. }) => {
                Some(RxLink { width: s.rx_width(&self.cfg), mask: s.rx_mask_for(header.mask_id) })
            }
            _ => None,
        }
    }

    /// The configuration, for changes the host makes while running (KISS
    /// persistence and slot time, SPEC 13.1). Changing the address or the ID
    /// block mid-session is not supported.
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.cfg
    }

    /// Replace the frame model (a new TXDelay or TXTail, SPEC 13.1).
    pub fn set_frame_model(&mut self, model: Box<dyn FrameModel + Send>) {
        self.model = model;
    }

    /// Process one event at `now`.
    pub fn handle(&mut self, ev: Event, now: Instant) -> Vec<Action> {
        if now > self.now {
            self.now = now;
        }
        self.update_held();
        match ev {
            Event::Frame(f) => self.on_frame(f),
            Event::HeaderFailed => {
                if let Some(s) = self.session.as_mut() {
                    s.on_header_failed();
                }
            }
            Event::Dcd(up) => self.dcd.set_raw(up, self.now),
            Event::LegacyActivity => self.legacy_until = Some(self.now + LEGACY_WINDOW),
            Event::TxDone => self.on_tx_done(),
            Event::Host(cmd) => self.on_host(cmd),
            Event::Tick => {}
        }
        self.run_timers();
        self.update_held();
        self.drive();
        if let Some(w) = self.next_deadline() {
            self.out.push(Action::Wakeup(w));
        }
        std::mem::take(&mut self.out)
    }

    fn host(&mut self, ev: HostEvent) {
        self.out.push(Action::Host(ev));
    }

    fn legacy(&self) -> bool {
        self.legacy_until.is_some_and(|t| self.now < t)
    }

    fn update_held(&mut self) {
        let h = self.dcd.held(self.now);
        if h != self.held {
            let at = if h {
                self.now
            } else {
                self.dcd.last_down().map_or(self.now, |d| (d + self.cfg.dcd_hold).min(self.now))
            };
            self.held = h;
            if let Some(s) = self.session.as_mut() {
                s.on_dcd(h, at);
            }
        }
    }

    // --- Receive -------------------------------------------------------------------

    fn on_frame(&mut self, f: RxFrame) {
        let h = f.header;
        let me = self.cfg.address;
        if h.dst != me && !h.dst.is_broadcast() {
            return;
        }
        match h.ftype {
            FrameType::Beacon | FrameType::Ui => return self.on_broadcast(f),
            FrameType::Connect if h.dst == me => return self.on_connect(f),
            FrameType::Probe if h.dst == me => return self.on_probe(f),
            FrameType::Sound if !h.flags.session && h.dst == me => {
                if let Some(p) = &self.prober {
                    if p.target() == h.src {
                        if let Some(r) = p.on_sound(&f, &self.cfg, &*self.model) {
                            self.prober = None;
                            self.host(HostEvent::LinkTest(r));
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        if h.dst != me {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            if s.peer() == h.src {
                let end = if h.flags.session {
                    let mut c = ctx!(self);
                    s.on_session_frame(&f, &mut c)
                } else if h.ftype == FrameType::Disconnect {
                    s.on_refusal()
                } else {
                    None
                };
                if let Some(e) = end {
                    self.end_session(e);
                }
                return;
            }
        }
        if let Some(l) = &self.linger {
            if l.peer == h.src && h.ftype == FrameType::Disconnect && h.flags.session {
                self.owed.push_back(OwedReply::Linger { trigger: Trigger::PeerFrame(f.ended_at) });
            }
        }
    }

    fn refuse(&mut self, f: &RxFrame) {
        self.owed.push_back(OwedReply::Refusal { to: f.header.src, trigger: Trigger::PeerFrame(f.ended_at) });
        self.host(HostEvent::Refused { peer: f.header.src });
    }

    fn on_connect(&mut self, f: RxFrame) {
        let Some(Some(ControlBlock::Connect(b3))) = f.controls.first() else { return };
        let b3 = *b3;
        let src = f.header.src;
        if let Some(s) = self.session.as_mut() {
            if s.peer() != src {
                return self.refuse(&f);
            }
            if b3.resume && s.session_id() == b3.session_id {
                let mut c = ctx!(self);
                match s.phase() {
                    // Crossing resume CONNECTs: the original initiator's wins.
                    Phase::Connecting { .. } if s.role() == Role::Responder => s.accept_resume(&f, &mut c),
                    Phase::Connecting { .. } => {}
                    Phase::HalfOpen { .. } => s.answer_connect(&f, &mut c),
                    Phase::Open | Phase::Closing { .. } => s.accept_resume(&f, &mut c),
                }
                return;
            }
            match s.phase() {
                Phase::HalfOpen { .. } if !b3.resume && !s.resume && s.session_id() == b3.session_id => {
                    // A repeated CONNECT: answer again, re-measured.
                    let mut c = ctx!(self);
                    s.answer_connect(&f, &mut c);
                    return;
                }
                // ADR 0013 `crossing-new-connects`: the station already
                // connecting refuses the peer's CONNECT and keeps its own.
                Phase::Connecting { .. } => return self.refuse(&f),
                _ => {
                    // A new session from the peer of a live one ends it, as a
                    // link loss with its undelivered data.
                    let s = self.session.take().expect("session");
                    let ids = s.tx.undelivered();
                    self.host(HostEvent::LinkLost { peer: src });
                    if !ids.is_empty() {
                        self.host(HostEvent::Undelivered { peer: src, ids });
                    }
                }
            }
        }
        if self.session.is_some() {
            return self.refuse(&f);
        }
        if !self.cfg.accept_connects || self.cfg.deny.contains(&src) {
            return self.refuse(&f);
        }
        let state = match self.retained.take(src) {
            Some(r) if b3.resume && r.session_id == b3.session_id => Some(r),
            Some(r) => {
                let ids = r.tx.undelivered();
                self.host(HostEvent::SessionExpired { peer: src });
                if !ids.is_empty() {
                    self.host(HostEvent::Undelivered { peer: src, ids });
                }
                None
            }
            None => None,
        };
        let mut c = ctx!(self);
        let s = Session::new_responder(&f, &b3, state, &mut c);
        self.session = Some(s);
    }

    fn on_probe(&mut self, f: RxFrame) {
        // A station in a session, or setting one up, does not answer
        // (ADR 0012 `probe-not-answered-in-session`).
        if self.session.is_some() || !self.cfg.answer_probes {
            return;
        }
        let trigger = Trigger::PeerFrame(f.ended_at);
        match f.controls.as_slice() {
            [Some(ControlBlock::Connect(_))] => {
                self.owed.push_back(OwedReply::ProbeSound { to: f.header.src, meas: f.meas.clone(), trigger })
            }
            [] => self.owed.push_back(OwedReply::ProbeBeacon { trigger }),
            _ => {}
        }
    }

    fn on_broadcast(&mut self, f: RxFrame) {
        let h = f.header;
        let role = FragmentRole::from_flags(h.flags.first_fragment, h.flags.more_fragments);
        let pdus = unpack_frame(&f.data);
        let Some(out) = self.reasm.on_frame(h.src, h.seq.0, role, pdus, self.now) else { return };
        if h.ftype == FrameType::Beacon {
            let id = f.controls.iter().flatten().find_map(|c| match c {
                ControlBlock::Id(i) => Some(i.clone()),
                _ => None,
            });
            self.host(HostEvent::BeaconHeard { from: h.src, id });
            if self.prober.as_ref().is_some_and(|p| p.target() == h.src && p.kind() == ProbeKind::Beacon) {
                self.prober = None;
            }
        }
        for pdu in out {
            self.out.push(Action::DeliverBroadcast { from: h.src, ftype: h.ftype, pdu });
        }
    }

    // --- Host -----------------------------------------------------------------------

    fn on_host(&mut self, cmd: HostCommand) {
        match cmd {
            HostCommand::Connect { peer, new_session } => {
                if self.session.is_some() {
                    self.host(HostEvent::ConnectFailed { peer, reason: ConnectFailure::Busy });
                    return;
                }
                let mut r = self.retained.take(peer);
                if new_session {
                    if let Some(old) = r.take() {
                        let ids = old.tx.undelivered();
                        if !ids.is_empty() {
                            self.host(HostEvent::Undelivered { peer, ids });
                        }
                    }
                }
                let mut c = ctx!(self);
                let s = Session::new_initiator(peer, r, &mut c);
                self.session = Some(s);
                self.host(HostEvent::Connecting { peer });
            }
            HostCommand::Disconnect { graceful } => {
                let Some(s) = self.session.as_mut() else { return };
                if matches!(s.phase(), Phase::Connecting { .. }) {
                    self.end_session(End::ConnectFailed(ConnectFailure::Cancelled));
                } else {
                    s.request_disconnect(graceful);
                }
            }
            HostCommand::Send { id, pdu } => {
                let ok = match self.session.as_mut() {
                    Some(s) => s.push(id, &pdu).is_ok(),
                    None => false,
                };
                if !ok {
                    self.host(HostEvent::SendRejected { id });
                }
            }
            HostCommand::Broadcast { id, pdu, repeats } => self.bcast.push(id, Message::Ui(pdu), repeats),
            HostCommand::Beacon { id, pdu, repeats } => self.bcast.push(id, Message::Beacon(pdu), repeats),
            HostCommand::Probe { target, kind, repeats } => {
                self.prober = Some(Prober::new(target, kind, repeats, self.now));
            }
        }
    }

    // --- Timers ---------------------------------------------------------------------

    fn run_timers(&mut self) {
        if let Some(s) = self.session.as_mut() {
            let mut c = ctx!(self);
            if let Some(e) = s.poll_timers(&mut c) {
                self.end_session(e);
            }
        }
        for r in self.retained.expire(self.now) {
            let ids = r.tx.undelivered();
            self.host(HostEvent::SessionExpired { peer: r.peer });
            if !ids.is_empty() {
                self.host(HostEvent::Undelivered { peer: r.peer, ids });
            }
        }
        if let Some(p) = self.prober.as_mut() {
            if p.deadline().is_some_and(|d| self.now >= d) && p.on_timeout(self.now, &mut self.rng) {
                let target = p.target();
                self.prober = None;
                self.host(HostEvent::Unreachable { target });
            }
        }
        if self.linger.as_ref().is_some_and(|l| self.now >= l.until) {
            self.linger = None;
        }
        self.reasm.purge(self.now);
    }

    fn ack_period(&self, peer_max: Duration) -> Duration {
        let d = ACK_TIMER_D_SIMPLEX + if self.cfg.repeater { ACK_TIMER_D_REPEATER_EXTRA } else { Duration::ZERO };
        let y = if self.cfg.repeater { YIELD_WINDOW_REPEATER } else { YIELD_WINDOW };
        ACK_TIMER_BASE + d + y + self.cfg.max_frame_time.min(peer_max) + ACK_TIMER_MARGIN
    }

    fn end_session(&mut self, e: End) {
        let Some(mut s) = self.session.take() else { return };
        let peer = s.peer();
        match e {
            End::LinkLost => {
                self.host(HostEvent::LinkLost { peer });
                let r = s.into_retained(self.now);
                self.retained.insert(r);
            }
            End::Closed { linger } => {
                s.rx.discard_held();
                let ids = s.tx.undelivered();
                self.host(HostEvent::Disconnected { peer });
                if !ids.is_empty() {
                    self.host(HostEvent::Undelivered { peer, ids });
                }
                if let Some(frame) = linger {
                    // ADR 0013 `disconnect-handshake-linger`: answer repeats
                    // until L = T + 0.5 + 0.25 + 0.1 s + the reply's air time
                    // has passed since the end of the latest reply.
                    let shape = FrameShape {
                        mcs: 0,
                        width: frame.tx.width,
                        mask: ALL_TONES,
                        controls: frame.controls.len(),
                        data: 0,
                        probe: false,
                        long_prefix: frame.tx.long_prefix,
                    };
                    let period = self.ack_period(s.peer_max_frame)
                        + crate::params::LINGER_EXTRA
                        + self.model.on_air_time(&shape);
                    self.linger = Some(Linger { peer, until: self.now + period, period, frame });
                }
            }
            End::ConnectFailed(reason) => {
                self.host(HostEvent::ConnectFailed { peer, reason });
                // ADR 0013 `duplicate-connect-and-half-open-retention`: a
                // responder giving up on a half-open session keeps its state
                // for resume, as after a link loss.
                if s.had_state || reason == ConnectFailure::HalfOpenTimeout {
                    let r = s.into_retained(self.now);
                    self.retained.insert(r);
                } else {
                    let ids = s.tx.undelivered();
                    if !ids.is_empty() {
                        self.host(HostEvent::Undelivered { peer, ids });
                    }
                }
            }
        }
        self.attempt = None;
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut v: Vec<Instant> = Vec::new();
        v.extend(self.attempt_wake);
        if let Some(s) = &self.session {
            v.extend(s.next_deadline(&self.cfg, self.now));
        }
        v.extend(self.retained.next_deadline());
        v.extend(self.prober.as_ref().and_then(|p| p.deadline()));
        v.extend(self.linger.as_ref().map(|l| l.until));
        v.extend(self.reasm.next_deadline());
        v.extend(self.dcd.held_clears_at(self.now));
        v.into_iter().min().map(|t| t.max(self.now))
    }

    // --- Transmit -------------------------------------------------------------------

    fn yield_due(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.yield_due(self.now))
    }

    fn desired(&self) -> Option<(Intent, Mode)> {
        let mut sess_csma = None;
        if let Some(s) = &self.session {
            if let Some((what, mode)) = s.next_tx(&self.cfg, self.now) {
                if matches!(mode, Mode::TurnHolder(_)) {
                    return Some((Intent::Session(what), mode));
                }
                sess_csma = Some((Intent::Session(what), mode));
            }
        }
        if let Some(r) = self.owed.front() {
            return Some((Intent::Owed, Mode::TurnHolder(r.trigger())));
        }
        if sess_csma.is_some() {
            return sess_csma;
        }
        // Broadcasts and PROBEs only while the session is idle (ADR 0010
        // `broadcast-during-session`).
        if self.session.as_ref().is_some_and(|s| !s.is_idle()) {
            return None;
        }
        if let Some(t) = self.prober.as_ref().and_then(|p| p.ready_at()) {
            if !self.bcast.mid_message() {
                return Some((Intent::Probe, Mode::Csma { not_before: t }));
            }
        }
        self.bcast.next_ready().map(|t| (Intent::Broadcast, Mode::Csma { not_before: t }))
    }

    fn access_params(&self) -> AccessParams {
        let p = if self.legacy() { self.cfg.persistence / 2.0 } else { self.cfg.persistence };
        let y = if self.cfg.repeater { YIELD_WINDOW_REPEATER } else { YIELD_WINDOW };
        AccessParams { p, slot: self.cfg.slot_time, yield_window: y }
    }

    fn broadcast_capacity(&self, controls: usize) -> usize {
        let mcs = self.cfg.broadcast_mcs.min(crate::params::BROADCAST_MCS_MAX);
        let mut limit = self.cfg.max_frame_time;
        if READING_RUNG_LIMIT_APPLIES_TO_BROADCAST {
            limit = limit.min(rung_frame_limit(mcs));
        }
        if self.legacy() {
            limit = limit.min(LEGACY_FRAME_CAP);
        }
        let width = self.cfg.broadcast_width.max(BASE_WIDTH);
        let shape = FrameShape {
            mcs,
            width,
            mask: ALL_TONES,
            controls,
            data: 0,
            probe: width > BASE_WIDTH,
            long_prefix: self.cfg.repeater,
        };
        self.model.max_data_blocks(&shape, limit)
    }

    fn drive(&mut self) {
        self.attempt_wake = None;
        if self.transmitting.is_some() {
            self.attempt = None;
            return;
        }
        if self.bcast.next_ready().is_none() && !self.bcast.is_empty() {
            let caps = [self.broadcast_capacity(0), self.broadcast_capacity(1)];
            for (id, error) in self.bcast.prepare(|n| caps[n.min(1)], self.now) {
                self.host(HostEvent::BroadcastRejected { id, error });
            }
        }
        let Some((intent, mode)) = self.desired() else {
            self.attempt = None;
            return;
        };
        if self.attempt.as_ref().map(|a| a.origin()) != Some(mode) {
            self.attempt = Some(Attempt::new(mode, self.yield_due(), &self.dcd, self.now));
        }
        let prm = self.access_params();
        let a = self.attempt.as_mut().expect("attempt");
        let poll = a.poll(&self.dcd, self.now, &prm, &mut self.rng);
        if let Some(t) = a.take_yield() {
            if let Some(s) = self.session.as_mut() {
                s.yield_start = Some(t);
            }
        }
        match poll {
            Poll::Wait(t) => self.attempt_wake = t,
            Poll::Grant => {
                let turn_holder = a.is_turn_holder();
                self.attempt = None;
                let frame = self.build(intent);
                self.out.push(Action::Transmit { frame: frame.clone(), turn_holder });
                self.transmitting = Some((intent, frame));
            }
        }
    }

    fn build(&mut self, intent: Intent) -> LinkFrame {
        match intent {
            Intent::Session(what) => {
                let s = self.session.as_mut().expect("session");
                let mut c = ctx!(self);
                s.build(what, &mut c)
            }
            Intent::Owed => match self.owed.pop_front().expect("owed reply") {
                OwedReply::Refusal { to, .. } => {
                    self.last_id = Some(self.now);
                    LinkFrame {
                        ftype: FrameType::Disconnect,
                        src: self.cfg.address,
                        dst: to,
                        flags: Flags::default(),
                        seq: Seq(0),
                        mcs: 0,
                        mask_id: 0,
                        controls: vec![ControlBlock::Ack(AckBlock::zero()), ControlBlock::Id(self.cfg.id.clone())],
                        data: vec![],
                        tx: TxParams { width: BASE_WIDTH, mask: ALL_TONES, long_prefix: self.cfg.repeater },
                    }
                }
                OwedReply::ProbeSound { to, meas, .. } => sound_reply(&self.cfg, to, &meas),
                OwedReply::ProbeBeacon { .. } => {
                    self.counter = self.counter.wrapping_add(1);
                    self.last_id = Some(self.now);
                    let width = self.cfg.broadcast_width.max(BASE_WIDTH);
                    LinkFrame {
                        ftype: FrameType::Beacon,
                        src: self.cfg.address,
                        dst: Address::BROADCAST,
                        flags: Flags { first_fragment: true, probe: width > BASE_WIDTH, ..Default::default() },
                        seq: Seq(self.counter),
                        mcs: 0,
                        mask_id: 0,
                        controls: vec![ControlBlock::Id(self.cfg.id.clone())],
                        data: vec![],
                        tx: TxParams { width, mask: ALL_TONES, long_prefix: self.cfg.repeater },
                    }
                }
                OwedReply::Linger { .. } => {
                    self.last_id = Some(self.now);
                    self.linger.as_ref().map(|l| l.frame.clone()).expect("linger frame")
                }
            },
            Intent::Broadcast => self.bcast.build(&self.cfg, &mut self.counter, &mut self.last_id, self.now),
            Intent::Probe => {
                self.counter = self.counter.wrapping_add(1);
                self.prober.as_ref().expect("prober").build(&self.cfg, self.counter)
            }
        }
    }

    fn on_tx_done(&mut self) {
        let Some((intent, frame)) = self.transmitting.take() else { return };
        match intent {
            Intent::Session(what) => {
                if let Some(s) = self.session.as_mut() {
                    let mut c = ctx!(self);
                    if let Some(e) = s.on_tx_done(what, &frame, &mut c) {
                        self.end_session(e);
                    }
                }
            }
            Intent::Owed => {
                // A lingering DISCONNECT answer restarts the linger time
                // (ADR 0013 `disconnect-handshake-linger`).
                if let Some(l) = self.linger.as_mut() {
                    if frame.ftype == FrameType::Disconnect && frame.flags.session && frame.dst == l.peer {
                        l.until = self.now + l.period;
                    }
                }
            }
            Intent::Broadcast => {
                let ids = self.bcast.on_tx_done(self.now, &mut self.rng);
                if !ids.is_empty() {
                    self.host(HostEvent::BroadcastSent { ids });
                }
            }
            Intent::Probe => {
                if let Some(p) = self.prober.as_mut() {
                    p.on_tx_done(self.now);
                }
            }
        }
    }
}
