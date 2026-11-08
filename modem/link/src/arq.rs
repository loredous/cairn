//! Selective-repeat ARQ (SPEC §9.3; ADR 0008): the sender's window,
//! retransmission order and block map, and the receiver's cumulative ACK,
//! reassembly buffer and in-order delivery.
//!
//! Both sides count blocks with 64-bit absolute indices that never wrap;
//! only the low 16 bits go on air ([`Seq`]).

use std::collections::{BTreeMap, VecDeque};

use crate::frame::{Block, BlockMap};
use crate::params::SEND_WINDOW;
use crate::pdu::{Pdu, PduError, PduQueue, StreamParser};
use crate::seq::Seq;

#[derive(Clone, Debug)]
enum Slot {
    /// Sent, not acknowledged.
    Unacked(Block),
    /// Held by the receiver (bitmap bit set), not yet below its base.
    Held,
}

/// The blocks chosen for one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Header Sequence: the first block's sequence, or the next unsent
    /// sequence when there are no blocks.
    pub seq: Seq,
    /// The blocks, in ascending sequence order.
    pub blocks: Vec<Block>,
    /// Their absolute indices.
    pub abs: Vec<u64>,
    /// The block map, when the blocks are not consecutive.
    pub map: Option<BlockMap>,
}

/// The sending half of one direction of a session.
#[derive(Clone, Debug)]
pub struct Sender {
    base: u64,
    next: u64,
    slots: VecDeque<Slot>,
    queue: PduQueue,
    pdu_ends: VecDeque<(u64, u64)>,
}

impl Sender {
    /// A sender whose first block is `initial` (0 for a new session).
    pub fn new(initial: u64) -> Self {
        Sender {
            base: initial,
            next: initial,
            slots: VecDeque::new(),
            queue: PduQueue::default(),
            pdu_ends: VecDeque::new(),
        }
    }

    /// Queue a PDU under a host id.
    pub fn push(&mut self, id: u64, pdu: &Pdu) -> Result<(), PduError> {
        self.queue.push(id, pdu)
    }

    /// The next unsent sequence.
    pub fn next_seq(&self) -> Seq {
        Seq::from_abs(self.next)
    }

    /// The next unsent absolute index.
    pub fn next_abs(&self) -> u64 {
        self.next
    }

    /// The cumulative acknowledgment point (absolute).
    pub fn acked_abs(&self) -> u64 {
        self.base
    }

    /// Lowest block sent and not acknowledged, or the next unsent one
    /// (ADR 0008 `send-window`).
    pub fn lowest_unacked(&self) -> u64 {
        self.slots.iter().position(|s| matches!(s, Slot::Unacked(_))).map_or(self.next, |i| self.base + i as u64)
    }

    fn retx(&self) -> Vec<u64> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Slot::Unacked(_)))
            .map(|(i, _)| self.base + i as u64)
            .collect()
    }

    fn window_room(&self) -> u64 {
        (self.lowest_unacked() + SEND_WINDOW as u64).saturating_sub(self.next)
    }

    /// True when a frame would carry data blocks (retransmissions, or new
    /// blocks the window allows).
    pub fn has_data_to_send(&self) -> bool {
        self.slots.iter().any(|s| matches!(s, Slot::Unacked(_))) || (!self.queue.is_empty() && self.window_room() > 0)
    }

    /// True when everything queued has been sent and cumulatively acknowledged.
    pub fn is_drained(&self) -> bool {
        self.queue.is_empty() && self.base == self.next
    }

    /// Apply a cumulative ACK (ADR 0008 `cumulative-ack`). Returns the ids
    /// of PDUs now wholly below the receiver's base, which the receiver has
    /// delivered. A base outside [acked, next unsent] is ignored as bogus;
    /// a stale one can only understate.
    pub fn on_ack(&mut self, base: Seq, bitmap: u128) -> Vec<u64> {
        let abs = base.to_abs(self.base);
        if abs > self.next {
            return Vec::new();
        }
        while self.base < abs {
            self.slots.pop_front();
            self.base += 1;
        }
        for i in 1..128u64 {
            if bitmap >> i & 1 == 1 {
                let a = abs + i;
                if a >= self.base && a < self.next {
                    self.slots[(a - self.base) as usize] = Slot::Held;
                }
            }
        }
        let mut done = Vec::new();
        while let Some(&(id, last)) = self.pdu_ends.front() {
            if last < self.base {
                done.push(id);
                self.pdu_ends.pop_front();
            } else {
                break;
            }
        }
        done
    }

    fn take_new(&mut self, n: usize) -> Vec<u64> {
        let mut out = Vec::new();
        let room = self.window_room() as usize;
        for _ in 0..n.min(room) {
            let Some((block, done)) = self.queue.next_block() else { break };
            let abs = self.next;
            for id in done {
                self.pdu_ends.push_back((id, abs));
            }
            self.slots.push_back(Slot::Unacked(block));
            self.next += 1;
            out.push(abs);
        }
        out
    }

    fn block(&self, abs: u64) -> Block {
        match &self.slots[(abs - self.base) as usize] {
            Slot::Unacked(b) => *b,
            Slot::Held => unreachable!("held blocks are never resent"),
        }
    }

    /// Choose the blocks of the next frame: every unacknowledged block,
    /// lowest first, then new blocks, within the window (§9.3). `cap_plain`
    /// is the capacity without a block map, `cap_mapped` with one (one
    /// control block fewer of room).
    pub fn select(&mut self, cap_plain: usize, cap_mapped: usize) -> Selection {
        let retx = self.retx();
        let consecutive_tail =
            retx.last().is_none_or(|&l| l + 1 == self.next) && retx.windows(2).all(|w| w[1] == w[0] + 1);
        let abs: Vec<u64> = if consecutive_tail {
            // Retransmissions (if any) run straight into the new blocks.
            let mut v: Vec<u64> = retx.iter().copied().take(cap_plain).collect();
            if v.len() < cap_plain {
                let n = cap_plain - v.len();
                v.extend(self.take_new(n));
            }
            v
        } else {
            let run = retx.iter().zip(retx.iter().skip(1)).take_while(|(a, b)| **b == **a + 1).count() + 1;
            let plain = retx.len().min(cap_plain).min(if run == retx.len() { usize::MAX } else { run });
            let room = self.window_room() as usize;
            let avail_new = if self.queue.is_empty() { 0 } else { room };
            let mapped = (retx.len() + avail_new).min(cap_mapped);
            if plain >= mapped {
                retx[..plain].to_vec()
            } else {
                let mut v: Vec<u64> = retx.iter().copied().take(cap_mapped).collect();
                if v.len() < cap_mapped {
                    let n = cap_mapped - v.len();
                    v.extend(self.take_new(n));
                }
                v
            }
        };
        let blocks = abs.iter().map(|&a| self.block(a)).collect();
        let seq = Seq::from_abs(abs.first().copied().unwrap_or(self.next));
        let consecutive = abs.windows(2).all(|w| w[1] == w[0] + 1);
        let map = if consecutive {
            None
        } else {
            let first = abs[0];
            let bitmap = abs.iter().fold(0u128, |m, &a| m | 1u128 << (a - first));
            Some(BlockMap { base: seq, bitmap })
        };
        Selection { seq, blocks, abs, map }
    }

    /// Host ids of PDUs not wholly acknowledged: the ones written into
    /// blocks the receiver has not delivered, then the one part-way, then
    /// those still queued (ADR 0010 `disconnect-handshake`, §12.4).
    pub fn undelivered(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.pdu_ends.iter().map(|&(id, _)| id).collect();
        for id in self.queue.pending_ids() {
            if !v.contains(&id) {
                v.push(id);
            }
        }
        v
    }

    /// Start over at sequence 0 (a resume answered as a new session, ADR 0008
    /// `resume-flag-echo`). Returns the undelivered ids of the old state.
    pub fn reset(&mut self, initial: u64) -> Vec<u64> {
        let u = self.undelivered();
        *self = Sender::new(initial);
        u
    }
}

/// The receiving half of one direction of a session.
#[derive(Clone, Debug)]
pub struct Receiver {
    next: u64,
    held: BTreeMap<u64, Block>,
    parser: StreamParser,
}

/// What one frame's data blocks did at the receiver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RxOutcome {
    /// PDUs completed, in order.
    pub pdus: Vec<Result<Pdu, PduError>>,
    /// Data blocks that failed (CRC, mask, or an unusable block map).
    pub failed: usize,
    /// Blocks received again that were already held or delivered.
    pub duplicates: usize,
}

impl Receiver {
    /// A receiver expecting `initial` first.
    pub fn new(initial: u64) -> Self {
        Receiver { next: initial, held: BTreeMap::new(), parser: StreamParser::default() }
    }

    /// The next in-order sequence.
    pub fn next_seq(&self) -> Seq {
        Seq::from_abs(self.next)
    }

    /// The next in-order absolute index.
    pub fn next_abs(&self) -> u64 {
        self.next
    }

    /// Blocks held behind a gap.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The cumulative ACK fields: base and bitmap (ADR 0008 `cumulative-ack`).
    pub fn ack_fields(&self) -> (Seq, u128) {
        let mut bitmap = 0u128;
        for &a in self.held.keys() {
            let i = a - self.next;
            if i < 128 {
                bitmap |= 1u128 << i;
            }
        }
        (self.next_seq(), bitmap)
    }

    /// Take a frame's data blocks. `seqs` is `None` when the block map was
    /// unusable: every block then counts as failed.
    pub fn on_blocks(&mut self, seqs: Option<&[Seq]>, blocks: &[Option<Block>]) -> RxOutcome {
        let mut out = RxOutcome::default();
        let Some(seqs) = seqs else {
            out.failed = blocks.len();
            return out;
        };
        for (s, b) in seqs.iter().zip(blocks) {
            let Some(b) = b else {
                out.failed += 1;
                continue;
            };
            let abs = s.to_abs(self.next);
            if abs < self.next || self.held.contains_key(&abs) {
                out.duplicates += 1;
            } else if abs < self.next + SEND_WINDOW as u64 {
                self.held.insert(abs, *b);
            }
        }
        while let Some(b) = self.held.remove(&self.next) {
            out.pdus.extend(self.parser.push(Some(&b)));
            self.next += 1;
        }
        out
    }

    /// Discard blocks held behind a gap and any part-way PDU (at close).
    pub fn discard_held(&mut self) {
        self.held.clear();
        self.parser.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdu(n: usize, fill: u8) -> Pdu {
        Pdu::message(vec![fill; n])
    }

    #[test]
    fn retransmissions_first_with_block_map() {
        let mut s = Sender::new(0);
        s.push(1, &pdu(126 * 20, 1)).unwrap();
        let sel = s.select(5, 4);
        assert_eq!(sel.abs, vec![0, 1, 2, 3, 4]);
        assert!(sel.map.is_none());
        // Receiver held 0, 2, 4 => base 1, bit 1 (block 2) and bit 3 (block 4).
        s.on_ack(Seq(1), 0b1010);
        let sel = s.select(5, 4);
        assert_eq!(sel.abs, vec![1, 3, 5, 6]);
        let m = sel.map.unwrap();
        assert_eq!(m.base, Seq(1));
        assert_eq!(m.bitmap, 0b110101);
        assert_eq!(sel.seq, Seq(1));
    }

    #[test]
    fn consecutive_retransmission_tail_needs_no_map() {
        let mut s = Sender::new(0);
        s.push(1, &pdu(126 * 20, 1)).unwrap();
        s.select(4, 3);
        s.on_ack(Seq(2), 0);
        let sel = s.select(4, 3);
        assert_eq!(sel.abs, vec![2, 3, 4, 5]);
        assert!(sel.map.is_none());
    }

    #[test]
    fn window_limits_new_blocks() {
        let mut s = Sender::new(65_500);
        s.push(1, &pdu(126 * 300, 1)).unwrap();
        assert_eq!(s.select(100, 99).abs.len(), 100);
        // Nothing acknowledged: everything is resent, and only 28 new blocks fit.
        let sel = s.select(128, 127);
        assert_eq!(sel.abs.len(), 128);
        assert_eq!(s.next_abs() - 65_500, 128);
        assert_eq!(s.lowest_unacked(), 65_500);
        // ACK everything but the first block: still blocked by the window.
        s.on_ack(Seq::from_abs(65_500), !1u128);
        let sel = s.select(128, 127);
        assert_eq!(sel.abs, vec![65_500]);
        assert!(sel.map.is_none());
    }

    #[test]
    fn receiver_in_order_and_ack() {
        let mut tx = Sender::new(65_530);
        tx.push(7, &pdu(126 * 9, 3)).unwrap();
        let sel = tx.select(12, 11);
        let mut rx = Receiver::new(65_530);
        let seqs: Vec<Seq> = sel.abs.iter().map(|&a| Seq::from_abs(a)).collect();
        let mut blocks: Vec<Option<Block>> = sel.blocks.iter().copied().map(Some).collect();
        blocks[1] = None;
        let o = rx.on_blocks(Some(&seqs), &blocks);
        assert_eq!(o.failed, 1);
        assert!(o.pdus.is_empty());
        let (base, bitmap) = rx.ack_fields();
        assert_eq!(base, Seq(65_531));
        assert_eq!(bitmap & 1, 0);
        assert_eq!(bitmap.count_ones() as usize, sel.blocks.len() - 2);
        assert!(tx.on_ack(base, bitmap).is_empty());
        let sel2 = tx.select(12, 11);
        assert_eq!(sel2.abs, vec![65_531]);
        let o = rx.on_blocks(Some(&[Seq(65_531)]), &[Some(sel2.blocks[0])]);
        assert_eq!(o.pdus.len(), 1);
        let (base, bitmap) = rx.ack_fields();
        assert_eq!(tx.on_ack(base, bitmap), vec![7]);
        assert!(tx.is_drained());
    }
}
