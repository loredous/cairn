//! PDUs in the block stream (SPEC 12.1, B.4, ADR 0009), mirroring
//! `sim/cairn_sim/pdu.py` and checked against `vectors/link/pdu_*.json`.
//!
//! A PDU is `Length (2) | Type (1) | Stream (1) | Encoding (1) | [Dictionary
//! ID (4), encoding 2 only] | Body | CRC-32 (4)`; every multi-byte field is
//! big-endian (`pdu-byte-order`), the Length counts every byte after itself,
//! the CRC-32 included, and the CRC-32 is CRC-32/ISO-HDLC (the zlib CRC) over
//! the decoded body (`pdu-crc32`).
//!
//! PDUs go back to back into 126-byte data-block payloads and may span
//! blocks. Where a PDU would start, a Length of 0x0000 marks the rest of the
//! block as filler, sent as zeros (`pdu-filler`); one byte left at a PDU
//! boundary is a 0x00 filler byte, so a Length never straddles a block
//! (`pdu-length-within-block`). A payload's bytes become the block's first
//! 1,008 information bits, each byte most significant bit first
//! (`data-block-byte-order`; [`block_bits`], [`bits_to_payload`]).
//!
//! Compression is outside this module: [`Pdu::body`] is the body as sent and
//! [`Pdu::crc`] the CRC-32 of the decoded body, which [`Pdu::raw`] computes
//! for an uncompressed PDU.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

/// Payload bytes of a data block (B.4).
pub const BLOCK_BYTES: usize = 126;
/// A data block's payload.
pub type Block = [u8; BLOCK_BYTES];
/// Encoding 2 (zstd with dictionary) carries the Dictionary ID.
pub const ENCODING_DICT: u8 = 2;

/// SPEC 12.1 PDU types (7-255 reserved).
pub mod pdu_type {
    #![allow(missing_docs)]
    pub const AX25: u8 = 0;
    pub const MESSAGE: u8 = 1;
    pub const FILE_CHUNK: u8 = 2;
    pub const SIGNATURE: u8 = 3;
    pub const CONTROL: u8 = 4;
    pub const RATELESS: u8 = 5;
    pub const AX25_CONNECTED: u8 = 6;
}

/// Why bytes are not a PDU, or a PDU does not fit its fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PduError {
    /// The Length would exceed 65,535.
    TooLong(usize),
    /// A Dictionary ID without encoding 2, or encoding 2 without one.
    DictionaryId,
    /// Fewer bytes than the Length says, or more.
    Truncated,
    /// A Length shorter than the fixed fields.
    Short(usize),
    /// Priority above 3 or stream number above 63.
    Stream,
    /// A slice of zero bytes.
    Slice,
}

impl fmt::Display for PduError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PduError::TooLong(n) => write!(f, "PDU length {n} above 65535"),
            PduError::DictionaryId => write!(f, "a Dictionary ID is present exactly with encoding 2"),
            PduError::Truncated => write!(f, "the bytes disagree with the Length field"),
            PduError::Short(n) => write!(f, "Length {n} is shorter than the fixed fields"),
            PduError::Stream => write!(f, "priority 0-3, stream number 0-63"),
            PduError::Slice => write!(f, "a slice carries at least one byte"),
        }
    }
}

impl std::error::Error for PduError {}

/// CRC-32/ISO-HDLC (`pdu-crc32`): reflected polynomial 0xEDB88320, initial
/// value and final XOR all ones; 0xCBF43926 over b"123456789".
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

/// SPEC 12.1 Stream byte: bits 7-6 priority (0 highest), bits 5-0 stream number.
pub fn stream_byte(priority: u8, number: u8) -> Result<u8, PduError> {
    if priority > 3 || number > 63 {
        return Err(PduError::Stream);
    }
    Ok(priority << 6 | number)
}

/// One PDU (SPEC 12.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pdu {
    /// Type ([`pdu_type`]).
    pub pdu_type: u8,
    /// The whole Stream byte ([`stream_byte`]).
    pub stream: u8,
    /// Encoding: 0 raw, 1 zstd, 2 zstd with dictionary, 3 schema.
    pub encoding: u8,
    /// The 4-byte Dictionary ID, present exactly with encoding 2.
    pub dictionary_id: Option<[u8; 4]>,
    /// The body as sent.
    pub body: Vec<u8>,
    /// CRC-32 of the decoded body.
    pub crc: u32,
}

impl Pdu {
    /// An uncompressed PDU (encoding 0): its CRC-32 is over `body`.
    pub fn raw(pdu_type: u8, body: &[u8], stream: u8) -> Self {
        Pdu { pdu_type, stream, encoding: 0, dictionary_id: None, body: body.to_vec(), crc: crc32(body) }
    }

    /// The Length field: every byte after it, the CRC-32 included.
    pub fn length(&self) -> usize {
        3 + if self.dictionary_id.is_some() { 4 } else { 0 } + self.body.len() + 4
    }

    /// For a raw PDU, the CRC-32 matches the body (compressed PDUs are
    /// checked after decompression, against the decoded body).
    pub fn crc_ok(&self) -> bool {
        self.encoding != 0 || crc32(&self.body) == self.crc
    }

    /// The PDU as sent, Length field first.
    pub fn encode(&self) -> Result<Vec<u8>, PduError> {
        if (self.encoding == ENCODING_DICT) != self.dictionary_id.is_some() {
            return Err(PduError::DictionaryId);
        }
        let n = self.length();
        if n > 0xFFFF {
            return Err(PduError::TooLong(n));
        }
        let mut out = Vec::with_capacity(2 + n);
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&[self.pdu_type, self.stream, self.encoding]);
        if let Some(d) = &self.dictionary_id {
            out.extend_from_slice(d);
        }
        out.extend_from_slice(&self.body);
        out.extend_from_slice(&self.crc.to_be_bytes());
        Ok(out)
    }

    /// Parse one whole PDU (Length field included).
    pub fn decode(data: &[u8]) -> Result<Self, PduError> {
        if data.len() < 2 {
            return Err(PduError::Truncated);
        }
        let length = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() != 2 + length {
            return Err(PduError::Truncated);
        }
        if length < 7 {
            return Err(PduError::Short(length));
        }
        let (pdu_type, stream, encoding) = (data[2], data[3], data[4]);
        let (dictionary_id, at) = if encoding == ENCODING_DICT {
            if length < 11 {
                return Err(PduError::Short(length));
            }
            (Some([data[5], data[6], data[7], data[8]]), 9)
        } else {
            (None, 5)
        };
        let n = data.len();
        Ok(Pdu {
            pdu_type,
            stream,
            encoding,
            dictionary_id,
            body: data[at..n - 4].to_vec(),
            crc: u32::from_be_bytes([data[n - 4], data[n - 3], data[n - 2], data[n - 1]]),
        })
    }
}

/// PDUs into block payloads, back to back (`pdu-filler`,
/// `pdu-length-within-block`): [`BlockPacker::push`] queues whole PDUs and
/// [`BlockPacker::take`] gives the next block, completed with filler when
/// fewer bytes are queued.
#[derive(Clone, Debug, Default)]
pub struct BlockPacker {
    buf: Vec<u8>,
}

impl BlockPacker {
    /// An empty packer, at a block boundary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a PDU; a filler byte goes first when it would leave its Length
    /// field one byte from a block boundary.
    pub fn push(&mut self, pdu: &Pdu) -> Result<(), PduError> {
        let bytes = pdu.encode()?;
        if self.buf.len() % BLOCK_BYTES == BLOCK_BYTES - 1 {
            self.buf.push(0);
        }
        self.buf.extend(bytes);
        Ok(())
    }

    /// Bytes queued (whole PDUs and any filler byte between them).
    pub fn queued(&self) -> usize {
        self.buf.len()
    }

    /// The next block's payload, None when nothing is queued. A last partial
    /// block is completed with zeros, which start at a PDU boundary, so they
    /// read as filler.
    pub fn take(&mut self) -> Option<Block> {
        if self.buf.is_empty() {
            return None;
        }
        let n = self.buf.len().min(BLOCK_BYTES);
        let mut out = [0u8; BLOCK_BYTES];
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Some(out)
    }
}

/// The block payloads carrying `pdus` back to back, the last completed with
/// filler: a session stream from block boundary to block boundary, or one
/// broadcast frame's complete PDUs.
pub fn pack_blocks(pdus: &[Pdu]) -> Result<Vec<Block>, PduError> {
    let mut p = BlockPacker::new();
    for pdu in pdus {
        p.push(pdu)?;
    }
    Ok(std::iter::from_fn(|| p.take()).collect())
}

/// Block payloads, in sequence order, back into PDUs. [`BlockParser::feed`]
/// returns the PDUs a block completes; a malformed one comes back as an
/// error (its Length still framed it, so parsing goes on).
#[derive(Clone, Debug, Default)]
pub struct BlockParser {
    pdu: Vec<u8>,
    need: usize,
}

impl BlockParser {
    /// A parser at a block boundary, outside any PDU.
    pub fn new() -> Self {
        Self::default()
    }

    /// A PDU is open across the next block.
    pub fn in_pdu(&self) -> bool {
        self.need > 0
    }

    /// Parse the next block.
    pub fn feed(&mut self, block: &Block) -> Vec<Result<Pdu, PduError>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < BLOCK_BYTES {
            if self.need > 0 {
                let n = self.need.min(BLOCK_BYTES - pos);
                self.pdu.extend_from_slice(&block[pos..pos + n]);
                pos += n;
                self.need -= n;
                if self.need == 0 {
                    out.push(Pdu::decode(&self.pdu));
                }
                continue;
            }
            if BLOCK_BYTES - pos == 1 {
                break; // one byte left at a PDU boundary: filler
            }
            let length = u16::from_be_bytes([block[pos], block[pos + 1]]) as usize;
            if length == 0 {
                break; // filler: the rest of the block
            }
            self.pdu = block[pos..pos + 2].to_vec();
            self.need = length;
            pos += 2;
        }
        out
    }
}

/// Every PDU completed in `blocks` (one still open at the end is left out).
pub fn unpack_blocks(blocks: &[Block]) -> Vec<Result<Pdu, PduError>> {
    let mut p = BlockParser::new();
    blocks.iter().flat_map(|b| p.feed(b)).collect()
}

/// A payload's 1,008 information bits (0/1), bytes in order, each most
/// significant bit first (`data-block-byte-order`): the payload
/// [`crate::tx::Transmitter::build_frame`] takes.
pub fn block_bits(payload: &Block) -> Vec<u8> {
    payload.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect()
}

/// The 126 payload bytes of a data block's information bits (the first
/// 1,008; a CRC after them is ignored).
pub fn bits_to_payload(bits: &[u8]) -> Block {
    let mut out = [0u8; BLOCK_BYTES];
    for (o, c) in out.iter_mut().zip(bits.chunks(8)) {
        *o = c.iter().fold(0, |a, &b| (a << 1) | (b & 1));
    }
    out
}

// --- broadcast fragmentation (ADR 0009 broadcast-message-fragments) -----------------

/// The largest body one PDU can carry alone in `n_blocks` data blocks.
pub fn max_body(n_blocks: usize, dictionary: bool) -> usize {
    (n_blocks * BLOCK_BYTES).saturating_sub(9 + if dictionary { 4 } else { 0 })
}

/// One frame of a broadcast message: its PDU and header flag bits 6
/// (`first`, first fragment) and 2 (`more`, more fragments).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// Header flag bit 6.
    pub first: bool,
    /// Header flag bit 2.
    pub more: bool,
    /// The frame's one complete PDU.
    pub pdu: Pdu,
}

/// A raw message as one frame per slice of at most `slice_bytes`, one
/// complete PDU each with the same Type and Stream (`first-fragment-flag`):
/// a message that fits one slice is one frame with flag bit 6 set and bit 2
/// clear; otherwise the first frame sets both, middle frames bit 2 only and
/// the last neither. The frames go out on consecutive frame counters.
pub fn fragment(pdu_type: u8, body: &[u8], slice_bytes: usize, stream: u8) -> Result<Vec<Fragment>, PduError> {
    if slice_bytes == 0 {
        return Err(PduError::Slice);
    }
    let slices: Vec<&[u8]> = if body.is_empty() { vec![body] } else { body.chunks(slice_bytes).collect() };
    let n = slices.len();
    Ok(slices
        .into_iter()
        .enumerate()
        .map(|(i, s)| Fragment { first: i == 0, more: i + 1 < n, pdu: Pdu::raw(pdu_type, s, stream) })
        .collect())
}

/// Broadcast messages back from their frames, per source `K`
/// (`broadcast-message-fragments`): a message starts at a frame with flag
/// bit 6; each frame whose counter is one higher appends its body; the frame
/// with flag bit 2 clear completes it. A gap (a missing counter, or a new
/// first fragment) discards the partial message, and a continuation with no
/// start held is discarded. Identical repeats (flag bit 5) are resolved
/// before this.
#[derive(Clone, Debug)]
pub struct Reassembler<K> {
    open: HashMap<K, (u16, Pdu)>,
}

impl<K: Eq + Hash> Default for Reassembler<K> {
    fn default() -> Self {
        Reassembler { open: HashMap::new() }
    }
}

impl<K: Eq + Hash> Reassembler<K> {
    /// Nothing held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one frame's PDU; returns the completed message as one raw PDU of
    /// the first fragment's Type and Stream, or None.
    pub fn feed(&mut self, source: K, counter: u16, first: bool, more: bool, pdu: Pdu) -> Option<Pdu> {
        let held = self.open.remove(&source);
        if first {
            if !more {
                return Some(pdu); // a complete message
            }
            self.open.insert(source, (counter, pdu));
            return None;
        }
        let (last, mut acc) = held?;
        if last.wrapping_add(1) != counter || (pdu.pdu_type, pdu.stream) != (acc.pdu_type, acc.stream) {
            return None;
        }
        acc.body.extend_from_slice(&pdu.body);
        if more {
            self.open.insert(source, (counter, acc));
            return None;
        }
        Some(Pdu::raw(acc.pdu_type, &acc.body, acc.stream))
    }
}
