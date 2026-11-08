//! PDU framing in the block stream (SPEC §12.1, B.4; ADR 0009): packing and
//! parsing with filler, CRC-32/ISO-HDLC, and broadcast message
//! fragmentation and reassembly with duplicate suppression.
//!
//! Byte-level only, with no dependency on the rest of the link layer except
//! [`Block`] and [`Address`], so it can move to another crate unchanged.

use std::collections::{HashMap, VecDeque};

use crate::frame::{Address, Block};
use crate::params::{
    BLOCK_PAYLOAD, BROADCAST_DEDUP_WINDOW, PDU_CRC_BYTES, PDU_DICT_BYTES, PDU_FIXED_BYTES, PDU_LENGTH_BYTES,
    PDU_MAX_LENGTH, REASSEMBLY_TIMEOUT,
};
use crate::time::Instant;

/// PDU types (§12.1).
pub mod pdu_type {
    /// A raw AX.25 frame from KISS.
    pub const AX25: u8 = 0;
    /// Opaque bytes for the native API.
    pub const MESSAGE: u8 = 1;
    /// A file chunk.
    pub const FILE_CHUNK: u8 = 2;
    /// A signature.
    pub const SIGNATURE: u8 = 3;
    /// Control.
    pub const CONTROL: u8 = 4;
}

/// Encodings (§12.1).
pub mod encoding {
    /// Raw body.
    pub const RAW: u8 = 0;
    /// zstd without dictionary.
    pub const ZSTD: u8 = 1;
    /// zstd with a dictionary (carries a Dictionary ID).
    pub const ZSTD_DICT: u8 = 2;
}

/// CRC-32/ISO-HDLC, the zlib CRC (ADR 0009 `pdu-crc32`).
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// A PDU (§12.1). `body` is the body as sent (possibly compressed); `crc`
/// is the CRC-32 of the decoded body. For a raw body the link computes and
/// checks it; for a compressed one the host supplies it on send and checks
/// it after decompression on receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pdu {
    /// Type ([`pdu_type`]).
    pub pdu_type: u8,
    /// Stream byte: bits 7-6 priority (0 highest), bits 5-0 stream number.
    pub stream: u8,
    /// Encoding ([`encoding`]).
    pub encoding: u8,
    /// Dictionary ID, present exactly when `encoding` is 2.
    pub dictionary_id: Option<[u8; 4]>,
    /// Body as sent.
    pub body: Vec<u8>,
    /// CRC-32 of the decoded body; `None` on send means "compute it" (raw only).
    pub crc: Option<u32>,
}

/// PDU errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PduError {
    /// Length above 65,535.
    TooLong,
    /// A compressed body without its decoded CRC, or a Dictionary ID that
    /// does not match the encoding.
    BadFields,
    /// A Length too short for the fields it must hold.
    Malformed,
    /// A raw body whose CRC-32 does not match.
    CrcMismatch,
    /// A block of the PDU was lost (broadcast frames only).
    Lost,
}

impl Pdu {
    /// A raw MESSAGE PDU on the default stream.
    pub fn message(body: Vec<u8>) -> Pdu {
        Pdu { pdu_type: pdu_type::MESSAGE, stream: 0, encoding: encoding::RAW, dictionary_id: None, body, crc: None }
    }

    /// Priority, 0 highest (§12.1).
    pub fn priority(&self) -> u8 {
        self.stream >> 6
    }

    /// Bytes after the Length field.
    pub fn length_field(&self) -> usize {
        PDU_FIXED_BYTES
            + if self.dictionary_id.is_some() { PDU_DICT_BYTES } else { 0 }
            + self.body.len()
            + PDU_CRC_BYTES
    }

    /// Bytes on the stream, Length field included.
    pub fn wire_len(&self) -> usize {
        PDU_LENGTH_BYTES + self.length_field()
    }

    /// Fixed bytes of a PDU around its body.
    pub fn overhead(encoding: u8) -> usize {
        PDU_LENGTH_BYTES
            + PDU_FIXED_BYTES
            + if encoding == encoding::ZSTD_DICT { PDU_DICT_BYTES } else { 0 }
            + PDU_CRC_BYTES
    }

    /// Encode to stream bytes (big-endian fields, ADR 0009 `pdu-byte-order`).
    pub fn encode(&self) -> Result<Vec<u8>, PduError> {
        let len = self.length_field();
        if len > PDU_MAX_LENGTH {
            return Err(PduError::TooLong);
        }
        if (self.encoding == encoding::ZSTD_DICT) != self.dictionary_id.is_some() {
            return Err(PduError::BadFields);
        }
        let crc = match (self.crc, self.encoding) {
            (Some(c), _) => c,
            (None, encoding::RAW) => crc32(&self.body),
            (None, _) => return Err(PduError::BadFields),
        };
        let mut v = Vec::with_capacity(len + 2);
        v.extend_from_slice(&(len as u16).to_be_bytes());
        v.extend_from_slice(&[self.pdu_type, self.stream, self.encoding]);
        if let Some(d) = self.dictionary_id {
            v.extend_from_slice(&d);
        }
        v.extend_from_slice(&self.body);
        v.extend_from_slice(&crc.to_be_bytes());
        Ok(v)
    }

    /// Decode the bytes after the Length field.
    fn decode_after_length(b: &[u8]) -> Result<Pdu, PduError> {
        if b.len() < PDU_FIXED_BYTES + PDU_CRC_BYTES {
            return Err(PduError::Malformed);
        }
        let (pdu_type, stream, enc) = (b[0], b[1], b[2]);
        let mut i = 3;
        let dictionary_id = if enc == encoding::ZSTD_DICT {
            if b.len() < PDU_FIXED_BYTES + PDU_DICT_BYTES + PDU_CRC_BYTES {
                return Err(PduError::Malformed);
            }
            i += 4;
            Some([b[3], b[4], b[5], b[6]])
        } else {
            None
        };
        let body = b[i..b.len() - 4].to_vec();
        let crc = u32::from_be_bytes([b[b.len() - 4], b[b.len() - 3], b[b.len() - 2], b[b.len() - 1]]);
        if enc == encoding::RAW && crc32(&body) != crc {
            return Err(PduError::CrcMismatch);
        }
        Ok(Pdu { pdu_type, stream, encoding: enc, dictionary_id, body, crc: Some(crc) })
    }
}

// --- Writing the block stream -----------------------------------------------------

/// Writes encoded PDUs into 126-byte blocks: back to back, a PDU's Length
/// never straddling a block boundary (ADR 0009 `pdu-length-within-block`),
/// filler (Length 0) for the rest of a block when nothing is left
/// (`pdu-filler`).
#[derive(Clone, Debug, Default)]
pub struct BlockWriter {
    current: Option<(u64, Vec<u8>, usize)>,
}

impl BlockWriter {
    /// True while a PDU is part-way through.
    pub fn in_progress(&self) -> Option<u64> {
        self.current.as_ref().map(|c| c.0)
    }

    /// Fill one block. `next` yields the next PDU to start (its id and
    /// encoded bytes). Returns the block and the ids of the PDUs whose last
    /// byte is in it, or `None` when there is nothing to send at a block
    /// boundary.
    pub fn next_block(&mut self, mut next: impl FnMut() -> Option<(u64, Vec<u8>)>) -> Option<(Block, Vec<u64>)> {
        if self.current.is_none() {
            let (id, bytes) = next()?;
            self.current = Some((id, bytes, 0));
        }
        let mut block = [0u8; BLOCK_PAYLOAD];
        let mut p = 0;
        let mut done = Vec::new();
        loop {
            let left = BLOCK_PAYLOAD - p;
            if left == 0 {
                break;
            }
            if self.current.is_none() {
                match next() {
                    Some((id, bytes)) => self.current = Some((id, bytes, 0)),
                    None => break, // zeros already there: filler (Length 0) or a single 0x00
                }
            }
            let (id, bytes, pos) = self.current.as_mut().expect("current set");
            if *pos == 0 && left == 1 {
                break; // a lone byte at a PDU boundary is filler
            }
            let n = left.min(bytes.len() - *pos);
            block[p..p + n].copy_from_slice(&bytes[*pos..*pos + n]);
            *pos += n;
            p += n;
            if *pos == bytes.len() {
                done.push(*id);
                self.current = None;
            }
        }
        Some((block, done))
    }
}

/// The session send queue: PDUs by priority, first in first out within a
/// priority, each completed before the next starts (ADR 0009
/// `stream-interleaving-at-pdu-boundaries`).
#[derive(Clone, Debug, Default)]
pub struct PduQueue {
    queues: [VecDeque<(u64, Vec<u8>)>; 4],
    writer: BlockWriter,
}

impl PduQueue {
    /// Queue an encoded PDU under a host id.
    pub fn push(&mut self, id: u64, pdu: &Pdu) -> Result<(), PduError> {
        let bytes = pdu.encode()?;
        self.queues[pdu.priority() as usize].push_back((id, bytes));
        Ok(())
    }

    /// True when nothing is queued or part-way through.
    pub fn is_empty(&self) -> bool {
        self.writer.in_progress().is_none() && self.queues.iter().all(|q| q.is_empty())
    }

    /// The next block of the stream, with the ids of PDUs it completes.
    pub fn next_block(&mut self) -> Option<(Block, Vec<u64>)> {
        let queues = &mut self.queues;
        self.writer.next_block(|| queues.iter_mut().find_map(|q| q.pop_front()))
    }

    /// Every id not yet completely written (part-way first, then queued).
    pub fn pending_ids(&self) -> Vec<u64> {
        self.writer.in_progress().into_iter().chain(self.queues.iter().flatten().map(|(id, _)| *id)).collect()
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        *self = PduQueue::default();
    }
}

/// Pack complete PDUs into the data blocks of one broadcast frame.
pub fn pack_frame(pdus: &[Pdu]) -> Result<Vec<Block>, PduError> {
    let mut encoded: VecDeque<(u64, Vec<u8>)> =
        pdus.iter().enumerate().map(|(i, p)| p.encode().map(|b| (i as u64, b))).collect::<Result<_, _>>()?;
    let mut w = BlockWriter::default();
    let mut out = Vec::new();
    while let Some((b, _)) = w.next_block(|| encoded.pop_front()) {
        out.push(b);
    }
    Ok(out)
}

/// Blocks needed to carry these PDUs in one frame.
pub fn blocks_needed(pdu_lens: impl IntoIterator<Item = usize>) -> usize {
    let mut blocks = 0usize;
    let mut p = BLOCK_PAYLOAD; // position in the current block; full = need a new one
    for len in pdu_lens {
        if BLOCK_PAYLOAD - p <= 1 {
            blocks += 1;
            p = 0;
        }
        let mut left = len;
        while left > 0 {
            if p == BLOCK_PAYLOAD {
                blocks += 1;
                p = 0;
            }
            let n = left.min(BLOCK_PAYLOAD - p);
            left -= n;
            p += n;
        }
    }
    blocks
}

// --- Parsing the block stream ------------------------------------------------------

#[derive(Clone, Debug, Default)]
enum ParseState {
    #[default]
    Boundary,
    Body {
        want: usize,
        bytes: Vec<u8>,
        lost: bool,
    },
    /// A Length field was lost: the rest of this frame cannot be parsed.
    Dead,
}

/// Parses the block stream back into PDUs. Session streams feed every block
/// in order; a broadcast frame feeds its blocks, `None` for a lost one.
#[derive(Clone, Debug, Default)]
pub struct StreamParser {
    state: ParseState,
}

impl StreamParser {
    /// True when no PDU is part-way through.
    pub fn at_boundary(&self) -> bool {
        matches!(self.state, ParseState::Boundary)
    }

    /// Feed one block (or a lost one); returns the PDUs it completes.
    pub fn push(&mut self, block: Option<&Block>) -> Vec<Result<Pdu, PduError>> {
        let mut out = Vec::new();
        let Some(b) = block else {
            match &mut self.state {
                ParseState::Body { want, bytes, lost } => {
                    let n = (*want - bytes.len()).min(BLOCK_PAYLOAD);
                    bytes.resize(bytes.len() + n, 0);
                    *lost = true;
                    if bytes.len() == *want {
                        out.push(Err(PduError::Lost));
                        self.state = ParseState::Boundary;
                        // The rest of this block (after the PDU) is unknown.
                        if n < BLOCK_PAYLOAD {
                            self.state = ParseState::Dead;
                        }
                    }
                }
                _ => self.state = ParseState::Dead,
            }
            return out;
        };
        let mut p = 0;
        while p < BLOCK_PAYLOAD {
            match &mut self.state {
                ParseState::Dead => break,
                ParseState::Boundary => {
                    if BLOCK_PAYLOAD - p == 1 {
                        break; // lone filler byte
                    }
                    let len = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
                    p += 2;
                    if len == 0 {
                        break; // filler: skip to the next block
                    }
                    self.state = ParseState::Body { want: len, bytes: Vec::with_capacity(len), lost: false };
                }
                ParseState::Body { want, bytes, lost } => {
                    let n = (*want - bytes.len()).min(BLOCK_PAYLOAD - p);
                    bytes.extend_from_slice(&b[p..p + n]);
                    p += n;
                    if bytes.len() == *want {
                        out.push(if *lost { Err(PduError::Lost) } else { Pdu::decode_after_length(bytes) });
                        self.state = ParseState::Boundary;
                    }
                }
            }
        }
        out
    }

    /// Discard any part-way PDU.
    pub fn reset(&mut self) {
        self.state = ParseState::Boundary;
    }
}

/// Parse a broadcast frame's data blocks into its PDUs, in order; a PDU
/// that touches a lost block is `Err(Lost)`, and parsing stops where a
/// Length field was lost.
pub fn unpack_frame(blocks: &[Option<Block>]) -> Vec<Result<Pdu, PduError>> {
    let mut p = StreamParser::default();
    let mut out = Vec::new();
    for b in blocks {
        out.extend(p.push(b.as_ref()));
    }
    if !p.at_boundary() && !matches!(p.state, ParseState::Dead) {
        out.push(Err(PduError::Lost));
    }
    out
}

/// The body of a raw PDU, or None for a compressed one: the decoder for
/// fragments when no codec is available.
pub fn raw_body(p: &Pdu) -> Option<Vec<u8>> {
    (p.encoding == encoding::RAW).then(|| p.body.clone())
}

// --- Broadcast fragmentation (ADR 0009 `broadcast-message-fragments`) -----------------

/// Split a raw message into slices of at most `max_body` bytes, each its own
/// complete PDU of the same type and stream.
pub fn fragment(msg: &Pdu, max_body: usize) -> Result<Vec<Pdu>, PduError> {
    if msg.encoding != encoding::RAW || max_body == 0 {
        // A compressed message must be sliced and compressed by the host.
        return Err(PduError::BadFields);
    }
    if msg.body.is_empty() {
        return Ok(vec![Pdu { crc: None, ..msg.clone() }]);
    }
    Ok(msg.body.chunks(max_body).map(|c| Pdu { body: c.to_vec(), crc: None, ..msg.clone() }).collect())
}

/// The role a broadcast frame plays from its flags (ADR 0009
/// `first-fragment-flag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentRole {
    /// Bit 6 set, bit 2 clear.
    Complete,
    /// Both set.
    First,
    /// Bit 6 clear, bit 2 set.
    Middle,
    /// Both clear.
    Last,
}

impl FragmentRole {
    /// From header flags bit 6 (first fragment) and bit 2 (more fragments).
    pub fn from_flags(first: bool, more: bool) -> FragmentRole {
        match (first, more) {
            (true, false) => FragmentRole::Complete,
            (true, true) => FragmentRole::First,
            (false, true) => FragmentRole::Middle,
            (false, false) => FragmentRole::Last,
        }
    }

    /// The (first, more) flags.
    pub fn flags(self) -> (bool, bool) {
        match self {
            FragmentRole::Complete => (true, false),
            FragmentRole::First => (true, true),
            FragmentRole::Middle => (false, true),
            FragmentRole::Last => (false, false),
        }
    }
}

#[derive(Clone, Debug)]
struct Partial {
    next: u16,
    pdu_type: u8,
    stream: u8,
    body: Vec<u8>,
    started: Instant,
}

#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    /// PDUs already delivered from this frame (by index), or all of them.
    delivered: u64,
    complete: bool,
}

/// Receive side of broadcast frames: duplicate suppression by source and
/// frame counter (§12.4), and reassembly of fragmented messages.
#[derive(Clone, Debug, Default)]
pub struct Reassembler {
    seen: HashMap<(Address, u16), Seen>,
    partial: HashMap<Address, Partial>,
}

impl Reassembler {
    /// Process one broadcast frame's PDUs (from [`unpack_frame`]); returns
    /// the PDUs to deliver. Fragments must be raw ([`raw_body`]); see
    /// [`Reassembler::on_frame_with`].
    pub fn on_frame(
        &mut self,
        src: Address,
        counter: u16,
        role: FragmentRole,
        pdus: Vec<Result<Pdu, PduError>>,
        now: Instant,
    ) -> Option<Vec<Pdu>> {
        self.on_frame_with(src, counter, role, pdus, now, &mut raw_body)
    }

    /// [`Reassembler::on_frame`] with a decoder for the slices of a
    /// fragmented message: each slice is compressed independently and the
    /// reassembled message is the concatenation of their decoded bodies (ADR
    /// 0009 `broadcast-message-fragments`), delivered raw. A slice that
    /// `decode` refuses ends the partial message. Complete PDUs pass
    /// through as received.
    pub fn on_frame_with(
        &mut self,
        src: Address,
        counter: u16,
        role: FragmentRole,
        pdus: Vec<Result<Pdu, PduError>>,
        now: Instant,
        decode: &mut dyn FnMut(&Pdu) -> Option<Vec<u8>>,
    ) -> Option<Vec<Pdu>> {
        self.purge(now);
        let all_ok = pdus.iter().all(|p| p.is_ok());
        let entry = self.seen.entry((src, counter)).or_insert(Seen { at: now, delivered: 0, complete: false });
        if entry.complete {
            return None; // a repeat of a frame already passed
        }
        let mut out = Vec::new();
        match role {
            FragmentRole::Complete => {
                for (i, p) in pdus.into_iter().enumerate() {
                    if let Ok(p) = p {
                        let bit = 1u64 << i.min(63);
                        if entry.delivered & bit == 0 {
                            entry.delivered |= bit;
                            out.push(p);
                        }
                    }
                }
                entry.complete = all_ok;
            }
            FragmentRole::First | FragmentRole::Middle | FragmentRole::Last => {
                let slice = match pdus.into_iter().next() {
                    Some(Ok(p)) if all_ok => p,
                    _ => {
                        // A lost slice: the message cannot complete.
                        self.partial.remove(&src);
                        return Some(out);
                    }
                };
                entry.complete = true;
                let Some(body) = decode(&slice) else {
                    self.partial.remove(&src);
                    return Some(out);
                };
                if role == FragmentRole::First {
                    self.partial.insert(
                        src,
                        Partial {
                            next: counter.wrapping_add(1),
                            pdu_type: slice.pdu_type,
                            stream: slice.stream,
                            body,
                            started: now,
                        },
                    );
                    return Some(out);
                }
                let Some(mut part) = self.partial.remove(&src) else { return Some(out) };
                if part.next != counter || part.pdu_type != slice.pdu_type || part.stream != slice.stream {
                    return Some(out); // gap or mismatch: discard
                }
                part.body.extend_from_slice(&body);
                part.next = counter.wrapping_add(1);
                if role == FragmentRole::Last {
                    out.push(Pdu {
                        pdu_type: part.pdu_type,
                        stream: part.stream,
                        encoding: encoding::RAW,
                        dictionary_id: None,
                        crc: Some(crc32(&part.body)),
                        body: part.body,
                    });
                } else {
                    self.partial.insert(src, part);
                }
            }
        }
        Some(out)
    }

    /// Note a frame counter from a source whose frame carried no PDU (a
    /// BEACON without one): it still breaks any partial message.
    pub fn on_other(&mut self, src: Address, counter: u16) {
        if self.partial.get(&src).is_some_and(|p| p.next == counter) {
            self.partial.remove(&src);
        }
    }

    /// The earliest time something here expires.
    pub fn next_deadline(&self) -> Option<Instant> {
        let a = self.partial.values().map(|p| p.started + REASSEMBLY_TIMEOUT).min();
        let b = self.seen.values().map(|s| s.at + BROADCAST_DEDUP_WINDOW).min();
        a.into_iter().chain(b).min()
    }

    /// Drop expired dedup entries and partial messages.
    pub fn purge(&mut self, now: Instant) {
        self.seen.retain(|_, s| now < s.at + BROADCAST_DEDUP_WINDOW);
        self.partial.retain(|_, p| now < p.started + REASSEMBLY_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The PDUs and blocks of `vectors/link/pdu_blocks.json` (cairn-phy PR
    /// #21, branch `phy/link-fields`), so this module packs bit for bit as
    /// `cairn_phy::pdu` does.
    #[test]
    fn matches_pdu_blocks_vector() {
        let g0 = vec![
            Pdu {
                pdu_type: 1,
                stream: 0,
                encoding: 0,
                dictionary_id: None,
                body: hex(
                    "7b6f97d5f3fa87718611ca523e07f2f8b76fbcf16303a7c12973df5c88e1fa0e2e0e8c2a8a779edac373d5a0dd0ff486e656f8ca23f5439771ddb0b2115883fbf69e68ac5c6e4dbe65ad117c2274afa807d825f30837acadcc022fa234b097e100766e4a58c147b802b6ab746cd873ca56848a79",
                ),
                crc: None,
            },
            Pdu {
                pdu_type: 2,
                stream: 194,
                encoding: 0,
                dictionary_id: None,
                body: hex(
                    "0000beef00001000e6789de00c4598758cd2865b7de2e4aaf65d46eccbc042cb9938569de4e3df2927a268d3715ae6098d321adc0a7e23db8a64134ef15bdde76202049cb475388d8c7e303523409fbd7b2755164aaf8996d37c88b0451e023469a4d10e02e3756f1547d835afda7daa0941eb2f82fdcc85c2afb5c31646c63f00cf63f5f2a56cbdb9c4c3ca5ab97a2acec099784c561b239da0653f5171601939885a771e384da65ef94af816556c63eab24e84597242bce7c62a4165f3d84eaae8561f6e23f83a422b912f412d3f62fa05178e57cbe5b7525381dc73b9493bdb1894251b052cd7d9afbf1a435b0c699a8f563e0c798f03fb40ec0471f79e3aae06b74d1e74a3b585946573784ca275f21ca85096b8316f67ad823c1316f5123a7738515adbcd983786f9a1",
                ),
                crc: None,
            },
            Pdu {
                pdu_type: 0,
                stream: 0,
                encoding: 2,
                dictionary_id: Some([0xac, 0xe0, 0x93, 0x75]),
                body: hex("3df49c9e22c8b45beb8546c3f939364e11b6c52998b502695ca2c9897b179235bbee16b96d7d0faa"),
                crc: Some(crc32(&hex(
                    "44696374696f6e6172792d636f6d707265737365642074657874207374616e647320696e20686572653b20746865204352432d333220636f766572732074686973206465636f64656420626f64792e",
                ))),
            },
        ];
        let g1 = vec![
            Pdu { pdu_type: 4, stream: 69, encoding: 0, dictionary_id: None, body: hex("00010203"), crc: None },
            Pdu::message(vec![]),
        ];
        assert_eq!(g0[2].crc, Some(0xb223_7c56));
        let blocks: Vec<Block> = pack_frame(&g0).unwrap().into_iter().chain(pack_frame(&g1).unwrap()).collect();
        let want = [
            "007b0100007b6f97d5f3fa87718611ca523e07f2f8b76fbcf16303a7c12973df5c88e1fa0e2e0e8c2a8a779edac373d5a0dd0ff486e656f8ca23f5439771ddb0b2115883fbf69e68ac5c6e4dbe65ad117c2274afa807d825f30837acadcc022fa234b097e100766e4a58c147b802b6ab746cd873ca56848a79bdcb99a400",
            "013302c2000000beef00001000e6789de00c4598758cd2865b7de2e4aaf65d46eccbc042cb9938569de4e3df2927a268d3715ae6098d321adc0a7e23db8a64134ef15bdde76202049cb475388d8c7e303523409fbd7b2755164aaf8996d37c88b0451e023469a4d10e02e3756f1547d835afda7daa0941eb2f82fdcc85c2",
            "afb5c31646c63f00cf63f5f2a56cbdb9c4c3ca5ab97a2acec099784c561b239da0653f5171601939885a771e384da65ef94af816556c63eab24e84597242bce7c62a4165f3d84eaae8561f6e23f83a422b912f412d3f62fa05178e57cbe5b7525381dc73b9493bdb1894251b052cd7d9afbf1a435b0c699a8f563e0c798f",
            "03fb40ec0471f79e3aae06b74d1e74a3b585946573784ca275f21ca85096b8316f67ad823c1316f5123a7738515adbcd983786f9a12984852f0033000002ace093753df49c9e22c8b45beb8546c3f939364e11b6c52998b502695ca2c9897b179235bbee16b96d7d0faab2237c5600000000000000000000000000000000",
            "000b044500000102038bb986130007010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        ];
        assert_eq!(blocks.len(), want.len());
        for (b, w) in blocks.iter().zip(want) {
            assert_eq!(b.to_vec(), hex(w));
        }
        // And back.
        let back = unpack_frame(&blocks[..4].iter().copied().map(Some).collect::<Vec<_>>());
        assert_eq!(back.len(), 3);
        assert_eq!(back[2].as_ref().unwrap().dictionary_id, Some([0xac, 0xe0, 0x93, 0x75]));
        assert_eq!(back[1].as_ref().unwrap().body, g0[1].body);
    }

    /// `vectors/link/block_map_*.json` (cairn-phy PR #21).
    #[test]
    fn matches_block_map_vectors() {
        use crate::frame::BlockMap;
        use crate::seq::Seq;
        let m = BlockMap { base: Seq(300), bitmap: 0xe5 };
        assert_eq!(m.seqs(), [300, 302, 305, 306, 307].map(Seq).to_vec());
        let m = BlockMap { base: Seq(65533), bitmap: (1u128 << 127) | 0xd };
        assert_eq!(m.seqs(), [65533, 65535, 0, 124].map(Seq).to_vec());
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), crate::params::CRC32_CHECK);
    }

    #[test]
    fn encode_layout() {
        let p = Pdu::message(b"hi".to_vec());
        let e = p.encode().unwrap();
        assert_eq!(&e[..2], &[0, 9]);
        assert_eq!(&e[2..5], &[1, 0, 0]);
        assert_eq!(&e[5..7], b"hi");
        assert_eq!(&e[7..], &crc32(b"hi").to_be_bytes());
    }

    #[test]
    fn lone_byte_is_filler() {
        // A PDU of exactly 125 bytes on the stream leaves one byte.
        let first = Pdu::message(vec![7u8; 125 - 9]);
        assert_eq!(first.wire_len(), 125);
        let second = Pdu::message(vec![9u8; 3]);
        let blocks = pack_frame(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0][125], 0);
        assert_eq!(&blocks[1][..2], &[0, 10]);
        let back: Vec<_> = unpack_frame(&blocks.iter().copied().map(Some).collect::<Vec<_>>());
        let mut f2 = first;
        f2.crc = Some(crc32(&f2.body));
        let mut s2 = second;
        s2.crc = Some(crc32(&s2.body));
        assert_eq!(back, vec![Ok(f2), Ok(s2)]);
        assert_eq!(blocks_needed([125, 10]), 2);
    }

    #[test]
    fn lost_block_loses_only_touching_pdus() {
        let a = Pdu::message(vec![1u8; 300]);
        let b = Pdu::message(vec![2u8; 20]);
        let mut blocks: Vec<_> = pack_frame(&[a, b.clone()]).unwrap().into_iter().map(Some).collect();
        assert_eq!(blocks.len(), 3);
        blocks[1] = None;
        let out = unpack_frame(&blocks);
        assert_eq!(out[0], Err(PduError::Lost));
        assert_eq!(out[1].as_ref().unwrap().body, b.body);
        // A lost Length field ends parsing of the frame.
        blocks[0] = None;
        assert!(unpack_frame(&blocks).is_empty());
    }

    #[test]
    fn reassembly_and_dedup() {
        let src = Address::callsign("N0AAA", 0).unwrap();
        let msg = Pdu::message((0..=255u8).cycle().take(1000).collect());
        let parts = fragment(&msg, 300).unwrap();
        assert_eq!(parts.len(), 4);
        let mut r = Reassembler::default();
        let t = Instant::ZERO;
        let roles = [FragmentRole::First, FragmentRole::Middle, FragmentRole::Middle, FragmentRole::Last];
        let mut got = Vec::new();
        for (i, (p, role)) in parts.iter().zip(roles).enumerate() {
            let pdus = vec![Ok(p.clone())];
            got.extend(r.on_frame(src, 65534u16.wrapping_add(i as u16), role, pdus.clone(), t).unwrap());
            // An identical repeat is suppressed.
            assert!(r.on_frame(src, 65534u16.wrapping_add(i as u16), role, pdus, t).is_none());
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, msg.body);
        // A gap discards.
        let mut r = Reassembler::default();
        r.on_frame(src, 10, FragmentRole::First, vec![Ok(parts[0].clone())], t);
        assert!(r.on_frame(src, 12, FragmentRole::Last, vec![Ok(parts[1].clone())], t).unwrap().is_empty());
        // A continuation without a first fragment is discarded.
        assert!(r.on_frame(src, 13, FragmentRole::Last, vec![Ok(parts[1].clone())], t).unwrap().is_empty());
    }

    proptest! {
        #[test]
        fn stream_round_trip(bodies in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..400), 1..12),
                             prios in proptest::collection::vec(0u8..4, 12)) {
            let mut q = PduQueue::default();
            for (i, b) in bodies.iter().enumerate() {
                let mut p = Pdu::message(b.clone());
                p.stream = prios[i] << 6;
                q.push(i as u64, &p).unwrap();
            }
            let mut parser = StreamParser::default();
            let mut out = Vec::new();
            let mut completed = Vec::new();
            while let Some((blk, done)) = q.next_block() {
                completed.extend(done);
                for p in parser.push(Some(&blk)) {
                    out.push(p.unwrap());
                }
            }
            prop_assert!(q.is_empty());
            prop_assert_eq!(out.len(), bodies.len());
            // Everything was queued before the first block: highest priority
            // first, first in first out within a priority.
            let mut order: Vec<usize> = (0..bodies.len()).collect();
            order.sort_by_key(|&i| (prios[i], i));
            let got: Vec<Vec<u8>> = out.iter().map(|p| p.body.clone()).collect();
            let want: Vec<Vec<u8>> = order.iter().map(|&i| bodies[i].clone()).collect();
            prop_assert_eq!(got, want);
            let mut c = completed.clone();
            c.sort();
            prop_assert_eq!(c, (0..bodies.len() as u64).collect::<Vec<_>>());
        }
    }
}
