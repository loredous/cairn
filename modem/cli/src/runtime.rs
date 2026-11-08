//! The modem runtime (SPEC 21.3 steps 5-6): one sans-IO core that owns a
//! link-layer [`Station`] and wires it to the physical layer.
//!
//! Audio in: every chunk goes to carrier detect ([`cairn_phy::dcd::Dcd`],
//! SPEC 7.2) and to the streaming preamble detector; a detection is
//! decoded header first, the header asks the station how to decode the
//! rest ([`Station::rx_link`] behind a `LinkSource`), the frame goes to the
//! station as [`Event::Frame`] with the ADR 0006 channel SNR. Raw DCD edges
//! (the OR of quieting, the preamble correlator, the AFSK detector and COS,
//! without hold) go to the station as [`Event::Dcd`]; the station applies
//! its own hold (ADR 0010). The AFSK detector asserting is also
//! [`Event::LegacyActivity`] (SPEC 11.3).
//!
//! Audio out: [`Action::Transmit`] becomes [`Output::Transmit`], the frame
//! built with `Transmitter::build_frame` and scaled to -3 dBFS peak; the
//! caller keys PTT now, plays it TXDelay later, releases TXTail after the
//! last sample and reports the release time with [`Modem::tx_done`]. While
//! keyed the receiver ignores its input (half duplex): the detector sees
//! silence and carrier detect neither asserts nor learns.
//!
//! Time: every call carries the caller's clock (`now`), and audio carries
//! the time of its first sample, so the same core runs on a sound card (the
//! wall clock) and in the two-station simulator (a virtual clock).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver as ChanRx, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_api::capture::AudioRing;
use cairn_audio::LevelMeter;
use cairn_link::adapter::{FieldCodec, PhyCodec, address_from_phy, header_from_phy};
use cairn_link::session::Phase;
use cairn_link::{
    Action, Address, Config, Event, FrameType, HostCommand, HostEvent, Instant, LinkFrame, Measurement, Pdu, RxHeader,
    Station,
};
use cairn_phy::dcd::{Dcd, DcdConfig, DcdEdge, DcdSource, DcdState};
use cairn_phy::detector::DetectorConfig;
use cairn_phy::fields::Header;
use cairn_phy::params::FS;
use cairn_phy::rx::{extent_samples, frame_link};
use cairn_phy::{ConfigError, FrameConfig, LinkState, Receiver, RxOptions, Transmitter};

use crate::agc::{LevelKind, LevelObs};
use crate::listen::{self, ConfigSource, FrameGap, Job, Listener, Plan, Resolution, header_samples};
use crate::phymodel::{PhyFrameModel, mask_tones};

/// How long the preamble correlator holds DCD after its early confirmation
/// while the detection and header resolve (the final detection comes about
/// 85 ms after the frame's start, the header 124 ms after it).
const EARLY_HOLD: usize = 7_200;
/// Longest a frame can be, samples (4 s of data, a probe, slack), for
/// "on the air" before the frame's length is known.
const ON_AIR_MAX: usize = 5 * 48_000;
/// Input at or above this magnitude counts as clipped (SPEC 7.1).
pub const CLIP_LEVEL: f64 = 0.999;
/// Idle level windows for the receive AGC, samples (0.5 s).
const IDLE_WINDOW: u64 = 24_000;

/// When a decoded frame reaches the station.
#[derive(Clone, Copy, Debug)]
pub enum DecodeTime {
    /// The caller's clock after the decode (a live modem: `now` is the
    /// wall clock since `origin`).
    Wall(std::time::Instant),
    /// The time of the chunk that completed the frame plus the measured
    /// decode time (the simulator: as long as the decode really took).
    Measured,
    /// That time plus a fixed processing time (the simulator, deterministic).
    Fixed(Duration),
}

/// Runtime settings.
#[derive(Clone, Debug)]
pub struct ModemConfig {
    /// The link layer's configuration (callsign, ID block, timers, CSMA).
    pub station: Config,
    /// PTT to first audio sample (SPEC 7.3).
    pub txdelay: Duration,
    /// Last audio sample to release (SPEC 7.3).
    pub txtail: Duration,
    /// Receiver options (data blocks on several threads).
    pub rx: RxOptions,
    /// Preamble detector.
    pub detector: DetectorConfig,
    /// Carrier detect.
    pub dcd: DcdConfig,
    /// When decoded frames reach the station.
    pub decode_time: DecodeTime,
    /// Guard after PTT release during which input is still ignored.
    pub unmute_guard: Duration,
    /// Raw-audio ring buffer length, seconds (SPEC 13.6; 0: off).
    pub ring_seconds: f64,
    /// The quieting band follows a live width-cap change
    /// ([`Modem::set_cap`]); off when it was set explicitly.
    pub quieting_follows_cap: bool,
}

impl ModemConfig {
    /// Defaults for a station.
    pub fn new(station: Config) -> Self {
        ModemConfig {
            station,
            txdelay: Duration::from_millis(150),
            txtail: Duration::from_millis(10),
            rx: RxOptions::default(),
            detector: DetectorConfig::default(),
            dcd: DcdConfig::default(),
            decode_time: DecodeTime::Fixed(Duration::from_millis(20)),
            unmute_guard: Duration::from_millis(5),
            ring_seconds: 120.0,
            quieting_follows_cap: true,
        }
    }
}

/// A frame to transmit now.
#[derive(Clone, Debug)]
pub struct TxRequest {
    /// Audio at 48 kHz, peaks at -3 dBFS.
    pub audio: Vec<f32>,
    /// When the station asked to key (the caller keys now).
    pub key_at: Instant,
    /// The link frame.
    pub frame: LinkFrame,
    /// A turn-holder reply (no p-persistence).
    pub turn_holder: bool,
    /// Samples of audio (kept when a copy drops the audio itself).
    pub samples: usize,
}

impl TxRequest {
    /// PTT-on time with these timings.
    pub fn keyed(&self, txdelay: Duration, txtail: Duration) -> Duration {
        txdelay + Duration::from_secs_f64(self.samples as f64 / FS as f64) + txtail
    }
}

/// A decoded frame, for the heard list and logs (SPEC 13.2 `heard`, 21.5).
#[derive(Clone, Debug)]
pub struct FrameInfo {
    /// Time of its last symbol.
    pub ended_at: Instant,
    /// The header.
    pub header: RxHeader,
    /// Width decoded.
    pub width: usize,
    /// Data blocks decoded / carried.
    pub blocks_ok: usize,
    /// Data blocks carried.
    pub blocks: usize,
    /// Control blocks decoded / carried.
    pub controls_ok: usize,
    /// Control blocks carried.
    pub controls: usize,
    /// 7 dB-equivalent channel SNR (ADR 0006).
    pub snr_db: f64,
    /// Decode time (wall), ms.
    pub decode_ms: f64,
    /// Sample-clock offset, ppm.
    pub ppm: f64,
    /// The header named a mask the station does not hold (ADR 0011).
    pub unknown_mask: bool,
    /// The frame's ACK block (B.2), when it carried one that decoded: in a
    /// session, the peer's report of this station's last frame.
    pub ack: Option<cairn_link::AckBlock>,
    /// This receiver's input clipped during the frame.
    pub clipped: bool,
    /// Capture gaps inside the frame (zeros in place of lost audio).
    pub audio_gap: Option<FrameGap>,
}

impl FrameInfo {
    /// One line.
    pub fn line(&self) -> String {
        let h = &self.header;
        format!(
            "{} > {} {:?}{} seq {} MCS {} x{} blocks {}/{} ctrl {}/{} SNR {:.1} dB clock {:+.1} ppm decode {:.0} ms{}",
            h.src,
            h.dst,
            h.ftype,
            if h.flags.session { " (session)" } else { "" },
            h.seq.0,
            h.mcs,
            self.width,
            self.blocks_ok,
            self.blocks,
            self.controls_ok,
            self.controls,
            self.snr_db,
            self.ppm,
            self.decode_ms,
            if self.unknown_mask { " unknown mask ID" } else { "" }
        ) + &self.audio_gap.map(|g| format!(" {}", g.text())).unwrap_or_default()
    }
}

/// What the modem has to say.
#[derive(Clone, Debug)]
pub enum Output {
    /// Key PTT now and send this.
    Transmit(TxRequest),
    /// A host event from the link layer.
    Host(HostEvent),
    /// A session PDU, in order, exactly once.
    Deliver {
        /// The peer.
        from: Address,
        /// The PDU.
        pdu: Pdu,
    },
    /// A broadcast PDU.
    DeliverBroadcast {
        /// The source.
        from: Address,
        /// BEACON or UI.
        ftype: FrameType,
        /// The PDU.
        pdu: Pdu,
    },
    /// Every decoded header.
    Heard(FrameInfo),
    /// A preamble whose header failed (toward the link layer as any header
    /// failure, also when a capture gap explains it).
    HeaderFailed {
        /// Its start.
        at: Instant,
        /// Capture gaps after its start.
        audio_gap: Option<FrameGap>,
    },
    /// A carrier-detect change (raw, no hold).
    Dcd {
        /// When.
        at: Instant,
        /// Each detector.
        state: DcdState,
    },
    /// A diagnostic line.
    Log(String),
}

/// Counters.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Samples taken.
    pub samples: u64,
    /// Preambles detected.
    pub detections: u64,
    /// Headers that failed.
    pub header_failures: u64,
    /// Frames decoded (header OK).
    pub frames: u64,
    /// Data blocks decoded / carried.
    pub blocks_ok: u64,
    /// Data blocks carried.
    pub blocks: u64,
    /// Session data blocks received again after they were delivered.
    pub dup_blocks: u64,
    /// Frames sent.
    pub tx_frames: u64,
    /// Time keyed.
    pub keyed: Duration,
    /// Raw DCD rising edges per detector: quieting, preamble, AFSK, COS.
    pub dcd_rises: [u64; 4],
    /// Turn-holder replies: from the peer frame's last symbol to keying.
    pub turnarounds: Vec<Duration>,
    /// Decode times (wall), ms.
    pub decode_ms: Vec<f64>,
    /// After each own transmission: how long the receiver took to hear the
    /// open-squelch noise again after PTT release (the radio's transmit
    /// tail; `cairn_phy::dcd::Dcd::take_recovery`), ms.
    pub rx_recovery_ms: Vec<f64>,
    /// Capture gaps (lost audio replaced by zeros).
    pub audio_gaps: u64,
    /// Samples in those gaps.
    pub audio_gap_samples: u64,
    /// Detections with a capture gap inside.
    pub gapped_frames: u64,
}

/// Per-tone SNR for the tone-mask rules (SPEC 10.3): the ADR 0006 channel
/// SNR fitted per tone against the rebuilt symbols where it was measured,
/// which is unbiased next to bad tones, unlike the decoder's smoothed
/// estimate (on the radio-like channel the decoder put tones 3-4 5-9 dB
/// below their neighbours when tones 0-2 were bad, against 1-4 dB measured
/// on the rebuilt symbols); tones without a fit take the decoder's estimate
/// shifted by the median difference between the two.
pub fn tone_snr(res: &cairn_phy::rx::RxResult) -> Vec<f64> {
    let Some(c) = &res.channel_snr else { return res.snr_db.clone() };
    let n = res.snr_db.len().min(c.per_tone_db.len());
    let mut diff: Vec<f64> = (0..n)
        .filter(|&k| c.per_tone_db[k].is_finite() && res.snr_db[k].is_finite())
        .map(|k| c.per_tone_db[k] - res.snr_db[k])
        .collect();
    if diff.is_empty() {
        return res.snr_db.clone();
    }
    diff.sort_by(f64::total_cmp);
    let shift = diff[diff.len() / 2];
    (0..res.snr_db.len())
        .map(|k| match c.per_tone_db.get(k) {
            Some(&v) if v.is_finite() => v,
            _ => res.snr_db[k] + shift,
        })
        .collect()
}

/// The session as last logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionView {
    peer: Address,
    tx_width: u8,
    rx_width: u8,
    tx_mask: (u64, u8),
    rx_mask: (u64, u8),
    tx_mcs: u8,
    rx_mcs: u8,
}

/// "tones 0-2 masked, 29 of 32 active" for a mask at a width.
pub fn describe_mask(mask: u64, width: u8) -> String {
    let w = width.min(56) as usize;
    let off: Vec<usize> = (0..w).filter(|&t| mask >> t & 1 == 0).collect();
    if off.is_empty() {
        return format!("all {w} tones active");
    }
    let mut runs: Vec<String> = Vec::new();
    let mut k = 0;
    while k < off.len() {
        let mut j = k;
        while j + 1 < off.len() && off[j + 1] == off[j] + 1 {
            j += 1;
        }
        runs.push(if j == k { off[k].to_string() } else { format!("{}-{}", off[k], off[j]) });
        k = j + 1;
    }
    format!("tones {} masked, {} of {w} active", runs.join(","), w - off.len())
}

/// What the receive path knows of the station between calls: the peer of
/// an open session, its width and the masks its Mask IDs name.
#[derive(Clone, Debug, Default)]
struct RxView {
    peer: Option<(Address, u8, [Option<u64>; 4])>,
    /// Starts of detections whose header named an unknown mask.
    masked_out: Vec<usize>,
    /// Starts of detections whose header failed.
    failed: Vec<usize>,
    /// Sources of detections whose header decoded, by start.
    sources: Vec<(usize, Address)>,
}

impl RxView {
    fn link_for(&self, h: &Header) -> (Option<LinkState>, bool) {
        let rh = header_from_phy(h);
        if matches!(rh.ftype, FrameType::Connect | FrameType::ConnectAck) || !rh.flags.session {
            return (None, false);
        }
        match self.peer {
            Some((peer, width, masks)) if peer == rh.src => {
                let mask = masks[rh.mask_id as usize & 3];
                let link = LinkState {
                    width: width as usize,
                    usable: mask.and_then(|m| mask_tones(m, width as usize)),
                    cp: None,
                    cap: Some(width as usize),
                    mask_id: rh.mask_id,
                    pending: None,
                };
                (Some(link), mask.is_none())
            }
            _ => (None, false),
        }
    }
}

/// Header-first configuration with the link state from the station
/// (as [`listen::HeaderFirst`], with the station's view and its failures
/// reported back).
struct StationSource {
    view: Arc<Mutex<RxView>>,
    rx: Receiver,
    opts: RxOptions,
    waiting: HashMap<usize, Header>,
}

impl ConfigSource for StationSource {
    fn lookahead(&self, det: &cairn_phy::detector::Detection) -> usize {
        header_samples(det.cp)
    }

    fn resolve(&mut self, det: &cairn_phy::detector::Detection, from_start: &[f64]) -> Resolution {
        let cp = det.cp;
        let header = match self.waiting.remove(&det.start) {
            Some(h) => h,
            None => {
                let head = self.rx.receive_header(from_start, 0, cp, &self.opts);
                match head.header {
                    Some(h) => h,
                    None => {
                        self.view.lock().expect("view").failed.push(det.start);
                        return Resolution::Fail(
                            head.header_error.map_or("header did not decode".into(), |e| e.to_string()),
                        );
                    }
                }
            }
        };
        let (link, masked_out) = {
            let mut v = self.view.lock().expect("view");
            v.sources.retain(|(s, _)| *s != det.start);
            v.sources.push((det.start, address_from_phy(&header.source)));
            v.link_for(&header)
        };
        if masked_out {
            self.view.lock().expect("view").masked_out.push(det.start);
        }
        let extent = if (link.is_none() || !header.session) && header.probe {
            let need = extent_samples(&header, cp) + 64;
            if from_start.len() < need {
                self.waiting.insert(det.start, header);
                return Resolution::More(need);
            }
            Some(self.rx.probe_extent(from_start, 0, cp, &header, &self.opts))
        } else {
            None
        };
        match FrameConfig::from_header(&header, &frame_link(&header, link.as_ref(), extent.as_ref()), cp) {
            Ok(cfg) => Resolution::Ready(Box::new(Plan { cfg, header: Some(header), link, extent })),
            Err(ConfigError::UnknownMask(id)) => Resolution::Fail(format!("mask ID {id} not held")),
            Err(e) => {
                self.view.lock().expect("view").failed.push(det.start);
                Resolution::Fail(format!("header describes no decodable frame: {e}"))
            }
        }
    }
}

/// The modem core.
pub struct Modem {
    cfg: ModemConfig,
    station: Station,
    codec: PhyCodec,
    tx: Transmitter,
    listener: Listener,
    jobs: ChanRx<Job>,
    rx: Receiver,
    view: Arc<Mutex<RxView>>,
    dcd: Dcd,
    dstate: DcdState,
    raw: bool,
    pos: usize,
    /// Time of sample `pos` (the next one).
    pos_time: Instant,
    /// Own transmission: keyed from, and muted until (FAR while keyed).
    mute: Option<(Instant, Instant)>,
    pre_up: bool,
    pre_hold_until: usize,
    clips: VecDeque<usize>,
    queued: VecDeque<(Instant, Event)>,
    wake: Option<Instant>,
    out: VecDeque<Output>,
    last_peer_frame: Option<(Address, Instant)>,
    now: Instant,
    /// What the session log last said (widths, masks, rungs).
    sess_seen: Option<SessionView>,
    /// The last seconds of input (SPEC 13.6).
    ring: AudioRing,
    /// The frame being received: its source and the index after its last sample.
    on_air: Option<(Address, usize, usize)>,
    /// Levels while the receiver is idle, for the AGC.
    idle_meter: LevelMeter,
    /// Level windows not yet taken.
    levels: Vec<LevelObs>,
    /// Counters.
    pub stats: Stats,
}

impl Modem {
    /// A modem with this configuration.
    pub fn new(cfg: ModemConfig) -> Self {
        let model = PhyFrameModel::new(cfg.txdelay, cfg.txtail, Duration::ZERO);
        let station = Station::new(cfg.station.clone(), Box::new(model));
        let view = Arc::new(Mutex::new(RxView::default()));
        let source = StationSource {
            view: view.clone(),
            rx: Receiver::new(),
            opts: RxOptions { rebuild_snr: false, threads: 1, ..cfg.rx.clone() },
            waiting: HashMap::new(),
        };
        let (job_tx, jobs) = channel();
        let listener = Listener::new(Box::new(source), cfg.detector.clone(), job_tx);
        let dcd = Dcd::new(cfg.dcd.clone());
        Modem {
            station,
            codec: PhyCodec,
            tx: Transmitter::new(),
            listener,
            jobs,
            rx: Receiver::new(),
            view,
            dcd,
            dstate: DcdState::default(),
            raw: false,
            pos: 0,
            pos_time: Instant::ZERO,
            mute: None,
            pre_up: false,
            pre_hold_until: 0,
            clips: VecDeque::new(),
            queued: VecDeque::new(),
            wake: None,
            out: VecDeque::new(),
            last_peer_frame: None,
            now: Instant::ZERO,
            sess_seen: None,
            ring: AudioRing::with_seconds(cfg.ring_seconds),
            on_air: None,
            idle_meter: LevelMeter::new(),
            levels: Vec::new(),
            stats: Stats::default(),
            cfg,
        }
    }

    /// The link layer.
    pub fn station(&self) -> &Station {
        &self.station
    }

    /// The link layer, for configuration changes (KISS parameters).
    pub fn station_mut(&mut self) -> &mut Station {
        &mut self.station
    }

    /// The configuration.
    pub fn config(&self) -> &ModemConfig {
        &self.cfg
    }

    /// New TXDelay / TXTail (KISS, SPEC 13.1): the frame model follows.
    pub fn set_timing(&mut self, txdelay: Duration, txtail: Duration) {
        self.cfg.txdelay = txdelay;
        self.cfg.txtail = txtail;
        self.station.set_frame_model(Box::new(PhyFrameModel::new(txdelay, txtail, Duration::ZERO)));
    }

    /// A new width cap in tones (24-56; the B.3 width cap is `cap` - 1,
    /// capability bit 0 above 24). Outside a session only. The quieting
    /// band follows when [`ModemConfig::quieting_follows_cap`].
    pub fn set_cap(&mut self, cap: u8) -> Result<(), String> {
        if !(24..=56).contains(&cap) {
            return Err("cap must be 24-56 tones".into());
        }
        if self.station.session_info().is_some() {
            return Err("cap cannot change during a session".into());
        }
        let c = self.station.config_mut();
        c.width_cap = cap - 1;
        if cap > 24 {
            c.capabilities |= cairn_link::frame::caps::WIDE;
        } else {
            c.capabilities &= !cairn_link::frame::caps::WIDE;
        }
        if self.cfg.quieting_follows_cap {
            self.cfg.dcd.quieting_band_hz = DcdConfig::for_width(cap as usize).quieting_band_hz;
            self.dcd = Dcd::new(self.cfg.dcd.clone());
            if self.dstate.quieting {
                let at = self.pos;
                self.apply_edges(vec![DcdEdge { source: DcdSource::Quieting, up: false, at }]);
            }
        }
        Ok(())
    }

    /// The raw-audio ring buffer (stream index = [`Modem::position`] - held).
    pub fn ring(&self) -> &AudioRing {
        &self.ring
    }

    /// The source of the frame on the air now (its header decoded, its last
    /// sample not yet in), for "who holds the channel".
    pub fn on_air(&self) -> Option<Address> {
        self.on_air.filter(|(_, _, end)| *end > self.pos).map(|(a, _, _)| a)
    }

    /// Raw detector states, as last reported.
    pub fn dcd_state(&self) -> DcdState {
        self.dstate
    }

    /// Carrier detect's state.
    pub fn dcd(&self) -> &Dcd {
        &self.dcd
    }

    /// Raw DCD (no hold), as last reported to the station.
    pub fn dcd_raw(&self) -> bool {
        self.raw
    }

    /// PTT is keyed for a transmission this modem asked for.
    pub fn keyed(&self) -> bool {
        self.mute.is_some_and(|(_, until)| until == Instant::FAR)
    }

    /// Time of input sample `idx` on the caller's clock.
    pub fn time_of(&self, idx: usize) -> Instant {
        let d = idx as f64 - self.pos as f64;
        let us = self.pos_time.0 as f64 + d * 1e6 / FS as f64;
        Instant(us.max(0.0).round() as u64)
    }

    /// Samples taken so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// A capture gap: `samples` zeros from stream index `at` stand in for
    /// lost audio. Call before pushing the chunk that holds them; a frame
    /// across the gap is flagged (`audio_gap`), and one whose header fails
    /// is a header failure like any other.
    pub fn note_gap(&mut self, at: usize, samples: usize) {
        self.stats.audio_gaps += 1;
        self.stats.audio_gap_samples += samples as u64;
        self.listener.note_gap(at, samples);
    }

    /// Nothing is being received: no carrier, no frame detected, queued or
    /// on the air, not keyed or just after. Receive gain may change now
    /// (SPEC 13.5: between frames only).
    pub fn rx_idle(&self) -> bool {
        !self.raw && !self.pre_up && self.listener.pending() == 0 && self.on_air().is_none() && self.mute.is_none()
    }

    /// Level windows since the last call: each received frame's levels and
    /// 0.5 s windows of idle input (open-squelch noise), for the AGC.
    pub fn take_levels(&mut self) -> Vec<LevelObs> {
        std::mem::take(&mut self.levels)
    }

    fn push_level(&mut self, l: LevelObs) {
        // Nobody may be taking them (no AGC): keep the newest only.
        if self.levels.len() >= 64 {
            self.levels.remove(0);
        }
        self.levels.push(l);
    }

    /// The receive gain changed by `db` (AGC): carrier detect's quieting
    /// floor follows, and the idle level window restarts.
    pub fn level_step(&mut self, db: f64) {
        self.dcd.level_step(db);
        self.idle_meter = LevelMeter::new();
    }

    /// When to call [`Modem::tick`] next.
    pub fn next_wakeup(&self) -> Option<Instant> {
        let q = self.queued.front().map(|(t, _)| *t);
        match (self.wake, q) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Everything said since the last call.
    pub fn drain(&mut self) -> Vec<Output> {
        self.out.drain(..).collect()
    }

    /// A host command.
    pub fn host(&mut self, cmd: HostCommand, now: Instant) {
        self.deliver_due(now);
        self.handle(Event::Host(cmd), now);
    }

    /// The PTT was released at `release` after the requested transmission.
    pub fn tx_done(&mut self, release: Instant, now: Instant) {
        if let Some((from, _)) = self.mute {
            self.stats.keyed += release.saturating_since(from);
            self.mute = Some((from, release + self.cfg.unmute_guard));
        }
        self.deliver_due(now);
        self.handle(Event::TxDone, release.max(self.now));
    }

    /// Timers.
    pub fn tick(&mut self, now: Instant) {
        self.deliver_due(now);
        if self.wake.is_some_and(|w| w <= now) {
            self.wake = None;
            self.handle(Event::Tick, now);
        }
    }

    /// Feed audio: `x` starts at `t_first` on the caller's clock; `now` is
    /// the time of the call.
    pub fn push_audio(&mut self, x: &[f64], t_first: Instant, now: Instant) {
        let start = self.pos;
        self.pos_time = t_first;
        self.pos = start;
        self.deliver_due(t_first);
        self.stats.samples += x.len() as u64;
        self.ring.push(x);
        // Half duplex: ignore input from keying to release (plus a guard).
        let dt = 1e6 / FS as f64;
        let muted: Vec<bool> = match self.mute {
            None => vec![false; x.len()],
            Some((from, until)) => (0..x.len())
                .map(|i| {
                    let t = Instant(t_first.0 + (i as f64 * dt) as u64);
                    t >= from && t <= until
                })
                .collect(),
        };
        let any_muted = muted.iter().any(|&m| m);
        if let Some((_, until)) = self.mute {
            if until != Instant::FAR && t_first > until {
                self.mute = None;
            }
        }
        for (i, &v) in x.iter().enumerate() {
            if v.abs() >= CLIP_LEVEL && !muted[i] {
                self.clips.push_back(start + i);
            }
        }
        while self.clips.len() > 64 || self.clips.front().is_some_and(|&c| c + 10 * FS as usize <= start) {
            self.clips.pop_front();
        }
        // Carrier detect.
        let mut edges = self.dcd.set_muted(any_muted);
        edges.extend(self.dcd.process(x));
        if let Some(r) = self.dcd.take_recovery() {
            self.stats.rx_recovery_ms.push((r + self.cfg.unmute_guard.as_secs_f64()) * 1e3);
        }
        // Preamble detector (silence while muted) and the frame pipeline.
        let rx_in: Vec<f64> =
            if any_muted { x.iter().zip(&muted).map(|(&v, &m)| if m { 0.0 } else { v }).collect() } else { x.to_vec() };
        let events = self.listener.push(&rx_in);
        // Headers decoded in this chunk: their source is on the air until
        // the frame's length is known (at most a frame's longest).
        for (s, a) in std::mem::take(&mut self.view.lock().expect("view").sources) {
            self.on_air = Some((a, s, s + ON_AIR_MAX));
        }
        self.pos = start + x.len();
        self.pos_time = Instant(t_first.0 + (x.len() as f64 * dt).round() as u64);
        if let Some(i) = self.listener.take_early() {
            self.pre_hold_until = self.pre_hold_until.max(i + EARLY_HOLD);
            if !self.pre_up {
                self.pre_up = true;
                edges.extend(self.dcd.set_preamble(true, i));
            }
        }
        for e in events {
            match e {
                listen::Event::Detected(d) => {
                    self.stats.detections += 1;
                    self.pre_hold_until = self.pre_hold_until.max(d.start + header_samples(d.cp) + 960);
                    if !self.pre_up {
                        self.pre_up = true;
                        edges.extend(self.dcd.set_preamble(true, d.start));
                    }
                }
                listen::Event::Resolved(d, n) => {
                    self.pre_hold_until = self.pre_hold_until.max(d.start + n);
                    if let Some((_, s, end)) = self.on_air.as_mut() {
                        if *s == d.start {
                            *end = d.start + n;
                        }
                    }
                }
                listen::Event::Unresolved(d, why, audio_gap) => {
                    if self.on_air.is_some_and(|(_, s, _)| s == d.start) {
                        self.on_air = None;
                    }
                    self.stats.gapped_frames += audio_gap.is_some() as u64;
                    let failed = {
                        let mut v = self.view.lock().expect("view");
                        let f = v.failed.contains(&d.start);
                        v.failed.retain(|&s| s != d.start);
                        f
                    };
                    if failed {
                        self.stats.header_failures += 1;
                        let at = self.time_of(d.start);
                        self.out.push_back(Output::HeaderFailed { at, audio_gap });
                        self.queue(at.max(self.time_of(d.start + header_samples(d.cp))), Event::HeaderFailed);
                    } else {
                        let gap = audio_gap.map(|g| format!(" ({})", g.text())).unwrap_or_default();
                        self.out.push_back(Output::Log(format!("undecoded frame: {why}{gap}")));
                    }
                }
                listen::Event::Queued(_) => {}
            }
        }
        if self.pre_up && self.pos >= self.pre_hold_until && self.listener.pending() == 0 {
            self.pre_up = false;
            edges.extend(self.dcd.set_preamble(false, self.pre_hold_until.max(start)));
        }
        self.apply_edges(edges);
        // Idle level windows for the AGC.
        if self.rx_idle() && !any_muted {
            self.idle_meter.push_f64(x);
            if self.idle_meter.samples() >= IDLE_WINDOW {
                let l = self.idle_meter.take();
                self.push_level(LevelObs {
                    kind: LevelKind::Idle,
                    at_s: self.pos_time.as_secs_f64(),
                    rms_dbfs: l.rms_dbfs,
                    peak_dbfs: l.peak_dbfs,
                    clipped: l.clipping(),
                });
            }
        } else if self.idle_meter.samples() > 0 {
            self.idle_meter = LevelMeter::new();
        }
        // Frames whose last sample is in: decode now.
        let chunk_end = self.pos_time;
        while let Ok(job) = self.jobs.try_recv() {
            self.decode(job, chunk_end);
        }
        self.deliver_due(now);
        if self.wake.is_some_and(|w| w <= now) {
            self.wake = None;
            self.handle(Event::Tick, now);
        }
    }

    /// Radio COS line, when wired.
    pub fn set_cos(&mut self, cos: Option<bool>) {
        let e = self.dcd.set_cos(cos, self.pos);
        self.apply_edges(e.into_iter().collect());
    }

    fn apply_edges(&mut self, mut edges: Vec<DcdEdge>) {
        edges.sort_by_key(|e| e.at);
        for e in edges {
            let s = &mut self.dstate;
            let slot = match e.source {
                DcdSource::Quieting => &mut s.quieting,
                DcdSource::Preamble => &mut s.preamble,
                DcdSource::Afsk => &mut s.afsk,
                DcdSource::Cos => &mut s.cos,
            };
            if *slot == e.up {
                continue;
            }
            *slot = e.up;
            let idx = match e.source {
                DcdSource::Quieting => 0,
                DcdSource::Preamble => 1,
                DcdSource::Afsk => 2,
                DcdSource::Cos => 3,
            };
            if e.up {
                self.stats.dcd_rises[idx] += 1;
            }
            let at = self.time_of(e.at);
            self.out.push_back(Output::Dcd { at, state: self.dstate });
            if e.up && matches!(e.source, DcdSource::Afsk) {
                self.handle(Event::LegacyActivity, at);
            }
            let raw = self.dstate.any();
            if raw != self.raw {
                self.raw = raw;
                self.handle(Event::Dcd(raw), at);
            }
        }
    }

    fn queue(&mut self, at: Instant, ev: Event) {
        let i = self.queued.iter().position(|(t, _)| *t > at).unwrap_or(self.queued.len());
        self.queued.insert(i, (at, ev));
    }

    fn deliver_due(&mut self, now: Instant) {
        while self.queued.front().is_some_and(|(t, _)| *t <= now) {
            let (t, ev) = self.queued.pop_front().expect("queued");
            self.handle(ev, t);
        }
    }

    fn decode(&mut self, job: Job, chunk_end: Instant) {
        let t0 = std::time::Instant::now();
        let res = listen::decode_job(&mut self.rx, &job, &self.cfg.rx);
        let wall = t0.elapsed();
        let decode_ms = wall.as_secs_f64() * 1e3;
        self.stats.decode_ms.push(decode_ms);
        let ready = match self.cfg.decode_time {
            DecodeTime::Wall(origin) => Instant(origin.elapsed().as_micros() as u64),
            DecodeTime::Measured => chunk_end + wall,
            DecodeTime::Fixed(d) => chunk_end + d,
        };
        let masked_out = {
            let mut v = self.view.lock().expect("view");
            let m = v.masked_out.contains(&job.det.start);
            v.masked_out.retain(|&s| s != job.det.start);
            m
        };
        let cfg = res.config.clone().unwrap_or_else(|| job.plan.cfg.clone());
        let end = job.det.start + cfg.n_samples();
        let ended_at = self.time_of(end);
        // The frame's own levels, for the AGC.
        let a = job.offset.min(job.samples.len());
        let b = (job.offset + cfg.n_samples()).min(job.samples.len());
        let mut meter = LevelMeter::new();
        meter.push_f64(&job.samples[a..b]);
        let l = meter.take();
        self.push_level(LevelObs {
            kind: LevelKind::Frame,
            at_s: ended_at.as_secs_f64(),
            rms_dbfs: l.rms_dbfs,
            peak_dbfs: l.peak_dbfs,
            clipped: l.clipping(),
        });
        self.stats.gapped_frames += job.gap.is_some() as u64;
        if !res.header_ok {
            self.stats.header_failures += 1;
            self.out.push_back(Output::HeaderFailed { at: self.time_of(job.det.start), audio_gap: job.gap });
            self.queue(ready, Event::HeaderFailed);
            return;
        }
        let snr = res.snr_7db_equivalent(&cfg).min(63.75);
        let clipped = self.clips.iter().any(|&c| c >= job.det.start && c < end);
        let meas = Measurement {
            snr_db: snr,
            distortion: 0,
            adc_clipped: clipped,
            tone_snr_db: Some(tone_snr(&res)),

            probe: cfg.has_probe,
        };
        let Ok(mut f) = self.codec.to_link(&res, meas, ended_at) else {
            self.out.push_back(Output::Log("decoded header did not convert".into()));
            return;
        };
        if masked_out {
            f.controls.iter_mut().for_each(|c| *c = None);
            f.data.iter_mut().for_each(|d| *d = None);
        }
        let blocks_ok = f.data.iter().filter(|d| d.is_some()).count();
        self.stats.frames += 1;
        self.stats.blocks_ok += blocks_ok as u64;
        self.stats.blocks += f.data.len() as u64;
        // Session blocks below the receiver's next in-order sequence were
        // delivered already: count them (§14.1 resume target).
        if let Some(info) = self.station.session_info() {
            if f.header.src == info.peer && f.header.flags.session && f.header.dst == self.station.address() {
                if let Some(seqs) = f.block_seqs() {
                    for (s, d) in seqs.iter().zip(&f.data) {
                        if d.is_some() && s.diff(info.rx_next) < 0 {
                            self.stats.dup_blocks += 1;
                        }
                    }
                }
            }
        }
        if f.header.dst == self.station.address() {
            self.last_peer_frame = Some((f.header.src, ended_at));
        }
        let info = FrameInfo {
            ended_at,
            header: f.header,
            width: cfg.width,
            blocks_ok,
            blocks: f.data.len(),
            controls_ok: f.controls.iter().filter(|c| c.is_some()).count(),
            controls: f.controls.len(),
            snr_db: snr,
            decode_ms,
            ppm: -res.drift / cfg.stride() as f64 * 1e6,
            unknown_mask: masked_out,
            ack: f.ack().copied(),
            clipped,
            audio_gap: job.gap,
        };
        self.out.push_back(Output::Heard(info));
        self.queue(ready, Event::Frame(f));
    }

    fn handle(&mut self, ev: Event, at: Instant) {
        if at > self.now {
            self.now = at;
        }
        let actions = self.station.handle(ev, at);
        let now = self.now;
        for a in actions {
            match a {
                Action::Transmit { frame, turn_holder } => self.transmit(frame, turn_holder, now),
                Action::Wakeup(t) => self.wake = Some(t),
                Action::Deliver { from, pdu } => self.out.push_back(Output::Deliver { from, pdu }),
                Action::DeliverBroadcast { from, ftype, pdu } => {
                    self.out.push_back(Output::DeliverBroadcast { from, ftype, pdu })
                }
                Action::Host(e) => self.out.push_back(Output::Host(e)),
            }
        }
        self.refresh_view();
        self.note_session();
    }

    /// Log the session's widths when it opens, and every mask and rung
    /// change in either direction.
    fn note_session(&mut self) {
        let Some(i) = self.station.session_info() else {
            self.sess_seen = None;
            return;
        };
        if !matches!(i.phase, Phase::Open | Phase::Closing { .. }) {
            return;
        }
        let v = SessionView {
            peer: i.peer,
            tx_width: i.tx_width,
            rx_width: i.rx_width,
            tx_mask: i.tx_mask,
            rx_mask: i.rx_mask,
            tx_mcs: i.tx_mcs,
            rx_mcs: i.requested_mcs,
        };
        let prev = self.sess_seen.replace(v);
        let mut lines = Vec::new();
        match prev {
            Some(p) if p.peer == v.peer => {
                if p.tx_mask != v.tx_mask {
                    lines.push(format!(
                        "tx mask ID {} from {}: {}",
                        v.tx_mask.1,
                        v.peer,
                        describe_mask(v.tx_mask.0, v.tx_width)
                    ));
                }
                if p.rx_mask != v.rx_mask {
                    lines.push(format!(
                        "rx mask report ID {} to {}: {}",
                        v.rx_mask.1,
                        v.peer,
                        describe_mask(v.rx_mask.0, v.rx_width)
                    ));
                }
                if p.tx_mcs != v.tx_mcs || p.rx_mcs != v.rx_mcs {
                    lines.push(format!(
                        "rungs with {}: tx MCS {} (was {}), rx MCS {} requested (was {})",
                        v.peer, v.tx_mcs, p.tx_mcs, v.rx_mcs, p.rx_mcs
                    ));
                }
            }
            _ => {
                let wide = |caps: u16| if caps & cairn_link::frame::caps::WIDE != 0 { "wide" } else { "not wide" };
                lines.push(format!(
                    "session width with {}: tx {} tones (own cap {}, peer {}), rx {} tones (peer cap {}, own {}); \
tx mask ID {}: {}; rx mask ID {}: {}; tx MCS {}, rx MCS {} requested",
                    v.peer,
                    v.tx_width,
                    self.station.config().width_cap,
                    wide(i.peer_capabilities),
                    v.rx_width,
                    i.peer_width_cap,
                    wide(self.station.config().capabilities),
                    v.tx_mask.1,
                    describe_mask(v.tx_mask.0, v.tx_width),
                    v.rx_mask.1,
                    describe_mask(v.rx_mask.0, v.rx_width),
                    v.tx_mcs,
                    v.rx_mcs
                ));
            }
        }
        for l in lines {
            self.out.push_back(Output::Log(l));
        }
    }

    fn refresh_view(&mut self) {
        let peer = self.station.session_info().and_then(|info| {
            let mut masks = [None; 4];
            let mut width = None;
            for id in 0..4u8 {
                let h = RxHeader {
                    ftype: FrameType::Data,
                    src: info.peer,
                    dst: self.station.address(),
                    flags: cairn_link::Flags { session: true, ..Default::default() },
                    block_map: false,
                    seq: cairn_link::Seq(0),
                    mcs: 0,
                    mask_id: id,
                };
                if let Some(l) = self.station.rx_link(&h) {
                    width = Some(l.width);
                    masks[id as usize] = l.mask;
                }
            }
            width.map(|w| (info.peer, w, masks))
        });
        self.view.lock().expect("view").peer = peer;
    }

    fn transmit(&mut self, frame: LinkFrame, turn_holder: bool, now: Instant) {
        let built =
            self.codec.to_phy(&frame).map_err(|e| format!("{e:?}")).and_then(|p| {
                self.tx.build_frame(&p.header, &p.controls, &p.data, &p.link).map_err(|e| format!("{e:?}"))
            });
        match built {
            Ok(tf) => {
                let audio: Vec<f32> = tf.output().iter().map(|&v| v as f32).collect();
                if turn_holder {
                    if let Some((peer, end)) = self.last_peer_frame {
                        if peer == frame.dst && now.saturating_since(end) < Duration::from_secs(2) {
                            self.stats.turnarounds.push(now.saturating_since(end));
                        }
                    }
                }
                self.stats.tx_frames += 1;
                self.mute = Some((now, Instant::FAR));
                self.out.push_back(Output::Transmit(TxRequest {
                    samples: audio.len(),
                    audio,
                    key_at: now,
                    frame,
                    turn_holder,
                }));
            }
            Err(e) => {
                // Never leave the station waiting for a release that will not come.
                self.out.push_back(Output::Log(format!("could not build {:?}: {e}", frame.ftype)));
                self.handle(Event::TxDone, now);
            }
        }
    }
}
