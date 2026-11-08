//! The native host API (SPEC 13.2) on the modem runtime: requests mapped
//! onto the link layer and the [`Modem`], runtime outputs turned into
//! events, the heard list, and diagnostic capture (`docs/api.md`).
//!
//! [`HostApi`] is transport-free: the caller hands it request lines and
//! every [`Output`] the modem produces, and takes back response lines and
//! events ([`HostApi::take_events`]) to send over `cairn_api::server`
//! (live, `driver`) or to inspect directly (the two-station simulator).
//! Times come from the modem's clock; wall-clock stamps are that clock
//! plus [`HostApiConfig::epoch_unix_us`].

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use cairn_api::capture::{SAMPLE_RATE, capture_paths, valid_label, write_wav};
use cairn_api::heard::{Heard, HeardList};
use cairn_api::proto::{
    API_VERSION, COMMANDS, EVENTS, MAX_PDU_BODY, Request, base64_decode, base64_encode, event, int_in, members,
    parse_request, parse_utc, round, utc_stamp, utc_string,
};
use cairn_api::{ApiError, Code};
use cairn_link::frame::snr_from_field;
use cairn_link::linktest::{DirectionResult, ProbeKind};
use cairn_link::pdu::{encoding, pdu_type};
use cairn_link::session::{Phase, Role};
use cairn_link::station::{WaitFor, WaitReason, Warning};
use cairn_link::{Address, ConnectFailure, FrameType, HostCommand, HostEvent, Instant, Pdu};
use cairn_phy::dcd::DcdState;
use serde_json::{Map, Value, json};

use crate::control::{FILE_CHUNK_DATA, parse_call};
use crate::runtime::{FrameInfo, Modem, Output};
use crate::rxdir::Saved;

/// Host ids of API sends start here (the line interface counts from 1,
/// KISS from 2^40).
const HOST_ID_BASE: u64 = 1 << 48;
/// Rate window for transfer progress.
const RATE_WINDOW: Duration = Duration::from_secs(30);
/// At most this often a `session` `progress` event.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// Frames in the block-error-rate window.
const BLER_FRAMES: usize = 16;
/// The channel state is looked at this often when nothing else happens.
const CHANNEL_EVERY: Duration = Duration::from_millis(10);
/// Heard file saved at most this often.
const HEARD_SAVE_EVERY: Duration = Duration::from_secs(30);
/// Settable `config` parameters.
const SETTABLE: [&str; 8] =
    ["persistence", "slot_ms", "max_frame_s", "broadcast_mcs", "cap", "txdelay_ms", "txtail_ms", "accept_connects"];
/// Read-only `config` parameters.
const READ_ONLY: [&str; 7] =
    ["callsign", "width_cap", "track_masks", "repeater", "ring_seconds", "quieting_hz", "settable"];

/// Settings.
#[derive(Clone, Debug)]
pub struct HostApiConfig {
    /// Where captures go.
    pub capture_dir: PathBuf,
    /// Keep the heard list in this file across restarts.
    pub heard_file: Option<PathBuf>,
    /// Unix microseconds at modem time zero.
    pub epoch_unix_us: i64,
}

impl HostApiConfig {
    /// Captures in `./captures`, no heard file, modem time zero is now.
    pub fn new() -> Self {
        HostApiConfig {
            capture_dir: PathBuf::from("captures"),
            heard_file: None,
            epoch_unix_us: cairn_api::proto::now_unix_us(),
        }
    }
}

impl Default for HostApiConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// An event to publish.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiEvent {
    /// Its name (for subscriptions).
    pub name: &'static str,
    /// The line.
    pub line: String,
}

/// The answer to one request line.
#[derive(Clone, Debug, PartialEq)]
pub struct Handled {
    /// The response line.
    pub response: String,
    /// A `subscribe` to apply: the events (None: all).
    pub subscribe: Option<Option<BTreeSet<String>>>,
}

#[derive(Clone, Debug)]
struct RefState {
    remaining: usize,
    bytes: usize,
    session: bool,
}

#[derive(Clone, Debug, Default)]
struct Progress {
    peer: Option<Address>,
    queued: u64,
    acked: u64,
    dropped: u64,
    delivered: u64,
    tx_win: VecDeque<(Instant, u64)>,
    rx_win: VecDeque<(Instant, u64)>,
    busy_since: Option<Instant>,
    rx_since: Option<Instant>,
    sent_blocks: u64,
    resent_blocks: u64,
    dirty: bool,
    force: bool,
    last_emit: Option<Instant>,
}

impl Progress {
    fn pending(&self) -> u64 {
        self.queued.saturating_sub(self.acked + self.dropped)
    }

    fn rate(win: &mut VecDeque<(Instant, u64)>, since: Option<Instant>, now: Instant) -> f64 {
        while win.front().is_some_and(|(t, _)| now.saturating_since(*t) > RATE_WINDOW) {
            win.pop_front();
        }
        let Some(since) = since else { return 0.0 };
        let span = now.saturating_since(since).min(RATE_WINDOW).as_secs_f64();
        if span < 1.0 {
            return 0.0;
        }
        win.iter().map(|(_, b)| *b).sum::<u64>() as f64 * 8.0 / span
    }

    fn snapshot(&mut self, now: Instant) -> Value {
        let tx_rate = Self::rate(&mut self.tx_win, self.busy_since, now);
        let rx_rate = Self::rate(&mut self.rx_win, self.rx_since, now);
        let pending = self.pending();
        let eta = (pending > 0 && tx_rate > 0.0).then(|| round(pending as f64 * 8.0 / tx_rate, 1));
        json!({
            "tx": {
                "queued_bytes": self.queued,
                "acked_bytes": self.acked,
                "pending_bytes": pending,
                "rate_bps": round(tx_rate, 1),
                "eta_s": eta,
            },
            "rx": {"delivered_bytes": self.delivered, "rate_bps": round(rx_rate, 1)},
            "retransmissions": self.resent_blocks,
            "blocks_sent": self.sent_blocks,
        })
    }
}

/// What a capture's sidecar needs from the recent past.
#[derive(Clone, Debug, Default)]
struct Diag {
    frames: VecDeque<FrameInfo>,
    fails: VecDeque<Instant>,
    dcd: VecDeque<(Instant, DcdState)>,
    dcd_before: DcdState,
    txs: VecDeque<(Instant, Value)>,
    keep: Duration,
}

impl Diag {
    fn trim(&mut self, now: Instant) {
        let cut = now - self.keep;
        while self.frames.front().is_some_and(|f| f.ended_at < cut) {
            self.frames.pop_front();
        }
        while self.fails.front().is_some_and(|t| *t < cut) {
            self.fails.pop_front();
        }
        while self.dcd.front().is_some_and(|(t, _)| *t < cut) {
            self.dcd_before = self.dcd.pop_front().expect("edge").1;
        }
        while self.txs.front().is_some_and(|(t, _)| *t < cut) {
            self.txs.pop_front();
        }
    }
}

/// The API's state on the modem side.
pub struct HostApi {
    cfg: HostApiConfig,
    heard: HeardList,
    diag: Diag,
    next_ref: u64,
    next_host_id: u64,
    next_object: u32,
    ids: HashMap<u64, (u64, usize)>,
    refs: HashMap<u64, RefState>,
    probe: Option<(u64, Address, ProbeKind)>,
    prog: Progress,
    bler: VecDeque<(usize, usize)>,
    channel_last: Option<String>,
    holder: Option<(String, Option<String>)>,
    channel_polled: Option<Instant>,
    heard_saved: Option<Instant>,
    events: Vec<ApiEvent>,
    now: Instant,
}

/// The SPEC 8.4 name of a frame type.
pub fn ftype_name(t: FrameType) -> String {
    match t {
        FrameType::Beacon => "BEACON".into(),
        FrameType::Ui => "UI".into(),
        FrameType::Connect => "CONNECT".into(),
        FrameType::ConnectAck => "CONNECT-ACK".into(),
        FrameType::Data => "DATA".into(),
        FrameType::Ack => "ACK".into(),
        FrameType::Sound => "SOUND".into(),
        FrameType::Disconnect => "DISCONNECT".into(),
        FrameType::Probe => "PROBE".into(),
        FrameType::CalRequest => "CAL-REQUEST".into(),
        FrameType::Poll => "POLL".into(),
        FrameType::Extended => "EXTENDED".into(),
        FrameType::Reserved(n) => format!("RESERVED-{n}"),
    }
}

/// Callsign text and SSID of an address (a hash: the printed address, 0).
fn call_ssid(a: Address) -> (String, u8) {
    match a.call_text() {
        Some(c) => (c, a.ssid()),
        None => (a.to_string(), 0),
    }
}

/// A session direction: rung, width, mask (to the width) and masked tones.
fn direction(mcs: u8, width: u8, mask: u64, mask_id: u8) -> Value {
    let w = width.min(56) as u32;
    let m = if w >= 64 { mask } else { mask & ((1u64 << w) - 1) };
    let masked: Vec<u32> = (0..w).filter(|&t| m >> t & 1 == 0).collect();
    json!({"mcs": mcs, "width": width, "mask_id": mask_id, "mask": format!("0x{m:016x}"), "masked": masked})
}

fn probe_dir(d: &DirectionResult) -> Value {
    json!({"snr_db": round(d.snr_db, 2), "mcs": d.mcs, "net_kbps": round(d.net_kbps, 3)})
}

fn detectors(s: &DcdState) -> Value {
    json!({"quieting": s.quieting, "preamble": s.preamble, "afsk": s.afsk, "cos": s.cos})
}

fn failure_name(r: ConnectFailure) -> &'static str {
    match r {
        ConnectFailure::Refused => "refused",
        ConnectFailure::NoAnswer => "no_answer",
        ConnectFailure::HalfOpenTimeout => "half_open_timeout",
        ConnectFailure::Busy => "busy",
        ConnectFailure::Cancelled => "cancelled",
    }
}

fn pdu_type_name(t: u8) -> &'static str {
    match t {
        pdu_type::AX25 => "ax25",
        pdu_type::MESSAGE => "message",
        pdu_type::FILE_CHUNK => "file_chunk",
        pdu_type::SIGNATURE => "signature",
        pdu_type::CONTROL => "control",
        _ => "unknown",
    }
}

fn encoding_name(e: u8) -> &'static str {
    match e {
        encoding::RAW => "raw",
        encoding::ZSTD => "zstd",
        encoding::ZSTD_DICT => "zstd_dict",
        _ => "unknown",
    }
}

fn wait_for_name(w: WaitFor) -> &'static str {
    match w {
        WaitFor::Connect => "connect",
        WaitFor::SessionReply => "session_reply",
        WaitFor::SessionData => "session_data",
        WaitFor::Keepalive => "keepalive",
        WaitFor::Disconnect => "disconnect",
        WaitFor::OwedReply => "owed_reply",
        WaitFor::Broadcast => "broadcast",
        WaitFor::Probe => "probe",
    }
}

fn wait_reason_name(r: WaitReason) -> &'static str {
    match r {
        WaitReason::Backoff => "backoff",
        WaitReason::Carrier => "carrier",
        WaitReason::CarrierGone => "carrier_gone",
        WaitReason::TurnCheck => "turn_check",
        WaitReason::YieldWindow => "yield_window",
        WaitReason::Persistence => "persistence",
        WaitReason::Clear => "clear",
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Connecting { .. } => "connecting",
        Phase::HalfOpen { .. } => "half_open",
        Phase::Open => "open",
        Phase::Closing { .. } => "closing",
    }
}

impl HostApi {
    /// A fresh API state (the heard list loaded from its file, if any).
    pub fn new(cfg: HostApiConfig, ring_seconds: f64) -> Result<Self, String> {
        let heard = match &cfg.heard_file {
            Some(p) => HeardList::load(p).map_err(|e| format!("heard file {}: {e}", p.display()))?,
            None => HeardList::new(),
        };
        let keep = Duration::from_secs_f64(ring_seconds.max(20.0) + 10.0);
        Ok(HostApi {
            cfg,
            heard,
            diag: Diag { keep, ..Default::default() },
            next_ref: 1,
            next_host_id: HOST_ID_BASE,
            next_object: 1,
            ids: HashMap::new(),
            refs: HashMap::new(),
            probe: None,
            prog: Progress::default(),
            bler: VecDeque::new(),
            channel_last: None,
            holder: None,
            channel_polled: None,
            heard_saved: None,
            events: Vec::new(),
            now: Instant::ZERO,
        })
    }

    /// Unix microseconds of a modem time.
    pub fn unix_us(&self, t: Instant) -> i64 {
        self.cfg.epoch_unix_us + t.0 as i64
    }

    /// The heard list.
    pub fn heard(&self) -> &HeardList {
        &self.heard
    }

    /// Everything to publish since the last call.
    pub fn take_events(&mut self) -> Vec<ApiEvent> {
        std::mem::take(&mut self.events)
    }

    fn push(&mut self, name: &'static str, members: Value) {
        let v = event(name, self.unix_us(self.now), self.now.as_secs_f64(), self::members(members));
        self.events.push(ApiEvent { name, line: v.to_string() });
    }

    fn tick(&mut self, now: Instant) {
        if now > self.now {
            self.now = now;
        }
    }

    /// The `hello` line for a new client.
    pub fn hello(&mut self, client: u64, modem: &Modem, now: Instant) -> String {
        self.tick(now);
        let mut unsupported = vec!["calibrate"];
        if modem.ring().capacity() == 0 {
            unsupported.push("capture");
        }
        event(
            "hello",
            self.unix_us(self.now),
            self.now.as_secs_f64(),
            members(json!({
                "api": "cairn",
                "api_version": API_VERSION,
                "implementation": {"name": "cairn-modem", "version": env!("CARGO_PKG_VERSION")},
                "station": modem.station().address().to_string(),
                "client": client,
                "commands": COMMANDS,
                "unsupported": unsupported,
                "events": EVENTS,
                "encodings": ["raw"],
            })),
        )
        .to_string()
    }

    // --- Requests ------------------------------------------------------------------

    /// Answer one request line.
    pub fn request(&mut self, line: &str, modem: &mut Modem, now: Instant) -> Handled {
        self.tick(now);
        let req = match parse_request(line) {
            Ok(r) => r,
            Err(rej) => return Handled { response: rej.response().to_string(), subscribe: None },
        };
        let mut subscribe = None;
        let result = match req.cmd.as_str() {
            "connect" => self.cmd_connect(&req, modem),
            "disconnect" => self.cmd_disconnect(&req, modem),
            "send" => self.cmd_send(&req, modem),
            "beacon" => self.cmd_beacon(&req, modem),
            "probe" => self.cmd_probe(&req, modem),
            "heard" => self.cmd_heard(&req),
            "status" => Ok(self.status(modem)),
            "config" => self.cmd_config(&req, modem),
            "calibrate" => Err(ApiError::new(
                Code::Unsupported,
                "calibrate is not implemented in this modem (no host level control yet)",
            )),
            "capture" => {
                let label = req.opt_str("label");
                match (req.opt_positive("seconds"), label) {
                    (Ok(s), Ok(l)) => self.capture(modem, s, l, "api"),
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            }
            "subscribe" => match parse_subscription(&req) {
                Ok(set) => {
                    let names: Vec<&str> = match &set {
                        None => EVENTS.to_vec(),
                        Some(s) => EVENTS.iter().copied().filter(|e| s.contains(*e)).collect(),
                    };
                    subscribe = Some(set);
                    Ok(members(json!({"events": names})))
                }
                Err(e) => Err(e),
            },
            other => Err(ApiError::new(Code::UnknownCommand, format!("unknown command {other:?}"))),
        };
        let response = match result {
            Ok(m) => req.ok(m),
            Err(e) => {
                subscribe = None;
                req.err(&e)
            }
        };
        Handled { response: response.to_string(), subscribe }
    }

    fn host_id(&mut self) -> u64 {
        self.next_host_id += 1;
        self.next_host_id
    }

    fn new_ref(&mut self) -> u64 {
        let r = self.next_ref;
        self.next_ref += 1;
        r
    }

    fn cmd_connect(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let peer = parse_call(req.req_str("peer")?).map_err(ApiError::bad)?;
        let resume = req.opt_bool("resume")?.unwrap_or(true);
        if let Some(i) = modem.station().session_info() {
            return Err(ApiError::new(Code::Busy, format!("already in a session with {}", i.peer)));
        }
        let resuming = resume && modem.station().has_retained(peer);
        if !resuming || self.prog.peer != Some(peer) {
            // A new session: transfer progress starts over.
            self.prog = Progress { peer: Some(peer), ..Default::default() };
        }
        modem.host(HostCommand::Connect { peer, new_session: !resume }, self.now);
        Ok(members(json!({"peer": peer.to_string(), "resuming": resuming})))
    }

    fn cmd_disconnect(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let graceful = req.opt_bool("graceful")?.unwrap_or(true);
        let Some(i) = modem.station().session_info() else {
            return Err(ApiError::new(Code::NoSession, "no session"));
        };
        modem.host(HostCommand::Disconnect { graceful }, self.now);
        if matches!(i.phase, Phase::Open | Phase::HalfOpen { .. }) {
            self.push(
                "session",
                json!({"kind": "state", "peer": i.peer.to_string(), "state": "closing", "session_id": i.session_id,
                    "initiator": null, "resumed": null, "reason": null}),
            );
        }
        Ok(members(json!({"peer": i.peer.to_string(), "graceful": graceful})))
    }

    fn cmd_send(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let session = match req.opt_str("mode")?.unwrap_or("session") {
            "session" => true,
            "broadcast" => false,
            m => return Err(ApiError::bad(format!("mode must be \"session\" or \"broadcast\", not {m:?}"))),
        };
        let text = req.opt_str("text")?;
        let data = req.opt_str("data")?;
        let file = req.opt_str("file")?;
        if [text.is_some(), data.is_some(), file.is_some()].iter().filter(|&&b| b).count() != 1 {
            return Err(ApiError::bad("exactly one of text, data and file is required"));
        }
        let ptype = match req.opt_str("pdu_type")?.unwrap_or("message") {
            "message" => pdu_type::MESSAGE,
            "ax25" => pdu_type::AX25,
            t => return Err(ApiError::bad(format!("pdu_type must be \"message\" or \"ax25\", not {t:?}"))),
        };
        if file.is_some() && req.get("pdu_type").is_some() {
            return Err(ApiError::bad("a file is always sent as FILE-CHUNK PDUs; pdu_type applies to text and data"));
        }
        let stream = req.opt_int("stream", 0, 63)?.unwrap_or(0) as u8;
        let priority = req.opt_int("priority", 0, 3)?.unwrap_or(if file.is_some() { 3 } else { 0 }) as u8;
        let repeats = req.opt_int("repeats", 0, 3)?.unwrap_or(0) as u8;
        if session && req.get("repeats").is_some() {
            return Err(ApiError::bad("repeats applies to broadcasts only"));
        }
        if let Some(d) = req.opt_str("dictionary")? {
            if d.len() != 8 || !d.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                return Err(ApiError::bad("dictionary is 8 lower-case hex digits"));
            }
        }
        match req.opt_str("encoding")?.unwrap_or("auto") {
            "auto" | "raw" => {}
            "zstd" | "zstd_dict" => {
                return Err(ApiError::new(
                    Code::Unsupported,
                    "this modem has no zstd codec or dictionaries yet (SPEC 12.2); use \"raw\" or \"auto\"",
                ));
            }
            e => return Err(ApiError::bad(format!("encoding must be auto, raw, zstd or zstd_dict, not {e:?}"))),
        }
        if session && modem.station().session_info().is_none() {
            return Err(ApiError::new(
                Code::NoSession,
                "no session: connect first, or send with \"mode\":\"broadcast\"",
            ));
        }
        let stream_byte = priority << 6 | stream;
        // (PDU, payload bytes) list.
        let mut pdus: Vec<(Pdu, usize)> = Vec::new();
        let mut object = None;
        if let Some(path) = file {
            let bytes = std::fs::read(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ApiError::new(Code::NotFound, format!("{path}: no such file")),
                _ => ApiError::new(Code::IoError, format!("{path}: {e}")),
            })?;
            let obj = self.next_object;
            self.next_object += 1;
            object = Some(obj);
            let mut chunks: Vec<&[u8]> = bytes.chunks(FILE_CHUNK_DATA).collect();
            if bytes.len() % FILE_CHUNK_DATA == 0 {
                chunks.push(&[]);
            }
            for (k, c) in chunks.into_iter().enumerate() {
                let mut body = Vec::with_capacity(c.len() + 8);
                body.extend_from_slice(&obj.to_be_bytes());
                body.extend_from_slice(&((k * FILE_CHUNK_DATA) as u32).to_be_bytes());
                body.extend_from_slice(c);
                let pdu = Pdu {
                    pdu_type: pdu_type::FILE_CHUNK,
                    stream: stream_byte,
                    encoding: encoding::RAW,
                    dictionary_id: None,
                    body,
                    crc: None,
                };
                pdus.push((pdu, c.len()));
            }
        } else {
            let body = match (text, data) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (_, Some(d)) => base64_decode(d).ok_or_else(|| ApiError::bad("data is not valid base64"))?,
                _ => unreachable!(),
            };
            if body.len() > MAX_PDU_BODY {
                return Err(ApiError::new(
                    Code::TooLarge,
                    format!("{} bytes do not fit one PDU ({MAX_PDU_BODY}); send them as a file", body.len()),
                ));
            }
            let n = body.len();
            let pdu = Pdu {
                pdu_type: ptype,
                stream: stream_byte,
                encoding: encoding::RAW,
                dictionary_id: None,
                body,
                crc: None,
            };
            pdus.push((pdu, n));
        }
        let r = self.new_ref();
        let total: usize = pdus.iter().map(|(_, n)| n).sum();
        let count = pdus.len();
        self.refs.insert(r, RefState { remaining: count, bytes: total, session });
        if session {
            self.note_queued(modem, total as u64);
        }
        for (pdu, n) in pdus {
            let id = self.host_id();
            self.ids.insert(id, (r, n));
            let cmd = if session { HostCommand::Send { id, pdu } } else { HostCommand::Broadcast { id, pdu, repeats } };
            modem.host(cmd, self.now);
        }
        let mut m = members(json!({
            "ref": r,
            "mode": if session { "session" } else { "broadcast" },
            "bytes": total,
            "pdus": count,
            "encoding": "raw",
        }));
        if let Some(o) = object {
            m.insert("object".into(), json!(o));
        }
        Ok(m)
    }

    fn note_queued(&mut self, modem: &Modem, bytes: u64) {
        let peer = modem.station().session_info().map(|i| i.peer);
        if self.prog.peer != peer {
            self.prog = Progress { peer, ..Default::default() };
        }
        if self.prog.pending() == 0 {
            self.prog.busy_since = Some(self.now);
            self.prog.tx_win.clear();
        }
        self.prog.queued += bytes;
        self.prog.dirty = true;
    }

    fn cmd_beacon(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let text = req.opt_str("text")?;
        let repeats = req.opt_int("repeats", 0, 3)?.unwrap_or(0) as u8;
        let pdu = text.map(|t| Pdu::message(t.as_bytes().to_vec()));
        if pdu.as_ref().is_some_and(|p| p.body.len() > MAX_PDU_BODY) {
            return Err(ApiError::new(Code::TooLarge, "beacon text too long"));
        }
        let r = self.new_ref();
        let id = self.host_id();
        self.ids.insert(id, (r, text.map_or(0, str::len)));
        self.refs.insert(r, RefState { remaining: 1, bytes: text.map_or(0, str::len), session: false });
        modem.host(HostCommand::Beacon { id, pdu, repeats }, self.now);
        Ok(members(json!({"ref": r})))
    }

    fn cmd_probe(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let target = parse_call(req.req_str("target")?).map_err(ApiError::bad)?;
        let kind = match req.opt_str("kind")?.unwrap_or("link_test") {
            "link_test" => ProbeKind::LinkTest,
            "beacon" => ProbeKind::Beacon,
            k => return Err(ApiError::bad(format!("kind must be \"link_test\" or \"beacon\", not {k:?}"))),
        };
        let repeats = req.opt_int("repeats", 0, 5)?.unwrap_or(2) as u8;
        let r = self.new_ref();
        self.probe = Some((r, target, kind));
        modem.host(HostCommand::Probe { target, kind, repeats }, self.now);
        Ok(members(json!({
            "ref": r,
            "target": target.to_string(),
            "kind": if kind == ProbeKind::LinkTest { "link_test" } else { "beacon" },
        })))
    }

    fn cmd_heard(&mut self, req: &Request) -> Result<Map<String, Value>, ApiError> {
        let since = match req.opt_str("since")? {
            Some(s) => Some(parse_utc(s).ok_or_else(|| ApiError::bad("since must be an RFC 3339 UTC time"))?),
            None => None,
        };
        let limit = req.opt_int("limit", 1, i64::MAX)?.map(|n| n as usize);
        let list: Vec<Value> = self.heard.list(since, limit).iter().map(|e| e.to_json()).collect();
        Ok(members(json!({"stations": list})))
    }

    /// The `status` result members.
    pub fn status(&mut self, modem: &Modem) -> Map<String, Value> {
        let st = modem.station();
        let s = &modem.stats;
        let session = match st.session_info() {
            Some(i) => {
                if self.prog.peer == Some(i.peer) {
                    self.prog.sent_blocks = i.sent_blocks;
                    self.prog.resent_blocks = i.resent_blocks;
                }
                let progress = self.prog.snapshot(self.now);
                json!({
                    "peer": i.peer.to_string(),
                    "phase": phase_name(i.phase),
                    "role": if i.role == Role::Initiator { "initiator" } else { "responder" },
                    "session_id": i.session_id,
                    "tx": direction(i.tx_mcs, i.tx_width, i.tx_mask.0, i.tx_mask.1),
                    "rx": direction(i.requested_mcs, i.rx_width, i.rx_mask.0, i.rx_mask.1),
                    "idle": i.idle,
                    "drained": i.drained,
                    "progress": progress,
                })
            }
            None => Value::Null,
        };
        let d = modem.dcd_state();
        members(json!({
            "station": st.address().to_string(),
            "time": utc_string(self.unix_us(self.now)),
            "modem_time": round(self.now.as_secs_f64(), 3),
            "session": session,
            "channel": self.channel_state(modem),
            "dcd": {
                "raw": modem.dcd_raw(),
                "detectors": detectors(&d),
                "quieting_floor_dbfs": modem.dcd().floor_dbfs().map(|f| round(f, 2)),
            },
            "counters": {
                "samples": s.samples,
                "detections": s.detections,
                "header_failures": s.header_failures,
                "frames": s.frames,
                "blocks_ok": s.blocks_ok,
                "blocks": s.blocks,
                "tx_frames": s.tx_frames,
                "keyed_s": round(s.keyed.as_secs_f64(), 3),
                "dcd_rises": {
                    "quieting": s.dcd_rises[0], "preamble": s.dcd_rises[1], "afsk": s.dcd_rises[2], "cos": s.dcd_rises[3],
                },
            },
            "capture": {
                "ring_seconds": round(modem.ring().seconds(), 3),
                "buffered_seconds": round(modem.ring().len() as f64 / SAMPLE_RATE as f64, 3),
            },
            "heard_stations": self.heard.len(),
        }))
    }

    /// The `config` object.
    pub fn config(&self, modem: &Modem) -> Value {
        let c = modem.station().config();
        let m = modem.config();
        json!({
            "callsign": c.address.to_string(),
            "persistence": round(c.persistence, 6),
            "slot_ms": c.slot_time.as_millis() as u64,
            "max_frame_s": round(c.max_frame_time.as_secs_f64(), 3),
            "broadcast_mcs": c.broadcast_mcs,
            "cap": c.width_cap as u32 + 1,
            "txdelay_ms": m.txdelay.as_millis() as u64,
            "txtail_ms": m.txtail.as_millis() as u64,
            "accept_connects": c.accept_connects,
            "width_cap": c.width_cap,
            "track_masks": c.track_masks,
            "repeater": c.repeater,
            "ring_seconds": round(modem.ring().seconds(), 3),
            "quieting_hz": [m.dcd.quieting_band_hz.0, m.dcd.quieting_band_hz.1],
            "settable": SETTABLE,
        })
    }

    fn cmd_config(&mut self, req: &Request, modem: &mut Modem) -> Result<Map<String, Value>, ApiError> {
        let mut changed = Vec::new();
        if let Some(set) = req.get("set") {
            let set = set.as_object().ok_or_else(|| ApiError::bad("set must be an object"))?;
            if set.is_empty() {
                return Err(ApiError::bad("set names no parameter"));
            }
            // Check everything first: all or nothing.
            enum Change {
                Persistence(f64),
                Slot(u64),
                MaxFrame(f64),
                BroadcastMcs(u8),
                Cap(u8),
                TxDelay(u64),
                TxTail(u64),
                Accept(bool),
            }
            let num = |k: &str, v: &Value| v.as_f64().ok_or_else(|| ApiError::bad(format!("{k} must be a number")));
            let int = |k: &str, v: &Value, lo: i64, hi: i64| -> Result<i64, ApiError> {
                if v.as_i64().is_none() {
                    return Err(ApiError::bad(format!("{k} must be an integer")));
                }
                int_in(v, k, lo, hi).map_err(|e| ApiError::new(Code::OutOfRange, e.message))
            };
            let range = |k: &str, msg: &str| ApiError::new(Code::OutOfRange, format!("{k} must be {msg}"));
            let mut plan = Vec::new();
            for (k, v) in set {
                let c = match k.as_str() {
                    "persistence" => {
                        let p = num(k, v)?;
                        if !(p > 0.0 && p <= 1.0) {
                            return Err(range(k, "in (0, 1]"));
                        }
                        Change::Persistence(p)
                    }
                    "slot_ms" => Change::Slot(int(k, v, 10, 2550)? as u64),
                    "max_frame_s" => {
                        let f = num(k, v)?;
                        if !(0.5..=4.0).contains(&f) {
                            return Err(range(k, "0.5-4.0 s"));
                        }
                        Change::MaxFrame(f)
                    }
                    "broadcast_mcs" => Change::BroadcastMcs(int(k, v, 0, 3)? as u8),
                    "cap" => {
                        let c = int(k, v, 24, 56)? as u8;
                        if modem.station().session_info().is_some() {
                            return Err(ApiError::new(Code::InvalidState, "cap cannot change during a session"));
                        }
                        Change::Cap(c)
                    }
                    "txdelay_ms" => Change::TxDelay(int(k, v, 0, 2550)? as u64),
                    "txtail_ms" => Change::TxTail(int(k, v, 0, 2550)? as u64),
                    "accept_connects" => {
                        Change::Accept(v.as_bool().ok_or_else(|| ApiError::bad("accept_connects must be a boolean"))?)
                    }
                    k if READ_ONLY.contains(&k) => {
                        return Err(ApiError::new(Code::ReadOnly, format!("{k} cannot change while running")));
                    }
                    k => return Err(ApiError::bad(format!("unknown parameter {k:?}"))),
                };
                plan.push(c);
                changed.push(k.clone());
            }
            for c in plan {
                match c {
                    Change::Persistence(p) => modem.station_mut().config_mut().persistence = p,
                    Change::Slot(ms) => modem.station_mut().config_mut().slot_time = Duration::from_millis(ms),
                    Change::MaxFrame(f) => modem.station_mut().config_mut().max_frame_time = Duration::from_secs_f64(f),
                    Change::BroadcastMcs(m) => modem.station_mut().config_mut().broadcast_mcs = m,
                    Change::Cap(c) => modem.set_cap(c).map_err(|e| ApiError::new(Code::InvalidState, e))?,
                    Change::TxDelay(ms) => {
                        let tail = modem.config().txtail;
                        modem.set_timing(Duration::from_millis(ms), tail);
                    }
                    Change::TxTail(ms) => {
                        let delay = modem.config().txdelay;
                        modem.set_timing(delay, Duration::from_millis(ms));
                    }
                    Change::Accept(b) => modem.station_mut().config_mut().accept_connects = b,
                }
            }
        }
        Ok(members(json!({"config": self.config(modem), "changed": changed})))
    }

    // --- Capture -------------------------------------------------------------------

    /// Save the last `seconds` of the ring (all of it by default) as a WAV
    /// file with a JSON sidecar of decoder state; returns the `capture`
    /// result members. `reason`: `api`, `control` or `auto`.
    pub fn capture(
        &mut self,
        modem: &Modem,
        seconds: Option<f64>,
        label: Option<&str>,
        reason: &str,
    ) -> Result<Map<String, Value>, ApiError> {
        let ring = modem.ring();
        if ring.capacity() == 0 {
            return Err(ApiError::new(Code::Unsupported, "no raw-audio ring buffer (run with --ring-seconds 0)"));
        }
        if let Some(l) = label {
            if !valid_label(l) {
                return Err(ApiError::bad("label must be 1-32 of A-Z a-z 0-9 _ -"));
            }
        }
        let want = seconds.map_or(ring.capacity(), |s| (s * SAMPLE_RATE as f64).round() as usize);
        let (start, samples) = ring.last(want);
        let start_t = modem.time_of(start as usize);
        let end_t = modem.time_of(modem.position());
        std::fs::create_dir_all(&self.cfg.capture_dir)
            .map_err(|e| ApiError::new(Code::IoError, format!("{}: {e}", self.cfg.capture_dir.display())))?;
        let created = self.unix_us(self.now);
        let stem = match label {
            Some(l) => format!("cairn-{}-{l}", utc_stamp(created)),
            None => format!("cairn-{}", utc_stamp(created)),
        };
        let (wav, sidecar) = capture_paths(&self.cfg.capture_dir, &stem);
        write_wav(&wav, &samples).map_err(|e| ApiError::new(Code::IoError, format!("{}: {e}", wav.display())))?;
        self.diag.trim(self.now);
        let rel = |t: Instant| round(t.as_secs_f64() - start_t.as_secs_f64(), 6);
        let inside = |t: Instant| t >= start_t && t <= end_t;
        let frames: Vec<Value> = self
            .diag
            .frames
            .iter()
            .filter(|f| inside(f.ended_at))
            .map(|f| {
                let h = &f.header;
                json!({
                    "t": rel(f.ended_at),
                    "modem_time": round(f.ended_at.as_secs_f64(), 6),
                    "src": h.src.to_string(),
                    "dst": h.dst.to_string(),
                    "frame_type": ftype_name(h.ftype),
                    "session": h.flags.session,
                    "seq": h.seq.0,
                    "mcs": h.mcs,
                    "width": f.width,
                    "blocks": f.blocks,
                    "blocks_ok": f.blocks_ok,
                    "controls": f.controls,
                    "controls_ok": f.controls_ok,
                    "snr_db": round(f.snr_db, 2),
                    "ppm": round(f.ppm, 2),
                    "decode_ms": round(f.decode_ms, 2),
                    "unknown_mask": f.unknown_mask,
                })
            })
            .collect();
        let fails: Vec<Value> = self
            .diag
            .fails
            .iter()
            .filter(|t| inside(**t))
            .map(|t| json!({"t": rel(*t), "modem_time": round(t.as_secs_f64(), 6)}))
            .collect();
        let mut state = self.diag.dcd_before;
        for (t, s) in &self.diag.dcd {
            if *t < start_t {
                state = *s;
            }
        }
        let mut dcd = vec![json!({"t": 0.0, "modem_time": round(start_t.as_secs_f64(), 6), "raw": state.any(),
            "detectors": detectors(&state)})];
        for (t, s) in self.diag.dcd.iter().filter(|(t, _)| inside(*t)) {
            dcd.push(json!({"t": rel(*t), "modem_time": round(t.as_secs_f64(), 6), "raw": s.any(), "detectors": detectors(s)}));
        }
        let txs: Vec<Value> = self
            .diag
            .txs
            .iter()
            .filter(|(t, _)| inside(*t))
            .map(|(t, v)| {
                let mut v = v.clone();
                v["t"] = json!(rel(*t));
                v["modem_time"] = json!(round(t.as_secs_f64(), 6));
                v
            })
            .collect();
        let doc = json!({
            "format": "cairn-capture/1",
            "station": modem.station().address().to_string(),
            "created": utc_string(created),
            "reason": reason,
            "wav": wav.file_name().map(|n| n.to_string_lossy().to_string()),
            "sample_rate": SAMPLE_RATE,
            "samples": samples.len(),
            "start_sample": start,
            "start_modem_time": round(start_t.as_secs_f64(), 6),
            "end_modem_time": round(end_t.as_secs_f64(), 6),
            "frames": frames,
            "header_failures": fails,
            "dcd": dcd,
            "transmissions": txs,
            "link": self.status(modem),
            "config": self.config(modem),
        });
        let text = serde_json::to_string_pretty(&doc).map_err(|e| ApiError::new(Code::Internal, e.to_string()))?;
        std::fs::write(&sidecar, text + "\n")
            .map_err(|e| ApiError::new(Code::IoError, format!("{}: {e}", sidecar.display())))?;
        Ok(members(json!({
            "wav": wav.display().to_string(),
            "sidecar": sidecar.display().to_string(),
            "seconds": round(samples.len() as f64 / SAMPLE_RATE as f64, 6),
            "samples": samples.len(),
            "sample_rate": SAMPLE_RATE,
            "start_modem_time": round(start_t.as_secs_f64(), 6),
        })))
    }

    // --- Outputs -------------------------------------------------------------------

    /// One output of the modem (`saved`: where `--rx-dir` put a delivered PDU).
    pub fn on_output(&mut self, o: &Output, saved: Option<&Saved>, modem: &Modem, now: Instant) {
        self.tick(now);
        match o {
            Output::Transmit(req) => {
                let f = &req.frame;
                let keyed = req.keyed(modem.config().txdelay, modem.config().txtail).as_secs_f64();
                let v = json!({
                    "frame_type": ftype_name(f.ftype),
                    "dst": f.dst.to_string(),
                    "seq": f.seq.0,
                    "mcs": f.mcs,
                    "width": f.tx.width,
                    "blocks": f.data.len(),
                    "turn_holder": req.turn_holder,
                    "keyed_s": round(keyed, 3),
                });
                self.diag.txs.push_back((req.key_at, v.clone()));
                let mut m = members(v);
                m.insert("kind".into(), json!("start"));
                self.push("tx", Value::Object(m));
            }
            Output::Host(e) => self.on_host_event(e, modem),
            Output::Deliver { from, pdu } => {
                let bytes = payload_len(pdu);
                if self.prog.peer.is_none() {
                    self.prog.peer = Some(*from);
                }
                if self.prog.rx_win.is_empty() {
                    self.prog.rx_since = Some(self.now);
                }
                self.prog.delivered += bytes as u64;
                self.prog.rx_win.push_back((self.now, bytes as u64));
                self.prog.dirty = true;
                self.push("rx", rx_event(*from, "session", FrameType::Data, pdu, saved));
            }
            Output::DeliverBroadcast { from, ftype, pdu } => {
                self.push("rx", rx_event(*from, "broadcast", *ftype, pdu, saved));
            }
            Output::Heard(f) => self.on_heard(f, modem),
            Output::HeaderFailed { at } => self.diag.fails.push_back(*at),
            Output::Dcd { at, state } => {
                self.diag.dcd.push_back((*at, *state));
                self.push(
                    "dcd",
                    json!({"raw": state.any(), "detectors": detectors(state), "at": round(at.as_secs_f64(), 3)}),
                );
            }
            Output::Log(_) => {}
        }
        self.diag.trim(self.now);
    }

    /// PTT released: the measured times (None in a simulator).
    pub fn on_tx_done(&mut self, measured: Option<(f64, f64, f64)>, now: Instant) {
        self.tick(now);
        let (k, d, t) = match measured {
            Some((k, d, t)) => (json!(round(k, 1)), json!(round(d, 1)), json!(round(t, 1))),
            None => (Value::Null, Value::Null, Value::Null),
        };
        self.push("tx", json!({"kind": "done", "keyed_ms": k, "txdelay_ms": d, "txtail_ms": t}));
    }

    /// A transmission failed (PTT released).
    pub fn on_tx_failed(&mut self, message: &str, now: Instant) {
        self.tick(now);
        self.error(Code::TxFailed, message, false, None, None);
    }

    fn error(&mut self, code: Code, message: &str, warning: bool, r: Option<u64>, peer: Option<Address>) {
        self.push(
            "error",
            json!({
                "code": code.as_str(),
                "message": message,
                "severity": if warning { "warning" } else { "error" },
                "ref": r,
                "peer": peer.map(|p| p.to_string()),
            }),
        );
    }

    fn state_event(&mut self, peer: Address, state: &str, sid: Option<u16>, init: Option<bool>, res: Option<bool>) {
        self.push(
            "session",
            json!({"kind": "state", "peer": peer.to_string(), "state": state, "session_id": sid,
                "initiator": init, "resumed": res, "reason": null}),
        );
    }

    fn on_host_event(&mut self, e: &HostEvent, modem: &Modem) {
        let sid = modem.station().session_info().map(|i| i.session_id);
        match e {
            HostEvent::Connecting { peer } => {
                self.push(
                    "session",
                    json!({"kind": "state", "peer": peer.to_string(), "state": "connecting", "session_id": null,
                        "initiator": true, "resumed": null, "reason": null}),
                );
            }
            HostEvent::Connected { peer, session_id, resumed, initiator } => {
                if !initiator && !resumed {
                    self.prog = Progress { peer: Some(*peer), ..Default::default() };
                }
                self.bler.clear();
                self.state_event(*peer, "open", Some(*session_id), Some(*initiator), Some(*resumed));
            }
            HostEvent::ConnectFailed { peer, reason } => {
                self.push(
                    "session",
                    json!({"kind": "state", "peer": peer.to_string(), "state": "failed", "session_id": sid,
                        "initiator": null, "resumed": null, "reason": failure_name(*reason)}),
                );
            }
            HostEvent::Refused { peer } => self.state_event(*peer, "refused", None, None, None),
            HostEvent::LinkLost { peer } => self.state_event(*peer, "link_lost", sid, None, None),
            HostEvent::Disconnected { peer } => self.state_event(*peer, "disconnected", sid, None, None),
            HostEvent::SessionExpired { peer } => self.state_event(*peer, "expired", None, None, None),
            HostEvent::Undelivered { peer, ids } => {
                let mut refs = BTreeSet::new();
                let mut bytes = 0usize;
                for id in ids {
                    if let Some((r, n)) = self.ids.remove(id) {
                        refs.insert(r);
                        bytes += n;
                    }
                }
                for r in &refs {
                    self.refs.remove(r);
                }
                self.ids.retain(|_, (r, _)| !refs.contains(r));
                if !refs.is_empty() {
                    self.prog.dropped += bytes as u64;
                    self.prog.dirty = true;
                    self.prog.force = true;
                    self.push(
                        "session",
                        json!({"kind": "undelivered", "peer": peer.to_string(), "refs": refs, "bytes": bytes}),
                    );
                }
            }
            HostEvent::Acked { peer, ids } => {
                for id in ids {
                    let Some((r, n)) = self.ids.remove(id) else { continue };
                    self.prog.acked += n as u64;
                    self.prog.tx_win.push_back((self.now, n as u64));
                    self.prog.dirty = true;
                    let done = match self.refs.get_mut(&r) {
                        Some(st) => {
                            st.remaining = st.remaining.saturating_sub(1);
                            st.remaining == 0
                        }
                        None => false,
                    };
                    if done {
                        let st = self.refs.remove(&r).expect("ref");
                        self.prog.force = true;
                        self.push(
                            "session",
                            json!({"kind": "delivered", "peer": peer.to_string(), "ref": r, "bytes": st.bytes}),
                        );
                    }
                }
            }
            HostEvent::SendRejected { id } => {
                if let Some((r, _)) = self.ids.remove(id) {
                    let st = self.refs.remove(&r);
                    self.ids.retain(|_, (x, _)| *x != r);
                    if let Some(st) = st.filter(|s| s.session) {
                        self.prog.dropped += st.bytes as u64;
                        self.prog.dirty = true;
                    }
                    self.error(
                        Code::SendRejected,
                        "the link layer refused the send (no session)",
                        false,
                        Some(r),
                        None,
                    );
                }
            }
            HostEvent::BroadcastSent { ids } => {
                let mut done = Vec::new();
                for id in ids {
                    let Some((r, _)) = self.ids.remove(id) else { continue };
                    if let Some(st) = self.refs.get_mut(&r) {
                        st.remaining = st.remaining.saturating_sub(1);
                        if st.remaining == 0 {
                            self.refs.remove(&r);
                            done.push(r);
                        }
                    }
                }
                if !done.is_empty() {
                    self.push("tx", json!({"kind": "broadcast_sent", "refs": done}));
                }
            }
            HostEvent::BroadcastRejected { id, error } => {
                if let Some((r, _)) = self.ids.remove(id) {
                    self.refs.remove(&r);
                    self.ids.retain(|_, (x, _)| *x != r);
                    self.error(
                        Code::BroadcastRejected,
                        &format!("broadcast could not be framed: {error:?}"),
                        false,
                        Some(r),
                        None,
                    );
                }
            }
            HostEvent::LinkTest(res) => {
                let r = match self.probe {
                    Some((r, t, _)) if t == res.target => {
                        self.probe = None;
                        Some(r)
                    }
                    _ => None,
                };
                self.push(
                    "link",
                    json!({"kind": "probe", "ref": r, "target": res.target.to_string(), "answered": true,
                        "forward": probe_dir(&res.forward), "reverse": probe_dir(&res.reverse)}),
                );
            }
            HostEvent::Unreachable { target } => {
                let r = match self.probe {
                    Some((r, t, _)) if t == *target => {
                        self.probe = None;
                        Some(r)
                    }
                    _ => None,
                };
                self.push(
                    "link",
                    json!({"kind": "probe", "ref": r, "target": target.to_string(), "answered": false,
                        "forward": null, "reverse": null}),
                );
            }
            HostEvent::BeaconHeard { from, .. } => {
                if let Some((r, t, ProbeKind::Beacon)) = self.probe {
                    if t == *from {
                        self.probe = None;
                        self.push(
                            "link",
                            json!({"kind": "probe", "ref": r, "target": from.to_string(), "answered": true,
                                "forward": null, "reverse": null}),
                        );
                    }
                }
            }
            HostEvent::PduError { peer, error } => self.error(
                Code::PduError,
                &format!("PDU from {peer} not delivered: {error:?}"),
                false,
                None,
                Some(*peer),
            ),
            HostEvent::Warning(Warning::ReceiveClipping { peer }) => self.error(
                Code::ReceiveClipping,
                &format!("receive input clipping on frames from {peer}; lower the receive level"),
                true,
                None,
                Some(*peer),
            ),
            HostEvent::Warning(Warning::Distortion { peer, index }) => self.error(
                Code::Distortion,
                &format!("{peer} reports distortion index {index}; lower the transmit drive"),
                true,
                None,
                Some(*peer),
            ),
        }
    }

    fn on_heard(&mut self, f: &FrameInfo, modem: &Modem) {
        self.diag.frames.push_back(f.clone());
        let h = &f.header;
        let (call, ssid) = call_ssid(h.src);
        let entry = self.heard.update(&Heard {
            address: h.src.to_string(),
            call: call.clone(),
            ssid,
            dst: h.dst.to_string(),
            frame_type: ftype_name(h.ftype),
            mode: "cairn".into(),
            mcs: h.mcs,
            width: f.width as u8,
            snr_db: f.snr_db,
            unix_us: self.unix_us(f.ended_at),
            modem_time: f.ended_at.as_secs_f64(),
        });
        let count = entry.count;
        self.push(
            "heard",
            json!({
                "address": h.src.to_string(),
                "call": call,
                "ssid": ssid,
                "dst": h.dst.to_string(),
                "frame_type": ftype_name(h.ftype),
                "mode": "cairn",
                "session": h.flags.session,
                "mcs": h.mcs,
                "width": f.width,
                "snr_db": round(f.snr_db, 2),
                "ppm": round(f.ppm, 2),
                "blocks": f.blocks,
                "blocks_ok": f.blocks_ok,
                "count": count,
            }),
        );
        // The link dashboard: frames from the session peer.
        let st = modem.station();
        let Some(i) = st.session_info() else { return };
        if !(h.flags.session && h.src == i.peer && h.dst == st.address()) {
            return;
        }
        if f.blocks > 0 {
            self.bler.push_back((f.blocks, f.blocks - f.blocks_ok));
            while self.bler.len() > BLER_FRAMES {
                self.bler.pop_front();
            }
        }
        let (nb, ne) = self.bler.iter().fold((0, 0), |(a, b), (x, y)| (a + x, b + y));
        let bler = if nb == 0 { 0.0 } else { ne as f64 / nb as f64 };
        let report = f.ack.or(i.peer_report);
        let (tx_mcs, tx_mask) = match &f.ack {
            Some(a) => (a.requested_mcs.min(6), if a.mask_present { (a.mask, a.mask_id) } else { i.tx_mask }),
            None => (i.tx_mcs, i.tx_mask),
        };
        let far = report.map(|a| {
            json!({"snr_db": round(snr_from_field(a.snr), 2), "block_errors": a.block_errors,
                "distortion": a.distortion.min(7), "requested_mcs": a.requested_mcs.min(7), "clipped": a.adc_clipped})
        });
        self.push(
            "link",
            json!({
                "kind": "frame",
                "peer": i.peer.to_string(),
                "frame_type": ftype_name(h.ftype),
                "seq": h.seq.0,
                "snr_db": round(f.snr_db, 2),
                "ppm": round(f.ppm, 2),
                "frame_mcs": h.mcs,
                "blocks": f.blocks,
                "block_errors": f.blocks - f.blocks_ok,
                "bler": round(bler, 4),
                "rx": direction(i.requested_mcs, i.rx_width, i.rx_mask.0, i.rx_mask.1),
                "tx": direction(tx_mcs, i.tx_width, tx_mask.0, tx_mask.1),
                "far_end": far,
                "distortion": 0,
                "clipped": f.clipped,
            }),
        );
    }

    // --- Polling -------------------------------------------------------------------

    /// The `channel` object (`docs/api.md`).
    pub fn channel_state(&self, modem: &Modem) -> Value {
        let st = modem.station();
        let ci = st.channel_info();
        let info = st.session_info();
        let peer = info.as_ref().map(|i| i.peer);
        // Who holds the channel: this station while keyed; the source of
        // the frame on the air; while carrier detect stays up after that
        // frame (its tail, the DCD hold) still that station; carrier with
        // no header decoded is "unknown".
        let last = self.holder.clone().filter(|(h, _)| h == "peer" || h == "other");
        let (holder, addr) = if modem.keyed() || ci.transmitting {
            ("self".to_string(), None)
        } else if let Some(a) = modem.on_air() {
            ((if Some(a) == peer { "peer" } else { "other" }).to_string(), Some(a.to_string()))
        } else if modem.dcd_raw() || ci.dcd_raw || ci.dcd_held {
            last.unwrap_or(("unknown".to_string(), None))
        } else {
            ("none".to_string(), None)
        };
        let turn = info.as_ref().and_then(|i| {
            (matches!(i.phase, Phase::Open | Phase::Closing { .. }) && !i.idle).then_some(if i.owes_reply {
                "self"
            } else {
                "peer"
            })
        });
        let waiting = ci.waiting.map(|w| {
            json!({
                "for": wait_for_name(w.what),
                "reason": wait_reason_name(w.reason),
                "until_s": w.until.map(|t| round(t.saturating_since(self.now).as_secs_f64(), 3)),
            })
        });
        let next_yield =
            info.as_ref().and_then(|i| i.next_yield).map(|t| round(t.saturating_since(self.now).as_secs_f64(), 3));
        json!({
            "dcd": ci.dcd_held,
            "dcd_raw": ci.dcd_raw,
            "keyed": modem.keyed(),
            "holder": holder,
            "holder_address": addr,
            "turn": turn,
            "waiting": waiting,
            "next_yield_s": next_yield,
            "legacy": ci.legacy,
        })
    }

    /// Timers: `channel` changes, throttled `progress`, the heard file.
    /// Call after every batch of outputs and now and then.
    pub fn poll(&mut self, modem: &Modem, now: Instant) {
        self.tick(now);
        // Channel: on a change of its discrete members, looked at whenever
        // something happened and every 10 ms otherwise.
        let due = self.channel_polled.is_none_or(|t| self.now.saturating_since(t) >= CHANNEL_EVERY);
        if due || !self.events.is_empty() {
            self.channel_polled = Some(self.now);
            self.poll_channel(modem);
        }
        self.poll_progress(modem);
        // Heard file.
        if self.cfg.heard_file.is_some()
            && self.heard.dirty()
            && self.heard_saved.is_none_or(|t| self.now.saturating_since(t) >= HEARD_SAVE_EVERY)
        {
            self.heard_saved = Some(self.now);
            if let Err(e) = self.save_heard() {
                self.error(Code::IoError, &e, false, None, None);
            }
        }
    }

    fn poll_channel(&mut self, modem: &Modem) {
        let c = self.channel_state(modem);
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            c["dcd"],
            c["dcd_raw"],
            c["keyed"],
            c["holder"],
            c["holder_address"],
            c["turn"],
            c["waiting"]["for"],
            c["waiting"]["reason"],
            c["legacy"]
        );
        if self.channel_last.as_deref() != Some(&key) {
            self.channel_last = Some(key);
            self.holder = Some((
                c["holder"].as_str().unwrap_or("none").to_string(),
                c["holder_address"].as_str().map(str::to_string),
            ));
            self.push("channel", c);
        }
    }

    fn poll_progress(&mut self, modem: &Modem) {
        if let Some(i) = modem.station().session_info() {
            if self.prog.peer == Some(i.peer)
                && (i.sent_blocks != self.prog.sent_blocks || i.resent_blocks != self.prog.resent_blocks)
            {
                self.prog.sent_blocks = i.sent_blocks;
                self.prog.resent_blocks = i.resent_blocks;
                self.prog.dirty = true;
            }
        }
        if self.prog.pending() == 0 && self.prog.queued > 0 && self.prog.dirty {
            self.prog.force = true;
        }
        let due = self.prog.last_emit.is_none_or(|t| self.now.saturating_since(t) >= PROGRESS_EVERY);
        if self.prog.dirty && (due || self.prog.force) {
            if let Some(peer) = self.prog.peer {
                self.prog.dirty = false;
                self.prog.force = false;
                self.prog.last_emit = Some(self.now);
                let p = self.prog.snapshot(self.now);
                self.push("session", json!({"kind": "progress", "peer": peer.to_string(), "progress": p}));
            }
        }
    }

    /// Write the heard file now (when configured and changed).
    pub fn save_heard(&mut self) -> Result<(), String> {
        let Some(p) = self.cfg.heard_file.clone() else { return Ok(()) };
        if !self.heard.dirty() {
            return Ok(());
        }
        self.heard.save(&p).map_err(|e| format!("heard file {}: {e}", p.display()))
    }
}

/// Payload bytes of a PDU (FILE-CHUNK: the data after object and offset).
fn payload_len(pdu: &Pdu) -> usize {
    if pdu.pdu_type == pdu_type::FILE_CHUNK { pdu.body.len().saturating_sub(8) } else { pdu.body.len() }
}

fn rx_event(from: Address, mode: &str, ftype: FrameType, pdu: &Pdu, saved: Option<&Saved>) -> Value {
    let (data, extra) = if pdu.pdu_type == pdu_type::FILE_CHUNK && pdu.body.len() >= 8 {
        let obj = u32::from_be_bytes(pdu.body[0..4].try_into().expect("4 bytes"));
        let off = u32::from_be_bytes(pdu.body[4..8].try_into().expect("4 bytes"));
        (&pdu.body[8..], Some((obj, off)))
    } else {
        (&pdu.body[..], None)
    };
    let mut m = members(json!({
        "from": from.to_string(),
        "mode": mode,
        "frame_type": ftype_name(ftype),
        "pdu_type": pdu_type_name(pdu.pdu_type),
        "pdu_type_code": pdu.pdu_type,
        "stream": pdu.stream & 63,
        "priority": pdu.stream >> 6,
        "encoding": encoding_name(pdu.encoding),
        "dictionary": pdu.dictionary_id.map(|d| d.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        "decoded": pdu.encoding == encoding::RAW,
        "crc": "ok",
        "verify": "unsigned",
        "bytes": data.len(),
        "data": base64_encode(data),
    }));
    if pdu.pdu_type == pdu_type::MESSAGE && pdu.encoding == encoding::RAW {
        if let Ok(t) = std::str::from_utf8(data) {
            m.insert("text".into(), json!(t));
        }
    }
    if let Some((o, off)) = extra {
        m.insert("object".into(), json!(o));
        m.insert("offset".into(), json!(off));
    }
    if let Some(s) = saved {
        let mut sv =
            members(json!({"path": s.path.display().to_string(), "complete": s.completed.is_some() || s.message}));
        if let Some(c) = &s.completed {
            sv.insert("bytes".into(), json!(c.bytes));
            sv.insert("sha256".into(), json!(c.sha256));
        }
        m.insert("saved".into(), Value::Object(sv));
    }
    Value::Object(m)
}

fn parse_subscription(req: &Request) -> Result<Option<BTreeSet<String>>, ApiError> {
    match req.get("events") {
        Some(Value::String(s)) if s == "all" => Ok(None),
        Some(Value::Array(a)) => {
            let mut set = BTreeSet::new();
            for e in a {
                match e.as_str() {
                    Some(n) if EVENTS.contains(&n) => {
                        set.insert(n.to_string());
                    }
                    _ => return Err(ApiError::bad(format!("unknown event {e}"))),
                }
            }
            Ok(Some(set))
        }
        _ => Err(ApiError::bad("events must be \"all\" or an array of event names")),
    }
}
