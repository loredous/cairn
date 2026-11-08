//! Frame header and control block fields (SPEC 8.2, 8.3, Appendix B,
//! ADRs 0008-0012), mirroring `sim/cairn_sim/fields.py` and checked against
//! `vectors/header/`, `vectors/control/`, `vectors/frame/` and
//! `vectors/link/`.
//!
//! Plain structs with `pack`/`unpack`: [`Header`] (ADR 0011
//! `header-field-layout`: flag bits 6 first fragment and 7 block map, the
//! Mask ID), [`ControlBlock`] over [`AckBlock`] (cumulative, with the mask
//! ID), [`ConnectBlock`] (B.3, also the dictionary-only and link-test
//! forms), [`CalRequestBlock`], [`CalReportBlock`], [`IdBlock`], [`PathBlock`]
//! and [`BlockMapBlock`] (B.8). The link rules that sit on the fields:
//! [`serial_diff`] (RFC 1982), [`plan_blocks`] and [`block_sequences`]
//! (ADR 0008), [`check_controls`] (control-block orders, ADR 0010 and ADR
//! 0012) and [`session_width`] (ADR 0011).
//!
//! Bits are `u8` values 0/1, first bit on air first; every field is
//! big-endian and every block ends with its CRC-16/CCITT-FALSE over the bits
//! before it. The readings chosen where the SPEC leaves room are listed in
//! `vectors/README.md` ("Field readings"): "bit n" has weight 2^n (bit 0 sent
//! last); one-bit-per-item fields (ACK bitmap, tone masks, CAL-REPORT clip
//! flags) put item i at bit i; sequences of multi-bit items put the first
//! item in the most significant bits; absent optional fields are zeros;
//! reserved bits are sent as zero and ignored on receipt (a header whose
//! version is not 1 is rejected). The second, dictionary-only B.3 block
//! sends width cap and maximum frame time as 0: a B.3 block with both 0 is
//! read as that block, any other out-of-range value is malformed.

use std::fmt;

use crate::crc;
use crate::params::{CONTROL_INFO_BITS, CRC_BITS, HEADER_INFO_BITS, MAX_CONTROL_BLOCKS, MAX_DATA_BLOCKS, MAX_TONES};

/// Header bits before the CRC.
pub const HEADER_MESSAGE_BITS: usize = HEADER_INFO_BITS - CRC_BITS;
/// Control block bits before the CRC.
pub const CONTROL_MESSAGE_BITS: usize = CONTROL_INFO_BITS - CRC_BITS;
/// Protocol version (SPEC 8.2).
pub const VERSION: u8 = 1;
/// All 56 tones usable.
pub const ALL_TONES: u64 = (1 << MAX_TONES) - 1;
/// Mask IDs are 2 bits (ADR 0011 `mask-id`).
pub const MASK_IDS: u8 = 4;
/// Blocks one block map or ACK bitmap spans; also the send window (ADR 0008).
pub const BLOCK_SPAN: usize = 128;

/// Appendix B control types (2 and 8-15 reserved, ADR 0008
/// `reserved-control-types-after-block-map`).
pub mod control_type {
    #![allow(missing_docs)]
    pub const ACK: u8 = 0;
    pub const CONNECT: u8 = 1;
    pub const CAL_REQUEST: u8 = 3;
    pub const CAL_REPORT: u8 = 4;
    pub const ID: u8 = 5;
    pub const PATH: u8 = 6;
    pub const BLOCK_MAP: u8 = 7;
}

/// `b - a` in serial-number arithmetic (RFC 1982, 16 bits; ADR 0008
/// `block-sequence-rules`): positive when `b` is after `a`, -32767 ..= 32768.
pub fn serial_diff(a: u16, b: u16) -> i32 {
    let d = b.wrapping_sub(a) as i32;
    if d > 32768 { d - 65536 } else { d }
}

/// A station's session width W in tones (ADR 0011 `session-width`): its B.3
/// width cap + 1 when its peer advertised capability bit 0, else 24.
pub fn session_width(width_cap: u8, peer_capabilities: u16) -> usize {
    if peer_capabilities & capability::WIDE != 0 { width_cap as usize + 1 } else { 24 }
}

/// SPEC 8.4 frame types (11-30 reserved).
pub mod frame_type {
    #![allow(missing_docs)]
    pub const BEACON: u8 = 0;
    pub const UI: u8 = 1;
    pub const CONNECT: u8 = 2;
    pub const CONNECT_ACK: u8 = 3;
    pub const DATA: u8 = 4;
    pub const ACK: u8 = 5;
    pub const SOUND: u8 = 6;
    pub const DISCONNECT: u8 = 7;
    pub const PROBE: u8 = 8;
    pub const CAL_REQUEST: u8 = 9;
    pub const POLL: u8 = 10;
    pub const EXTENDED: u8 = 31;
}

/// B.3 capability bits.
pub mod capability {
    #![allow(missing_docs)]
    pub const WIDE: u16 = 1 << 0;
    pub const HARQ: u16 = 1 << 1;
    pub const SIGNATURES: u16 = 1 << 2;
    pub const SCHEMA: u16 = 1 << 3;
    pub const SHORT_HEADERS: u16 = 1 << 4;
    pub const CALIBRATION: u16 = 1 << 5;
    pub const MULTIPLEX: u16 = 1 << 6;
    pub const DICT_FETCH: u16 = 1 << 7;
}

/// Why a header or control block was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldError {
    /// Wrong number of bits.
    Length(usize),
    /// CRC-16 failed.
    Crc,
    /// A value that does not fit its field or its range.
    Range(&'static str, u128),
    /// Header version other than 1.
    Version(u8),
    /// Header flag bit 0 disagrees with the control block count (SPEC 8.2).
    ControlFlag,
    /// A malformed address or text field.
    Malformed(&'static str),
    /// Fields that break a frame-level rule (block sequences, control-block order).
    Rule(&'static str),
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldError::Length(n) => write!(f, "wrong length ({n} bits)"),
            FieldError::Crc => write!(f, "CRC-16 failed"),
            FieldError::Range(name, v) => write!(f, "{name} = {v} out of range"),
            FieldError::Version(v) => write!(f, "header version {v} is not {VERSION}"),
            FieldError::ControlFlag => write!(f, "header flag bit 0 disagrees with the control block count"),
            FieldError::Malformed(what) => write!(f, "malformed {what}"),
            FieldError::Rule(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for FieldError {}

struct Writer(Vec<u8>);

impl Writer {
    fn put(&mut self, value: u128, width: usize, name: &'static str) -> Result<(), FieldError> {
        if width < 128 && value >> width != 0 {
            return Err(FieldError::Range(name, value));
        }
        self.0.extend((0..width).rev().map(|i| if i < 128 { ((value >> i) & 1) as u8 } else { 0 }));
        Ok(())
    }
}

struct Reader<'a> {
    bits: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn get(&mut self, width: usize) -> u128 {
        let v = self.bits[self.pos..self.pos + width].iter().fold(0u128, |a, &b| (a << 1) | (b & 1) as u128);
        self.pos += width;
        v
    }
}

fn check_crc(bits: &[u8], n: usize) -> Result<&[u8], FieldError> {
    if bits.len() != n {
        return Err(FieldError::Length(bits.len()));
    }
    if !crc::check(bits) {
        return Err(FieldError::Crc);
    }
    Ok(&bits[..n - CRC_BITS])
}

fn flags(bits: &[bool]) -> u128 {
    bits.iter().enumerate().map(|(i, &b)| (b as u128) << i).sum()
}

fn flag(v: u128, bit: usize) -> bool {
    (v >> bit) & 1 == 1
}

/// Usable tones -> 56-bit mask field (tone k at bit k).
pub fn tones_to_mask(usable: &[usize]) -> u64 {
    usable.iter().filter(|&&k| k < MAX_TONES).fold(0, |m, &k| m | 1 << k)
}

/// 56-bit mask field -> usable tones, lowest first.
pub fn mask_to_tones(mask: u64) -> Vec<usize> {
    (0..MAX_TONES).filter(|&k| (mask >> k) & 1 == 1).collect()
}

// --- SHA-256 (FIPS 180-4), for the long-callsign hash only ----------------------------

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

// --- addresses (SPEC 8.3, B.1) ------------------------------------------------------

const ALPHABET: &[u8; 38] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/-";
const HASH_CODE: u64 = 63;
const HASH_MASK: u32 = (1 << 30) - 1;
/// The broadcast address field value (all 40 bits set).
pub const BROADCAST_VALUE: u64 = (1 << 40) - 1;

/// 30-bit hash of a long callsign: the first 30 bits of SHA-256 over the
/// upper-case ASCII text; all ones becomes all ones minus one.
pub fn callsign_hash(text: &str) -> u32 {
    let d = sha256(text.to_ascii_uppercase().as_bytes());
    let h = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) >> 2;
    if h == HASH_MASK { h - 1 } else { h }
}

/// A header address as it is on air.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    /// Up to six characters from the B.1 alphabet, and an SSID.
    Callsign {
        /// Upper-case callsign, 1-6 characters.
        text: String,
        /// SSID, 0-15.
        ssid: u8,
    },
    /// A 7-10 character callsign, sent as code 63 and its 30-bit hash.
    Hashed {
        /// The hash (`callsign_hash`).
        hash30: u32,
        /// SSID, 0-15.
        ssid: u8,
    },
    /// All ones.
    Broadcast,
}

impl Address {
    /// The address of `callsign` (1-10 characters of A-Z, 0-9, '/', '-'; upper-cased)
    /// with `ssid`; a callsign longer than six characters becomes its hash.
    pub fn new(callsign: &str, ssid: u8) -> Result<Self, FieldError> {
        let text = callsign.to_ascii_uppercase();
        if !(1..=10).contains(&text.len()) || !text.bytes().all(|c| ALPHABET.contains(&c)) {
            return Err(FieldError::Malformed("callsign"));
        }
        if ssid > 15 {
            return Err(FieldError::Range("SSID", ssid as u128));
        }
        Ok(if text.len() > 6 {
            Address::Hashed { hash30: callsign_hash(&text), ssid }
        } else {
            Address::Callsign { text, ssid }
        })
    }

    /// The 40-bit field value.
    pub fn encode(&self) -> u64 {
        match self {
            Address::Broadcast => BROADCAST_VALUE,
            Address::Hashed { hash30, ssid } => (HASH_CODE << 34) | ((*hash30 as u64) << 4) | *ssid as u64,
            Address::Callsign { text, ssid } => {
                let mut v = 0u64;
                for i in 0..6 {
                    let c = text.as_bytes().get(i).map_or(0, |&b| ALPHABET.iter().position(|&a| a == b).unwrap() + 1);
                    v = (v << 6) | c as u64;
                }
                (v << 4) | *ssid as u64
            }
        }
    }

    /// Parse a 40-bit field value.
    pub fn decode(value: u64) -> Result<Self, FieldError> {
        if value > BROADCAST_VALUE {
            return Err(FieldError::Range("address", value as u128));
        }
        if value == BROADCAST_VALUE {
            return Ok(Address::Broadcast);
        }
        let ssid = (value & 0xF) as u8;
        let codes: Vec<u64> = (0..6).map(|i| (value >> (4 + 6 * (5 - i))) & 0x3F).collect();
        if codes[0] == HASH_CODE {
            return Ok(Address::Hashed { hash30: ((value >> 4) as u32) & HASH_MASK, ssid });
        }
        let mut text = String::new();
        for (i, &c) in codes.iter().enumerate() {
            if c == 0 {
                if codes[i..].iter().any(|&x| x != 0) {
                    return Err(FieldError::Malformed("address (padding before a character)"));
                }
                break;
            }
            if c as usize > ALPHABET.len() {
                return Err(FieldError::Malformed("address (reserved character code)"));
            }
            text.push(ALPHABET[c as usize - 1] as char);
        }
        if text.is_empty() {
            return Err(FieldError::Malformed("address (no characters)"));
        }
        Ok(Address::Callsign { text, ssid })
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Broadcast => write!(f, "*"),
            Address::Callsign { text, ssid: 0 } => write!(f, "{text}"),
            Address::Callsign { text, ssid } => write!(f, "{text}-{ssid}"),
            Address::Hashed { hash30, ssid } => write!(f, "#{hash30:08x}-{ssid}"),
        }
    }
}

// --- header (SPEC 8.2) ------------------------------------------------------------

/// The 144-bit frame header (SPEC 8.2, ADR 0011 `header-field-layout`).
/// Flag bits 0 and 1 follow from the block counts; the data block count is
/// sent as count - 1; the Mask ID and one reserved bit follow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Protocol version (1).
    pub version: u8,
    /// Frame type (`frame_type`), 0-31.
    pub frame_type: u8,
    /// Source address.
    pub source: Address,
    /// Destination address.
    pub destination: Address,
    /// Session frame: the first (lowest) data block's sequence, or the next
    /// unsent one without data (ADR 0008); BEACON, UI, PROBE: the frame
    /// counter (ADR 0010).
    pub sequence: u16,
    /// Data rung, 0-15 (sent as given and ignored without data blocks).
    pub mcs: u8,
    /// Control blocks, 0-3.
    pub n_control: usize,
    /// Data blocks, 0-128.
    pub n_data: usize,
    /// Flag bit 2: more fragments (broadcast).
    pub more_fragments: bool,
    /// Flag bit 3: session frame.
    pub session: bool,
    /// Flag bit 4: width probe follows the header.
    pub probe: bool,
    /// Flag bit 5: identical repeat of a broadcast frame.
    pub repeat: bool,
    /// Flag bit 6: first fragment, on every BEACON and UI frame that starts
    /// a message (ADR 0009 `first-fragment-flag`).
    pub first_fragment: bool,
    /// Flag bit 7: the second control block is a block map (ADR 0008).
    pub block_map: bool,
    /// Session frame: ID of the tone mask applied (ADR 0011 `mask-id`), 0-3; 0 otherwise.
    pub mask_id: u8,
}

impl Header {
    /// A header with no flags, sequence 0, MCS 0 and no blocks.
    pub fn new(frame_type: u8, source: Address, destination: Address) -> Self {
        Header {
            version: VERSION,
            frame_type,
            source,
            destination,
            sequence: 0,
            mcs: 0,
            n_control: 0,
            n_data: 0,
            more_fragments: false,
            session: false,
            probe: false,
            repeat: false,
            first_fragment: false,
            block_map: false,
            mask_id: 0,
        }
    }

    /// The 8-bit flags field.
    pub fn flags(&self) -> u8 {
        flags(&[
            self.n_control > 0,
            self.n_data > 0,
            self.more_fragments,
            self.session,
            self.probe,
            self.repeat,
            self.first_fragment,
            self.block_map,
        ]) as u8
    }

    /// The 128 bits before the CRC.
    pub fn message_bits(&self) -> Result<Vec<u8>, FieldError> {
        if self.n_control > MAX_CONTROL_BLOCKS {
            return Err(FieldError::Range("control block count", self.n_control as u128));
        }
        if self.n_data > MAX_DATA_BLOCKS {
            return Err(FieldError::Range("data block count", self.n_data as u128));
        }
        let mut w = Writer(Vec::with_capacity(HEADER_MESSAGE_BITS));
        w.put(self.version as u128, 3, "version")?;
        w.put(self.frame_type as u128, 5, "frame type")?;
        w.put(self.flags() as u128, 8, "flags")?;
        w.put(self.source.encode() as u128, 40, "source")?;
        w.put(self.destination.encode() as u128, 40, "destination")?;
        w.put(self.sequence as u128, 16, "sequence")?;
        w.put(self.mcs as u128, 4, "MCS")?;
        w.put(self.n_control as u128, 2, "control block count")?;
        w.put(self.n_data.saturating_sub(1) as u128, 7, "data block count")?;
        w.put(self.mask_id as u128, 2, "mask ID")?;
        w.put(0, 1, "reserved")?;
        Ok(w.0)
    }

    /// All 144 bits, CRC included.
    pub fn bits(&self) -> Result<Vec<u8>, FieldError> {
        Ok(crc::append_crc(&self.message_bits()?))
    }

    /// Parse 144 header bits.
    pub fn unpack(bits: &[u8]) -> Result<Self, FieldError> {
        let mut r = Reader { bits: check_crc(bits, HEADER_INFO_BITS)?, pos: 0 };
        let version = r.get(3) as u8;
        if version != VERSION {
            return Err(FieldError::Version(version));
        }
        let frame_type = r.get(5) as u8;
        let fl = r.get(8);
        let source = Address::decode(r.get(40) as u64)?;
        let destination = Address::decode(r.get(40) as u64)?;
        let sequence = r.get(16) as u16;
        let mcs = r.get(4) as u8;
        let n_control = r.get(2) as usize;
        let count = r.get(7) as usize;
        let mask_id = r.get(2) as u8;
        if flag(fl, 0) != (n_control > 0) {
            return Err(FieldError::ControlFlag);
        }
        Ok(Header {
            version,
            frame_type,
            source,
            destination,
            sequence,
            mcs,
            n_control,
            n_data: if flag(fl, 1) { count + 1 } else { 0 },
            more_fragments: flag(fl, 2),
            session: flag(fl, 3),
            probe: flag(fl, 4),
            repeat: flag(fl, 5),
            first_fragment: flag(fl, 6),
            block_map: flag(fl, 7),
            mask_id,
        })
    }
}

// --- control blocks (Appendix B) ------------------------------------------------------

/// B.2 measured SNR code: 0.25 dB steps, nearest (halves up), saturating at 0 and 63.75 dB.
pub fn snr_code(snr_db: f64) -> u8 {
    (snr_db * 4.0 + 0.5).floor().clamp(0.0, 255.0) as u8
}

/// B.2 ACK control block (type 0, ADR 0011 `ack-control-block-layout`),
/// cumulative (ADR 0008 `cumulative-ack`): the base is the receiver's next
/// in-order sequence and bit i says it holds block base + i, so bit 0 is
/// clear. Every session ACK carries the tone mask with its ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AckBlock {
    /// The receiver's next in-order sequence (0 in a PROBE reply or refusal).
    pub base_sequence: u16,
    /// Bit i set: the receiver holds block base + i.
    pub bitmap: u128,
    /// Rung the sender should use next.
    pub requested_mcs: u8,
    /// Measured SNR in 0.25 dB steps (`snr_code`).
    pub snr_q: u8,
    /// Block errors in the peer's last frame.
    pub block_errors: u8,
    /// Usable tones (tone k at bit k), sent with flag bit 0.
    pub tone_mask: Option<u64>,
    /// Flag bit 2: re-probe requested.
    pub reprobe: bool,
    /// Flag bit 3: use the long prefix.
    pub long_prefix: bool,
    /// Distortion index, 0-7.
    pub distortion: u8,
    /// This receiver's input clipped.
    pub adc_clipped: bool,
    /// ID of `tone_mask` (ADR 0011 `mask-id`), 0-3; 0 outside a session.
    pub mask_id: u8,
}

impl AckBlock {
    /// Bitmap of `held[i]` at bit i.
    pub fn bitmap_from(held: &[bool]) -> u128 {
        flags(held)
    }

    /// (base sequence, bitmap) of a receiver whose next in-order sequence is
    /// `next_in_order` and that holds `held` beyond it (ADR 0008
    /// `cumulative-ack`); blocks at or before the base, or 128 or more
    /// beyond it, are left out.
    pub fn cumulative(next_in_order: u16, held: &[u16]) -> (u16, u128) {
        let bitmap = held
            .iter()
            .map(|&s| serial_diff(next_in_order, s))
            .filter(|&d| 0 < d && d < BLOCK_SPAN as i32)
            .fold(0u128, |m, d| m | 1 << d);
        (next_in_order, bitmap)
    }

    /// Bit i: the receiver holds block base + i.
    pub fn holds(&self, i: usize) -> bool {
        i < BLOCK_SPAN && (self.bitmap >> i) & 1 == 1
    }

    /// Block `seq` is acknowledged: before the base, or its bit is set.
    pub fn acknowledged(&self, seq: u16) -> bool {
        let d = serial_diff(self.base_sequence, seq);
        d < 0 || (d < BLOCK_SPAN as i32 && self.holds(d as usize))
    }

    /// Measured SNR, dB.
    pub fn snr_db(&self) -> f64 {
        self.snr_q as f64 / 4.0
    }
}

/// B.3 CONNECT / CONNECT-ACK control block (type 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectBlock {
    /// Session ID.
    pub session_id: u16,
    /// Capability bits (`capability`).
    pub capabilities: u16,
    /// Highest tone index this station transmits, 23-55.
    pub width_cap: u8,
    /// Rung the sender wants to receive at.
    pub requested_mcs: u8,
    /// Units of 0.1 s, 5-40.
    pub max_frame_time: u8,
    /// Usable tones (all ones in CONNECT).
    pub tone_mask: u64,
    /// Four 32-bit dictionary ID slots (zero: empty).
    pub dictionary_ids: [u32; 4],
    /// Flag bit 0: a second block of dictionary IDs follows.
    pub more_dictionaries: bool,
    /// Flag bit 1: resume the session.
    pub resume: bool,
}

impl ConnectBlock {
    /// The second B.3 block of CONNECT or CONNECT-ACK: dictionary IDs only,
    /// every other field zero (ADR 0010 `session-establishment-steps`).
    pub fn dictionaries(ids: [u32; 4]) -> Self {
        ConnectBlock { width_cap: 0, max_frame_time: 0, tone_mask: 0, dictionary_ids: ids, ..Default::default() }
    }

    /// A link-test PROBE's block (ADR 0012 `probe-frame-contents`): flags and
    /// session ID 0, requested MCS 0, all-ones mask, no dictionaries.
    pub fn link_test(capabilities: u16, width_cap: u8, max_frame_time: u8) -> Self {
        ConnectBlock { capabilities, width_cap, max_frame_time, ..Default::default() }
    }

    /// A dictionary-ID block: width cap and max frame time both 0.
    pub fn is_dictionary_only(&self) -> bool {
        self.width_cap == 0 && self.max_frame_time == 0
    }

    /// The fields ADR 0012 fixes for a link-test PROBE's block.
    pub fn is_link_test(&self) -> bool {
        !self.more_dictionaries
            && !self.resume
            && self.session_id == 0
            && self.requested_mcs == 0
            && self.tone_mask == ALL_TONES
            && self.dictionary_ids == [0; 4]
            && !self.is_dictionary_only()
    }

    /// Tones this station transmits at most: width cap + 1 (ADR 0011).
    pub fn width(&self) -> usize {
        self.width_cap as usize + 1
    }

    fn check(cap: u128, mft: u128) -> Result<(), FieldError> {
        if cap == 0 && mft == 0 {
            return Ok(()); // a dictionary-only block
        }
        if !(23..=55).contains(&cap) {
            return Err(FieldError::Range("width cap", cap));
        }
        if !(5..=40).contains(&mft) {
            return Err(FieldError::Range("max frame time", mft));
        }
        Ok(())
    }
}

impl Default for ConnectBlock {
    fn default() -> Self {
        ConnectBlock {
            session_id: 0,
            capabilities: 0,
            width_cap: 23,
            requested_mcs: 0,
            max_frame_time: 40,
            tone_mask: ALL_TONES,
            dictionary_ids: [0; 4],
            more_dictionaries: false,
            resume: false,
        }
    }
}

/// B.5 CAL-REQUEST control block (type 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalRequestBlock {
    /// Units of 10 Hz.
    pub tone_freq: u8,
    /// Steps.
    pub step_count: u8,
    /// Units of 10 ms.
    pub step_length: u8,
    /// Units of 10 ms.
    pub gap_length: u8,
    /// dB below nominal output, 0.25 dB steps.
    pub start_level: u8,
    /// dB per step.
    pub step_size: u8,
}

impl Default for CalRequestBlock {
    fn default() -> Self {
        CalRequestBlock {
            tone_freq: 100,
            step_count: 8,
            step_length: 40,
            gap_length: 10,
            start_level: 96,
            step_size: 3,
        }
    }
}

/// B.5 CAL-REPORT control block (type 4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalReportBlock {
    /// Steps 1-8: dB below full scale, 0.25 dB steps.
    pub amplitudes: [u8; 8],
    /// Steps 1-8: harmonic distortion, percent (4 bits).
    pub distortion: [u8; 8],
    /// Steps 1-8: receiver clipped (step j at bit j - 1).
    pub clipped: [bool; 8],
    /// Flag bit 0: every step heard.
    pub sweep_valid: bool,
}

/// B.6 ID control block (type 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdBlock {
    /// Callsign, 1-10 printable ASCII characters, upper case, no spaces.
    pub callsign: String,
    /// SSID.
    pub ssid: u8,
    /// Maidenhead locator, 4 or 6 characters (flag bit 0).
    pub grid: Option<String>,
}

/// B.7 path control block (type 6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathBlock {
    /// 0-2 relay entries.
    pub entries: Vec<Address>,
    /// Hops remaining, 0-3.
    pub hops: u8,
    /// Flag bits 0 and 1: entry used.
    pub used: [bool; 2],
    /// Flag bit 2: via (session relay).
    pub via: bool,
}

/// B.8 block map control block (type 7, ADR 0008 `block-map-control-block`):
/// bit i set, block `base_sequence` + i is in the frame; the frame's k-th
/// data block is the k-th set bit from bit 0. Unpacking only parses; the
/// receive rules are [`block_sequences`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMapBlock {
    /// Equal to the header's Sequence field.
    pub base_sequence: u16,
    /// Bit i set: block base + i is carried.
    pub bitmap: u128,
}

impl BlockMapBlock {
    /// The map of `sequences` (ascending in serial order, within 128 of the first).
    pub fn for_sequences(sequences: &[u16]) -> Result<Self, FieldError> {
        check_ascending(sequences)?;
        let base = sequences[0];
        Ok(BlockMapBlock {
            base_sequence: base,
            bitmap: sequences.iter().fold(0, |m, &s| m | 1 << serial_diff(base, s)),
        })
    }

    /// The sequences of the frame's data blocks, in order.
    pub fn sequences(&self) -> Vec<u16> {
        (0..BLOCK_SPAN)
            .filter(|&i| (self.bitmap >> i) & 1 == 1)
            .map(|i| self.base_sequence.wrapping_add(i as u16))
            .collect()
    }
}

/// `sequences` are 1-128 blocks in ascending serial order, all within 128 of
/// the first (one frame's blocks, ADR 0008 `block-sequence-rules`).
pub fn check_ascending(sequences: &[u16]) -> Result<(), FieldError> {
    if sequences.is_empty() || sequences.len() > MAX_DATA_BLOCKS {
        return Err(FieldError::Rule("a frame carries 1-128 data blocks"));
    }
    if sequences.windows(2).any(|w| serial_diff(w[0], w[1]) <= 0) {
        return Err(FieldError::Rule("block sequences not ascending"));
    }
    if serial_diff(sequences[0], sequences[sequences.len() - 1]) >= BLOCK_SPAN as i32 {
        return Err(FieldError::Rule("block sequences span 128 or more"));
    }
    Ok(())
}

/// A session frame carrying the blocks `sequences` (ascending): the header's
/// Sequence (the lowest) and the block map to send, None when the blocks are
/// consecutive (ADR 0008 `block-map-control-block`). Payloads go in the
/// order of `sequences`.
pub fn plan_blocks(sequences: &[u16]) -> Result<(u16, Option<BlockMapBlock>), FieldError> {
    check_ascending(sequences)?;
    if serial_diff(sequences[0], sequences[sequences.len() - 1]) == sequences.len() as i32 - 1 {
        return Ok((sequences[0], None));
    }
    Ok((sequences[0], Some(BlockMapBlock::for_sequences(sequences)?)))
}

/// The sequence of each data block of a decoded frame (ADR 0008):
/// consecutive from the header's Sequence when flag bit 7 is clear; with it
/// set, as the block map in the second control block names them, which
/// must have bit 0 set, base equal to the Sequence and one set bit per data
/// block. None when flag bit 7 is set and the map is missing, failed
/// (`None` in `controls`) or breaks a rule: no data block then counts as
/// received.
pub fn block_sequences(header: &Header, controls: &[Option<ControlBlock>]) -> Option<Vec<u16>> {
    if !header.block_map {
        return Some((0..header.n_data).map(|k| header.sequence.wrapping_add(k as u16)).collect());
    }
    match controls.get(1) {
        Some(Some(ControlBlock::BlockMap(m)))
            if m.base_sequence == header.sequence
                && m.bitmap & 1 == 1
                && m.bitmap.count_ones() as usize == header.n_data =>
        {
            Some(m.sequences())
        }
        _ => None,
    }
}

/// A frame's block map agrees with its header (ADR 0008): flag bit 7 set
/// exactly when the second control block is a block map whose base is the
/// header's Sequence, bit 0 set and one bit per data block.
pub fn check_block_map(header: &Header, controls: &[ControlBlock]) -> Result<(), FieldError> {
    if header.block_map {
        let parsed: Vec<Option<ControlBlock>> = controls.iter().cloned().map(Some).collect();
        if block_sequences(header, &parsed).is_none() {
            return Err(FieldError::Rule(
                "flag bit 7 needs a block map as the second control block, base = Sequence, bit 0 set, one bit per data block",
            ));
        }
    } else if controls.iter().any(|c| matches!(c, ControlBlock::BlockMap(_))) {
        return Err(FieldError::Rule("a block map needs header flag bit 7"));
    }
    Ok(())
}

/// A control block of any Appendix B type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlBlock {
    /// Type 0.
    Ack(AckBlock),
    /// Type 1.
    Connect(ConnectBlock),
    /// Type 3.
    CalRequest(CalRequestBlock),
    /// Type 4.
    CalReport(CalReportBlock),
    /// Type 5.
    Id(IdBlock),
    /// Type 6.
    Path(PathBlock),
    /// Type 7.
    BlockMap(BlockMapBlock),
    /// A reserved type (2, 8-15): receivers skip it.
    Unknown {
        /// The type field.
        control_type: u8,
        /// The 240 message bits.
        message: Vec<u8>,
    },
}

fn put_ascii(w: &mut Writer, text: &str, n: usize, name: &'static str) -> Result<(), FieldError> {
    if text.len() > n || !text.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return Err(FieldError::Malformed(name));
    }
    for i in 0..n {
        w.put(*text.as_bytes().get(i).unwrap_or(&b' ') as u128, 8, name)?;
    }
    Ok(())
}

fn get_ascii(r: &mut Reader, n: usize, name: &'static str) -> Result<String, FieldError> {
    let bytes: Vec<u8> = (0..n).map(|_| r.get(8) as u8).collect();
    if !bytes.iter().all(|b| (0x20..=0x7E).contains(b)) {
        return Err(FieldError::Malformed(name));
    }
    Ok(String::from_utf8(bytes).unwrap().trim_end_matches(' ').to_string())
}

impl ControlBlock {
    /// The control type field.
    pub fn control_type(&self) -> u8 {
        match self {
            ControlBlock::Ack(_) => 0,
            ControlBlock::Connect(_) => 1,
            ControlBlock::CalRequest(_) => 3,
            ControlBlock::CalReport(_) => 4,
            ControlBlock::Id(_) => 5,
            ControlBlock::Path(_) => 6,
            ControlBlock::BlockMap(_) => 7,
            ControlBlock::Unknown { control_type, .. } => *control_type,
        }
    }

    /// The 240 bits before the CRC.
    pub fn message_bits(&self) -> Result<Vec<u8>, FieldError> {
        if let ControlBlock::Unknown { message, .. } = self {
            return if message.len() == CONTROL_MESSAGE_BITS {
                Ok(message.clone())
            } else {
                Err(FieldError::Length(message.len()))
            };
        }
        let mut w = Writer(Vec::with_capacity(CONTROL_MESSAGE_BITS));
        w.put(self.control_type() as u128, 4, "control type")?;
        match self {
            ControlBlock::Ack(a) => {
                w.put(flags(&[a.tone_mask.is_some(), false, a.reprobe, a.long_prefix]), 4, "flags")?;
                w.put(a.base_sequence as u128, 16, "base sequence")?;
                w.put(a.bitmap, 128, "block bitmap")?;
                w.put(a.requested_mcs as u128, 4, "requested MCS")?;
                w.put(a.snr_q as u128, 8, "measured SNR")?;
                w.put(a.block_errors as u128, 8, "block errors")?;
                w.put(a.tone_mask.unwrap_or(0) as u128, 56, "tone mask")?;
                w.put(a.distortion as u128, 3, "distortion index")?;
                w.put(a.adc_clipped as u128, 1, "own ADC clipped")?;
                w.put(a.mask_id as u128, 2, "mask ID")?;
                w.put(0, 6, "reserved")?;
            }
            ControlBlock::Connect(c) => {
                ConnectBlock::check(c.width_cap as u128, c.max_frame_time as u128)?;
                w.put(flags(&[c.more_dictionaries, c.resume]), 4, "flags")?;
                w.put(c.session_id as u128, 16, "session ID")?;
                w.put(c.capabilities as u128, 16, "capabilities")?;
                w.put(c.width_cap as u128, 6, "width cap")?;
                w.put(c.requested_mcs as u128, 4, "requested MCS")?;
                w.put(c.max_frame_time as u128, 6, "max frame time")?;
                w.put(c.tone_mask as u128, 56, "tone mask")?;
                for &d in &c.dictionary_ids {
                    w.put(d as u128, 32, "dictionary ID")?;
                }
            }
            ControlBlock::CalRequest(c) => {
                w.put(0, 4, "flags")?;
                w.put(c.tone_freq as u128, 8, "tone frequency")?;
                w.put(c.step_count as u128, 4, "step count")?;
                w.put(c.step_length as u128, 8, "step length")?;
                w.put(c.gap_length as u128, 8, "gap length")?;
                w.put(c.start_level as u128, 8, "start level")?;
                w.put(c.step_size as u128, 4, "step size")?;
                w.put(0, 192, "reserved")?;
            }
            ControlBlock::CalReport(c) => {
                w.put(c.sweep_valid as u128, 4, "flags")?;
                for &a in &c.amplitudes {
                    w.put(a as u128, 8, "amplitude")?;
                }
                for &d in &c.distortion {
                    w.put(d as u128, 4, "distortion")?;
                }
                w.put(flags(&c.clipped), 8, "clip flags")?;
                w.put(0, 128, "reserved")?;
            }
            ControlBlock::Id(c) => {
                if c.callsign.is_empty() || c.callsign.contains(' ') || c.callsign != c.callsign.to_ascii_uppercase() {
                    return Err(FieldError::Malformed("ID callsign"));
                }
                if c.grid.as_ref().is_some_and(|g| g.len() != 4 && g.len() != 6) {
                    return Err(FieldError::Malformed("grid locator"));
                }
                w.put(c.grid.is_some() as u128, 4, "flags")?;
                put_ascii(&mut w, &c.callsign, 10, "ID callsign")?;
                w.put(c.ssid as u128, 4, "SSID")?;
                match &c.grid {
                    Some(g) => put_ascii(&mut w, g, 6, "grid locator")?,
                    None => w.put(0, 48, "grid locator")?,
                }
                w.put(0, 100, "reserved")?;
            }
            ControlBlock::Path(p) => {
                if p.entries.len() > 2 {
                    return Err(FieldError::Range("path entry count", p.entries.len() as u128));
                }
                w.put(flags(&[p.used[0], p.used[1], p.via]), 4, "flags")?;
                w.put(p.hops as u128, 2, "hops remaining")?;
                w.put(p.entries.len() as u128, 2, "entry count")?;
                for i in 0..2 {
                    w.put(p.entries.get(i).map_or(0, Address::encode) as u128, 40, "path entry")?;
                }
                w.put(0, 148, "reserved")?;
            }
            ControlBlock::BlockMap(m) => {
                w.put(0, 4, "flags")?;
                w.put(m.base_sequence as u128, 16, "base sequence")?;
                w.put(m.bitmap, 128, "block bitmap")?;
                w.put(0, 88, "reserved")?;
            }
            ControlBlock::Unknown { .. } => unreachable!(),
        }
        Ok(w.0)
    }

    /// All 256 bits, CRC included.
    pub fn bits(&self) -> Result<Vec<u8>, FieldError> {
        Ok(crc::append_crc(&self.message_bits()?))
    }

    /// Parse 256 control block bits; a reserved type comes back as `Unknown`.
    pub fn unpack(bits: &[u8]) -> Result<Self, FieldError> {
        let msg = check_crc(bits, CONTROL_INFO_BITS)?;
        let mut r = Reader { bits: msg, pos: 0 };
        let t = r.get(4) as u8;
        Ok(match t {
            0 => {
                let fl = r.get(4);
                let (base, bitmap, mcs, snr, errors, mask) =
                    (r.get(16), r.get(128), r.get(4), r.get(8), r.get(8), r.get(56));
                let (distortion, clipped, mask_id) = (r.get(3), r.get(1), r.get(2));
                ControlBlock::Ack(AckBlock {
                    base_sequence: base as u16,
                    bitmap,
                    requested_mcs: mcs as u8,
                    snr_q: snr as u8,
                    block_errors: errors as u8,
                    tone_mask: flag(fl, 0).then_some(mask as u64),
                    reprobe: flag(fl, 2),
                    long_prefix: flag(fl, 3),
                    distortion: distortion as u8,
                    adc_clipped: clipped == 1,
                    mask_id: mask_id as u8,
                })
            }
            1 => {
                let fl = r.get(4);
                let (sid, caps, cap, mcs, mft, mask) = (r.get(16), r.get(16), r.get(6), r.get(4), r.get(6), r.get(56));
                let mut dictionary_ids = [0u32; 4];
                for d in dictionary_ids.iter_mut() {
                    *d = r.get(32) as u32;
                }
                ConnectBlock::check(cap, mft)?;
                ControlBlock::Connect(ConnectBlock {
                    session_id: sid as u16,
                    capabilities: caps as u16,
                    width_cap: cap as u8,
                    requested_mcs: mcs as u8,
                    max_frame_time: mft as u8,
                    tone_mask: mask as u64,
                    dictionary_ids,
                    more_dictionaries: flag(fl, 0),
                    resume: flag(fl, 1),
                })
            }
            3 => {
                r.get(4);
                ControlBlock::CalRequest(CalRequestBlock {
                    tone_freq: r.get(8) as u8,
                    step_count: r.get(4) as u8,
                    step_length: r.get(8) as u8,
                    gap_length: r.get(8) as u8,
                    start_level: r.get(8) as u8,
                    step_size: r.get(4) as u8,
                })
            }
            4 => {
                let fl = r.get(4);
                let mut c = CalReportBlock { sweep_valid: flag(fl, 0), ..Default::default() };
                for a in c.amplitudes.iter_mut() {
                    *a = r.get(8) as u8;
                }
                for d in c.distortion.iter_mut() {
                    *d = r.get(4) as u8;
                }
                let clip = r.get(8);
                for (j, c) in c.clipped.iter_mut().enumerate() {
                    *c = flag(clip, j);
                }
                ControlBlock::CalReport(c)
            }
            5 => {
                let fl = r.get(4);
                let callsign = get_ascii(&mut r, 10, "ID callsign")?;
                let ssid = r.get(4) as u8;
                let grid = if flag(fl, 0) {
                    let g = get_ascii(&mut r, 6, "grid locator")?;
                    if g.len() != 4 && g.len() != 6 {
                        return Err(FieldError::Malformed("grid locator"));
                    }
                    Some(g)
                } else {
                    None
                };
                if callsign.is_empty() || callsign.contains(' ') || callsign != callsign.to_ascii_uppercase() {
                    return Err(FieldError::Malformed("ID callsign"));
                }
                ControlBlock::Id(IdBlock { callsign, ssid, grid })
            }
            6 => {
                let (fl, hops, count) = (r.get(4), r.get(2), r.get(2) as usize);
                let raw = [r.get(40) as u64, r.get(40) as u64];
                if count > 2 {
                    return Err(FieldError::Range("path entry count", count as u128));
                }
                let entries = raw[..count].iter().map(|&v| Address::decode(v)).collect::<Result<Vec<_>, _>>()?;
                ControlBlock::Path(PathBlock {
                    entries,
                    hops: hops as u8,
                    used: [flag(fl, 0), flag(fl, 1)],
                    via: flag(fl, 2),
                })
            }
            7 => {
                r.get(4);
                ControlBlock::BlockMap(BlockMapBlock { base_sequence: r.get(16) as u16, bitmap: r.get(128) })
            }
            _ => ControlBlock::Unknown { control_type: t, message: msg.to_vec() },
        })
    }
}

// --- control-block order (ADR 0010, ADR 0012) -------------------------------------------

/// Check a frame's control blocks against the order and contents fixed for
/// its type (a sender's check; receivers identify blocks by type):
///
/// - CONNECT: B.3 (requested MCS 0), ID, then a dictionary-only B.3 exactly
///   when the first sets flag bit 0 (ADR 0010 `session-establishment-steps`).
/// - CONNECT-ACK: B.3, ID, then on resume (flag bit 1) an ACK block,
///   otherwise a dictionary-only B.3 exactly when flag bit 0 is set.
/// - PROBE: nothing (a BEACON request, no width probe) or one link-test
///   B.3 (ADR 0012 `probe-frame-contents`); no data blocks.
/// - SOUND outside a session (a PROBE reply): one ACK block with base,
///   bitmap and block errors 0, the mask present and mask ID 0.
/// - DISCONNECT outside a session (a refusal): an all-zero ACK block, then ID.
/// - Session frames: the ACK block, the block map exactly when flag bit 7
///   is set, an ID block when due, then others; DISCONNECT carries the ID
///   block (ADR 0010 `control-block-order`).
///
/// Also: the header's count, flag bit 6 only on BEACON and UI, a block map
/// only as the second block of a session frame with data, Mask ID 0 on
/// unmasked frames (outside a session, CONNECT, CONNECT-ACK).
pub fn check_controls(header: &Header, controls: &[ControlBlock]) -> Result<(), FieldError> {
    use control_type as ct;
    let types: Vec<u8> = controls.iter().map(ControlBlock::control_type).collect();
    let t = header.frame_type;
    if controls.len() != header.n_control {
        return Err(FieldError::Rule("control blocks disagree with the header's count"));
    }
    if header.first_fragment && t != frame_type::BEACON && t != frame_type::UI {
        return Err(FieldError::Rule("flag bit 6 (first fragment) on a frame other than BEACON or UI"));
    }
    let unmasked = !header.session || t == frame_type::CONNECT || t == frame_type::CONNECT_ACK;
    if unmasked && header.mask_id != 0 {
        return Err(FieldError::Rule("mask ID other than 0 on an unmasked frame (ADR 0011)"));
    }
    let maps: Vec<usize> = (0..types.len()).filter(|&i| types[i] == ct::BLOCK_MAP).collect();
    if header.block_map != !maps.is_empty() || !(maps.is_empty() || maps == [1]) {
        return Err(FieldError::Rule("a block map is the second control block exactly when flag bit 7 is set"));
    }
    if header.block_map && (!header.session || header.n_data == 0) {
        return Err(FieldError::Rule("a block map belongs to a session frame with data blocks"));
    }
    let want = |expected: &[u8]| {
        if types == expected { Ok(()) } else { Err(FieldError::Rule("control blocks out of order for the frame type")) }
    };
    let dict_only = |i: usize| matches!(controls.get(i), Some(ControlBlock::Connect(c)) if c.is_dictionary_only());
    match t {
        frame_type::CONNECT | frame_type::CONNECT_ACK => {
            let Some(ControlBlock::Connect(first)) = controls.first() else {
                return Err(FieldError::Rule("CONNECT and CONNECT-ACK start with their B.3 block"));
            };
            if first.is_dictionary_only() {
                return Err(FieldError::Rule("CONNECT and CONNECT-ACK start with their B.3 block"));
            }
            if t == frame_type::CONNECT && first.requested_mcs != 0 {
                return Err(FieldError::Rule("CONNECT sends requested MCS 0 (ADR 0010)"));
            }
            if t == frame_type::CONNECT_ACK && first.resume {
                want(&[ct::CONNECT, ct::ID, ct::ACK])?;
            } else if first.more_dictionaries {
                want(&[ct::CONNECT, ct::ID, ct::CONNECT])?;
                if !dict_only(2) {
                    return Err(FieldError::Rule("the second B.3 block carries only dictionary IDs"));
                }
            } else {
                want(&[ct::CONNECT, ct::ID])?;
            }
        }
        frame_type::PROBE => {
            if header.n_data > 0 {
                return Err(FieldError::Rule("a PROBE carries no data blocks"));
            }
            match controls {
                [] if header.probe => return Err(FieldError::Rule("a BEACON-request PROBE carries no width probe")),
                [] => {}
                [ControlBlock::Connect(c)] if c.is_link_test() => {}
                _ => return Err(FieldError::Rule("a link-test PROBE carries one link-test B.3 block (ADR 0012)")),
            }
        }
        _ if header.session => {
            if types.first() != Some(&ct::ACK) {
                return Err(FieldError::Rule("a session frame leads with its ACK block (SPEC 8.4)"));
            }
            let mut i = if header.block_map { 2 } else { 1 };
            if types.get(i) == Some(&ct::ID) {
                i += 1;
            }
            if types[i.min(types.len())..].iter().any(|ty| [ct::ACK, ct::BLOCK_MAP, ct::ID, ct::CONNECT].contains(ty)) {
                return Err(FieldError::Rule("session control blocks out of order"));
            }
            if t == frame_type::DISCONNECT && !types.contains(&ct::ID) {
                return Err(FieldError::Rule("DISCONNECT carries the sender's ID block"));
            }
        }
        frame_type::SOUND => match controls {
            [ControlBlock::Ack(a)]
                if a.base_sequence == 0
                    && a.bitmap == 0
                    && a.block_errors == 0
                    && a.tone_mask.is_some()
                    && a.mask_id == 0 => {}
            _ => return Err(FieldError::Rule("a PROBE reply carries one measurement ACK block (ADR 0012)")),
        },
        frame_type::DISCONNECT => match controls {
            [ControlBlock::Ack(a), ControlBlock::Id(_)] if *a == AckBlock::default() => {}
            _ => return Err(FieldError::Rule("a refusal DISCONNECT carries an all-zero ACK block, then ID")),
        },
        _ => {}
    }
    Ok(())
}
