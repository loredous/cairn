//! The broadcast send path: UI and BEACON frames with the frame counter,
//! several PDUs per UI frame, fragmentation of large messages, identical
//! repeats (SPEC §9.1, §12.1, §13.1; ADR 0009 `broadcast-message-fragments`,
//! `first-fragment-flag`; ADR 0010 `broadcast-frame-counter`,
//! `kiss-ui-frames`, `broadcast-during-session`).

use std::collections::VecDeque;

use crate::frame::{Address, ControlBlock, Flags, FrameType, LinkFrame, TxParams};
use crate::params::{ALL_TONES, BACKOFF_MAX, BACKOFF_MIN, BASE_WIDTH, BLOCK_PAYLOAD};
use crate::pdu::{FragmentRole, Pdu, PduError, blocks_needed, fragment, pack_frame};
use crate::rng::Rng;
use crate::seq::Seq;
use crate::station::Config;
use crate::time::Instant;

/// A queued broadcast message.
#[derive(Clone, Debug)]
pub enum Message {
    /// A UI message (one PDU; AX.25 frames from KISS are AX25 PDUs).
    Ui(Pdu),
    /// A BEACON, with an optional status PDU.
    Beacon(Option<Pdu>),
}

#[derive(Clone, Debug)]
struct Prepared {
    ftype: FrameType,
    role: FragmentRole,
    pdus: Vec<Pdu>,
    /// Host ids completed when this frame (and its repeats) is sent.
    ids: Vec<u64>,
    repeats_left: u8,
    counter: Option<u16>,
    not_before: Instant,
}

/// The broadcast send queue.
#[derive(Clone, Debug, Default)]
pub struct BroadcastTx {
    msgs: VecDeque<(u64, Message, u8)>,
    frames: VecDeque<Prepared>,
}

impl BroadcastTx {
    /// Queue a message with `repeats` identical repeats of each frame.
    pub fn push(&mut self, id: u64, msg: Message, repeats: u8) {
        self.msgs.push_back((id, msg, repeats));
    }

    /// Nothing queued.
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty() && self.frames.is_empty()
    }

    /// A fragmented message is part-way: no other counter-numbered frame
    /// may go between its frames.
    pub fn mid_message(&self) -> bool {
        self.frames
            .front()
            .is_some_and(|f| f.counter.is_some() || matches!(f.role, FragmentRole::Middle | FragmentRole::Last))
    }

    /// Prepare frames from queued messages. `cap(controls)` is the data
    /// blocks a broadcast frame with that many control blocks can carry.
    /// Returns the ids of messages that cannot be sent.
    pub fn prepare(&mut self, cap: impl Fn(usize) -> usize, now: Instant) -> Vec<(u64, PduError)> {
        let mut rejected = Vec::new();
        while self.frames.is_empty() {
            let Some((id, msg, repeats)) = self.msgs.pop_front() else { break };
            let (ftype, pdu, ncontrol) = match msg {
                Message::Ui(p) => (FrameType::Ui, Some(p), 0),
                Message::Beacon(p) => (FrameType::Beacon, p, 1),
            };
            let cap = cap(ncontrol);
            let Some(pdu) = pdu else {
                self.frames.push_back(Prepared {
                    ftype,
                    role: FragmentRole::Complete,
                    pdus: vec![],
                    ids: vec![id],
                    repeats_left: repeats,
                    counter: None,
                    not_before: now,
                });
                continue;
            };
            if pdu.encode().is_err() {
                rejected.push((id, PduError::BadFields));
                continue;
            }
            if blocks_needed([pdu.wire_len()]) <= cap {
                let mut pdus = vec![pdu];
                let mut ids = vec![id];
                // Several complete UI PDUs may share a frame (ADR 0010 `kiss-ui-frames`).
                if ftype == FrameType::Ui {
                    while let Some((nid, Message::Ui(np), nrep)) = self.msgs.front() {
                        let lens: Vec<usize> = pdus.iter().chain(std::iter::once(np)).map(|p| p.wire_len()).collect();
                        if *nrep != repeats || np.encode().is_err() || blocks_needed(lens) > cap {
                            break;
                        }
                        pdus.push(np.clone());
                        ids.push(*nid);
                        self.msgs.pop_front();
                    }
                }
                self.frames.push_back(Prepared {
                    ftype,
                    role: FragmentRole::Complete,
                    pdus,
                    ids,
                    repeats_left: repeats,
                    counter: None,
                    not_before: now,
                });
                continue;
            }
            let max_body = (cap * BLOCK_PAYLOAD).saturating_sub(Pdu::overhead(pdu.encoding));
            match fragment(&pdu, max_body) {
                Ok(slices) => {
                    let n = slices.len();
                    for (i, s) in slices.into_iter().enumerate() {
                        let role = match (i == 0, i + 1 == n) {
                            (true, true) => FragmentRole::Complete,
                            (true, false) => FragmentRole::First,
                            (false, false) => FragmentRole::Middle,
                            (false, true) => FragmentRole::Last,
                        };
                        self.frames.push_back(Prepared {
                            ftype,
                            role,
                            pdus: vec![s],
                            ids: if i + 1 == n { vec![id] } else { vec![] },
                            repeats_left: repeats,
                            counter: None,
                            not_before: now,
                        });
                    }
                }
                Err(e) => rejected.push((id, e)),
            }
        }
        rejected
    }

    /// When the next frame may start its carrier sense.
    pub fn next_ready(&self) -> Option<Instant> {
        self.frames.front().map(|f| f.not_before)
    }

    /// Build the next frame; a new frame takes the next counter value.
    pub fn build(&mut self, cfg: &Config, counter: &mut u16, last_id: &mut Option<Instant>, now: Instant) -> LinkFrame {
        let f = self.frames.front_mut().expect("a prepared frame");
        let repeat = f.counter.is_some();
        let c = match f.counter {
            Some(c) => c,
            None => {
                *counter = counter.wrapping_add(1);
                f.counter = Some(*counter);
                *counter
            }
        };
        let (first, more) = f.role.flags();
        let controls = if f.ftype == FrameType::Beacon {
            *last_id = Some(now);
            vec![ControlBlock::Id(cfg.id.clone())]
        } else {
            vec![]
        };
        let data = pack_frame(&f.pdus).expect("checked when prepared");
        let width = cfg.broadcast_width.max(BASE_WIDTH);
        LinkFrame {
            ftype: f.ftype,
            src: cfg.address,
            dst: Address::BROADCAST,
            flags: Flags {
                more_fragments: more,
                first_fragment: first,
                repeat,
                probe: width > BASE_WIDTH,
                session: false,
            },
            seq: Seq(c),
            mcs: if data.is_empty() { 0 } else { cfg.broadcast_mcs },
            mask_id: 0,
            controls,
            data,
            tx: TxParams { width, mask: ALL_TONES, long_prefix: cfg.repeater },
        }
    }

    /// PTT released after the front frame: schedule its repeat or move on.
    /// Returns the ids of messages now completely sent.
    pub fn on_tx_done(&mut self, now: Instant, rng: &mut Rng) -> Vec<u64> {
        let Some(f) = self.frames.front_mut() else { return vec![] };
        if f.repeats_left > 0 {
            f.repeats_left -= 1;
            f.not_before = now + rng.duration(BACKOFF_MIN, BACKOFF_MAX);
            return vec![];
        }
        let f = self.frames.pop_front().expect("front");
        if let Some(next) = self.frames.front_mut() {
            next.not_before = now;
        }
        f.ids
    }
}
