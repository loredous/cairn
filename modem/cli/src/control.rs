//! A minimal line-based control interface (the native API of SPEC 13.2 is
//! not built yet): commands in, one line per host event out.
//!
//! ```text
//! connect CALL[-SSID] [new]   open a session (resumes retained state unless "new")
//! send TEXT                   a MESSAGE PDU in the session
//! send @PATH                  a file as FILE-CHUNK PDUs (object ID, offset, data)
//! disconnect [now]            close (graceful: after everything queued is acknowledged)
//! probe CALL [beacon]         link test (or a BEACON request)
//! beacon [TEXT]               a BEACON (ID block, optional status text)
//! ui TEXT                     a UI broadcast carrying a MESSAGE PDU
//! status                      session, rung, carrier detect, counters
//! quit
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use cairn_link::adapter::address_from_phy;
use cairn_link::linktest::ProbeKind;
use cairn_link::pdu::{encoding, pdu_type};
use cairn_link::station::Warning;
use cairn_link::{Address, ConnectFailure, FrameType, HostCommand, HostEvent, Pdu};

use crate::runtime::{Modem, Output};

/// FILE-CHUNK data bytes per PDU.
pub const FILE_CHUNK_DATA: usize = 2048;

/// A parsed control line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// Open a session.
    Connect {
        /// The peer.
        peer: Address,
        /// Discard retained state.
        new_session: bool,
    },
    /// Send text.
    SendText(String),
    /// Send a file.
    SendFile(PathBuf),
    /// Close.
    Disconnect {
        /// Wait for the queue to drain.
        graceful: bool,
    },
    /// Link test or BEACON request.
    Probe {
        /// The target.
        target: Address,
        /// Which.
        kind: ProbeKind,
    },
    /// A BEACON.
    Beacon(Option<String>),
    /// A UI broadcast.
    Ui(String),
    /// Status.
    Status,
    /// Help.
    Help,
    /// Stop the modem.
    Quit,
}

/// `CALL` or `CALL-SSID` (longer callsigns are carried as a hash, B.1).
pub fn parse_call(s: &str) -> Result<Address, String> {
    let (call, ssid) = match s.rsplit_once('-') {
        Some((c, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
            (c, n.parse::<u8>().map_err(|_| format!("bad SSID in {s}"))?)
        }
        _ => (s, 0),
    };
    let a = cairn_phy::fields::Address::new(&call.to_ascii_uppercase(), ssid).map_err(|e| format!("{s}: {e}"))?;
    Ok(address_from_phy(&a))
}

/// Parse one line.
pub fn parse(line: &str) -> Result<Control, String> {
    let line = line.trim();
    let (cmd, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(c, r)| (c, r.trim()));
    let words: Vec<&str> = rest.split_whitespace().collect();
    match cmd.to_ascii_lowercase().as_str() {
        "connect" | "c" => {
            let peer = parse_call(words.first().ok_or("connect CALL[-SSID] [new]")?)?;
            Ok(Control::Connect { peer, new_session: words.get(1).is_some_and(|w| w.eq_ignore_ascii_case("new")) })
        }
        "send" | "s" => {
            if let Some(p) = rest.strip_prefix('@') {
                Ok(Control::SendFile(PathBuf::from(p)))
            } else if rest.is_empty() {
                Err("send TEXT | send @PATH".into())
            } else {
                Ok(Control::SendText(rest.to_string()))
            }
        }
        "disconnect" | "d" => Ok(Control::Disconnect { graceful: !words.first().is_some_and(|w| *w == "now") }),
        "probe" | "p" => {
            let target = parse_call(words.first().ok_or("probe CALL [beacon]")?)?;
            let kind = if words.get(1).is_some_and(|w| w.eq_ignore_ascii_case("beacon")) {
                ProbeKind::Beacon
            } else {
                ProbeKind::LinkTest
            };
            Ok(Control::Probe { target, kind })
        }
        "beacon" | "b" => Ok(Control::Beacon((!rest.is_empty()).then(|| rest.to_string()))),
        "ui" => {
            if rest.is_empty() {
                Err("ui TEXT".into())
            } else {
                Ok(Control::Ui(rest.to_string()))
            }
        }
        "status" | "st" => Ok(Control::Status),
        "help" | "?" => Ok(Control::Help),
        "quit" | "exit" | "q" => Ok(Control::Quit),
        "" => Err(String::new()),
        other => Err(format!("unknown command {other:?} (help)")),
    }
}

/// Help text.
pub const HELP: &str = "commands: connect CALL[-SSID] [new] | send TEXT | send @PATH | disconnect [now] | \
probe CALL [beacon] | beacon [TEXT] | ui TEXT | status | quit";

/// The host side: ids, what each send was, received files.
#[derive(Default)]
pub struct HostState {
    next_id: u64,
    next_object: u32,
    /// Send label by id, and the ids each send still waits for.
    sends: Vec<(String, Vec<u64>, usize)>,
    /// Received FILE-CHUNK data by (source, object): bytes so far.
    files: HashMap<(Address, u32), usize>,
}

impl HostState {
    /// A fresh state.
    pub fn new() -> Self {
        HostState { next_object: 1, ..Default::default() }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The link commands for a control line (status, help and quit are the
    /// caller's), with a line to print.
    pub fn commands(&mut self, c: &Control) -> Result<(Vec<HostCommand>, String), String> {
        Ok(match c {
            Control::Connect { peer, new_session } => {
                // The link layer reports "connecting" itself.
                (vec![HostCommand::Connect { peer: *peer, new_session: *new_session }], String::new())
            }
            Control::SendText(t) => {
                let id = self.id();
                self.sends.push((format!("message {id} ({} bytes)", t.len()), vec![id], t.len()));
                (
                    vec![HostCommand::Send { id, pdu: Pdu::message(t.as_bytes().to_vec()) }],
                    format!("queued message {id}"),
                )
            }
            Control::SendFile(p) => {
                let data = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
                let (cmds, ids, obj) = self.file_commands(&data);
                self.sends.push((
                    format!("file {} (object {obj}, {} bytes)", p.display(), data.len()),
                    ids,
                    data.len(),
                ));
                let n = cmds.len();
                (cmds, format!("queued {} as object {obj}: {} bytes in {n} PDUs", p.display(), data.len()))
            }
            Control::Disconnect { graceful } => {
                (vec![HostCommand::Disconnect { graceful: *graceful }], "disconnecting".to_string())
            }
            Control::Probe { target, kind } => (
                vec![HostCommand::Probe { target: *target, kind: *kind, repeats: 2 }],
                format!("probing {target} ({kind:?})"),
            ),
            Control::Beacon(text) => {
                let id = self.id();
                let pdu = text.as_ref().map(|t| Pdu::message(t.as_bytes().to_vec()));
                (vec![HostCommand::Beacon { id, pdu, repeats: 0 }], format!("queued beacon {id}"))
            }
            Control::Ui(text) => {
                let id = self.id();
                (
                    vec![HostCommand::Broadcast { id, pdu: Pdu::message(text.as_bytes().to_vec()), repeats: 0 }],
                    format!("queued UI {id}"),
                )
            }
            Control::Status | Control::Help | Control::Quit => (vec![], String::new()),
        })
    }

    /// A file as FILE-CHUNK PDUs (SPEC 12.4: 32-bit object ID, 32-bit
    /// offset, then data), with their ids and the object ID. Every chunk
    /// but the last carries [`FILE_CHUNK_DATA`] bytes; a file whose length
    /// is a multiple of that (or empty) ends with an empty chunk at its
    /// length, so a receiver knows where the object ends (this modem's
    /// convention: the protocol carries no object length; `rxdir`).
    pub fn file_commands(&mut self, data: &[u8]) -> (Vec<HostCommand>, Vec<u64>, u32) {
        let obj = self.next_object;
        self.next_object += 1;
        let mut cmds = Vec::new();
        let mut ids = Vec::new();
        let mut chunks: Vec<&[u8]> = data.chunks(FILE_CHUNK_DATA).collect();
        if data.len() % FILE_CHUNK_DATA == 0 {
            chunks.push(&[]);
        }
        for (k, chunk) in chunks.into_iter().enumerate() {
            let mut body = Vec::with_capacity(chunk.len() + 8);
            body.extend_from_slice(&obj.to_be_bytes());
            body.extend_from_slice(&((k * FILE_CHUNK_DATA) as u32).to_be_bytes());
            body.extend_from_slice(chunk);
            let pdu = Pdu {
                pdu_type: pdu_type::FILE_CHUNK,
                stream: 0,
                encoding: encoding::RAW,
                dictionary_id: None,
                body,
                crc: None,
            };
            let id = self.id();
            ids.push(id);
            cmds.push(HostCommand::Send { id, pdu });
        }
        (cmds, ids, obj)
    }

    /// One line for an output worth showing the operator (None for the rest).
    pub fn describe(&mut self, o: &Output) -> Option<String> {
        Some(match o {
            Output::Host(e) => self.host_event(e)?,
            Output::Deliver { from, pdu } => match pdu.pdu_type {
                pdu_type::FILE_CHUNK if pdu.body.len() >= 8 => {
                    let obj = u32::from_be_bytes(pdu.body[0..4].try_into().expect("4 bytes"));
                    let off = u32::from_be_bytes(pdu.body[4..8].try_into().expect("4 bytes"));
                    let total = self.files.entry((*from, obj)).or_default();
                    *total += pdu.body.len() - 8;
                    format!(
                        "rx file from {from}: object {obj} offset {off} +{} bytes ({} so far)",
                        pdu.body.len() - 8,
                        total
                    )
                }
                _ => format!("rx from {from}: {}", text_or_len(&pdu.body)),
            },
            Output::DeliverBroadcast { from, ftype, pdu } => match pdu.pdu_type {
                pdu_type::AX25 => format!("{ftype:?} from {from}: AX.25 frame, {} bytes", pdu.body.len()),
                _ => format!("{ftype:?} from {from}: {}", text_or_len(&pdu.body)),
            },
            _ => return None,
        })
    }

    fn host_event(&mut self, e: &HostEvent) -> Option<String> {
        Some(match e {
            HostEvent::Connecting { peer } => format!("connecting to {peer}"),
            HostEvent::Connected { peer, session_id, resumed, initiator } => format!(
                "connected to {peer} (session {session_id:04x}, {}{})",
                if *initiator { "initiator" } else { "responder" },
                if *resumed { ", resumed" } else { "" }
            ),
            HostEvent::ConnectFailed { peer, reason } => format!(
                "connect to {peer} failed: {}",
                match reason {
                    ConnectFailure::Refused => "refused",
                    ConnectFailure::NoAnswer => "no answer after three retries",
                    ConnectFailure::HalfOpenTimeout => "no first frame after CONNECT-ACK (state kept for resume)",
                    ConnectFailure::Busy => "already in a session",
                    ConnectFailure::Cancelled => "cancelled",
                }
            ),
            HostEvent::Refused { peer } => format!("refused a CONNECT from {peer}"),
            HostEvent::LinkLost { peer } => {
                format!("link lost with {peer} (state kept 10 minutes: connect {peer} resumes)")
            }
            HostEvent::Disconnected { peer } => format!("disconnected from {peer}"),
            HostEvent::Undelivered { peer, ids } => format!("undelivered to {peer}: {} PDU(s) {ids:?}", ids.len()),
            HostEvent::Acked { peer, ids } => {
                let mut done = Vec::new();
                for (label, wait, _) in &mut self.sends {
                    wait.retain(|i| !ids.contains(i));
                    if wait.is_empty() {
                        done.push(label.clone());
                    }
                }
                self.sends.retain(|(_, w, _)| !w.is_empty());
                if done.is_empty() {
                    return None;
                }
                format!("delivered to {peer}: {}", done.join(", "))
            }
            HostEvent::SessionExpired { peer } => format!("retained session with {peer} expired"),
            HostEvent::SendRejected { id } => format!("send {id} rejected (no session)"),
            HostEvent::BroadcastSent { ids } => format!("broadcast sent: {ids:?}"),
            HostEvent::BroadcastRejected { id, error } => format!("broadcast {id} rejected: {error:?}"),
            HostEvent::LinkTest(r) => format!(
                "link test {}: forward SNR {:.1} dB MCS {} {:.2} kbps; reverse SNR {:.1} dB MCS {} {:.2} kbps",
                r.target,
                r.forward.snr_db,
                r.forward.mcs,
                r.forward.net_kbps,
                r.reverse.snr_db,
                r.reverse.mcs,
                r.reverse.net_kbps
            ),
            HostEvent::Unreachable { target } => format!("probe {target}: no answer"),
            HostEvent::BeaconHeard { from, id } => match id {
                Some(i) => format!("beacon from {from} (ID {}-{})", i.callsign, i.ssid),
                None => format!("beacon from {from}"),
            },
            HostEvent::PduError { peer, error } => format!("PDU error from {peer}: {error:?}"),
            HostEvent::Warning(Warning::ReceiveClipping { peer }) => {
                format!("warning: receive input clipping on frames from {peer}; lower the receive level")
            }
            HostEvent::Warning(Warning::Distortion { peer, index }) => {
                format!("warning: {peer} reports distortion index {index}; lower the transmit drive")
            }
        })
    }
}

fn text_or_len(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) if s.len() <= 200 && !s.chars().any(|c| c.is_control() && c != '\n') => format!("{s:?}"),
        _ => format!("{} bytes", b.len()),
    }
}

/// The status line.
pub fn status(m: &Modem) -> String {
    let st = m.station();
    let s = &m.stats;
    let sess = match st.session_info() {
        Some(i) => format!(
            "session with {} {:?} {:?} id {:04x}: tx MCS {} x{} ({}, ID {}) rx MCS {} (requested) x{} ({}, ID {}), \
idle {}, next seq {}, rx next {}",
            i.peer,
            i.phase,
            i.role,
            i.session_id,
            i.tx_mcs,
            i.tx_width,
            crate::runtime::describe_mask(i.tx_mask.0, i.tx_width),
            i.tx_mask.1,
            i.requested_mcs,
            i.rx_width,
            crate::runtime::describe_mask(i.rx_mask.0, i.rx_width),
            i.rx_mask.1,
            i.idle,
            i.next_seq.0,
            i.rx_next.0
        ),
        None => "no session".into(),
    };
    let rec = if s.rx_recovery_ms.is_empty() {
        String::new()
    } else {
        let mut v = s.rx_recovery_ms.clone();
        v.sort_by(f64::total_cmp);
        format!(
            "; receiver back after own PTT {:.0}/{:.0}/{:.0} ms (min/median/max of {})",
            v[0],
            v[v.len() / 2],
            v[v.len() - 1],
            v.len()
        )
    };
    let d = m.dcd().state();
    let floor = m.dcd().floor_dbfs().map_or("unarmed".to_string(), |f| format!("{f:.1} dBFS"));
    format!(
        "{} {}; DCD raw {} (quieting {} preamble {} AFSK {} COS {}), quieting floor {floor}; rises q/p/a/c {:?}; \
frames rx {} (header failures {}), blocks {}/{}, tx {} frames, keyed {:.1} s{rec}",
        st.address(),
        sess,
        m.dcd_raw(),
        d.quieting,
        d.preamble,
        d.afsk,
        d.cos,
        s.dcd_rises,
        s.frames,
        s.header_failures,
        s.blocks_ok,
        s.blocks,
        s.tx_frames,
        s.keyed.as_secs_f64()
    )
}

/// The frame type name used in logs.
pub fn frame_name(t: FrameType) -> String {
    format!("{t:?}").to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let n0 = parse_call("N0CALL-7").unwrap();
        assert_eq!(n0.to_string(), "N0CALL-7");
        assert_eq!(parse("connect n0call-7").unwrap(), Control::Connect { peer: n0, new_session: false });
        assert_eq!(parse("c N0CALL-7 new").unwrap(), Control::Connect { peer: n0, new_session: true });
        assert_eq!(parse("send hello  there").unwrap(), Control::SendText("hello  there".into()));
        assert_eq!(parse("send @/tmp/x").unwrap(), Control::SendFile("/tmp/x".into()));
        assert_eq!(parse("disconnect").unwrap(), Control::Disconnect { graceful: true });
        assert_eq!(parse("disconnect now").unwrap(), Control::Disconnect { graceful: false });
        assert_eq!(parse("probe N0CALL-7").unwrap(), Control::Probe { target: n0, kind: ProbeKind::LinkTest });
        assert_eq!(parse("probe N0CALL-7 beacon").unwrap(), Control::Probe { target: n0, kind: ProbeKind::Beacon });
        assert_eq!(parse("beacon").unwrap(), Control::Beacon(None));
        assert_eq!(parse("status").unwrap(), Control::Status);
        assert!(parse("frobnicate").is_err());
        assert!(parse("connect").is_err());
        // A long callsign is carried as a hash.
        assert!(parse_call("N0CALL/P1").unwrap().to_string().starts_with('#'));
    }

    #[test]
    fn file_chunks_carry_object_and_offset() {
        let mut h = HostState::new();
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let (cmds, ids, obj) = h.file_commands(&data);
        assert_eq!(cmds.len(), 3);
        assert_eq!(ids.len(), 3);
        let HostCommand::Send { pdu, .. } = &cmds[2] else { panic!() };
        assert_eq!(&pdu.body[0..4], &obj.to_be_bytes());
        assert_eq!(&pdu.body[4..8], &4096u32.to_be_bytes());
        assert_eq!(&pdu.body[8..], &data[4096..]);
    }
}
