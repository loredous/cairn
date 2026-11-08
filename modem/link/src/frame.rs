//! The link layer's own view of a frame (SPEC §8, Appendix B; ADRs 0008,
//! 0010, 0011): header fields the link layer uses, control blocks as typed
//! values and data blocks as 126-byte payloads. Bit packing, LDPC and the
//! waveform belong to `cairn-phy`; [`crate::adapter`] is where the two meet.

use std::fmt;
use std::time::Duration;

use crate::params::{
    ALL_TONES, BASE_WIDTH, BLOCK_PAYLOAD, MASK_ID_MODULUS, MAX_CONTROL_BLOCKS, MAX_DATA_BLOCKS, RUNGS, SNR_MAX_DB,
    SNR_STEP_DB,
};
use crate::seq::Seq;
use crate::time::Instant;

/// One data block's payload (B.4). The PHY adds and checks the CRC-16.
pub type Block = [u8; BLOCK_PAYLOAD];

// --- Addresses (§8.3, B.1) ---------------------------------------------------------

/// A 40-bit station address (B.1): six 6-bit characters and a 4-bit SSID,
/// or a long-callsign hash, or all ones for broadcast.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(u64);

/// Errors building an [`Address`] or an [`IdBlock`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressError {
    /// More than 40 bits.
    TooWide,
    /// Empty, too long (6 characters in an address, 10 in an ID block) or a
    /// character outside the B.1 alphabet.
    BadCallsign,
    /// SSID above 15.
    BadSsid,
}

impl Address {
    /// The broadcast address (all 40 bits set).
    pub const BROADCAST: Address = Address((1 << 40) - 1);

    /// From the raw 40-bit field.
    pub fn from_raw(raw: u64) -> Result<Address, AddressError> {
        if raw >> 40 != 0 { Err(AddressError::TooWide) } else { Ok(Address(raw)) }
    }

    /// The raw 40-bit field.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// A callsign of up to six characters (A-Z, 0-9, '/', '-') and an SSID.
    /// Longer callsigns are carried as a hash (B.1), which needs SHA-256 and
    /// is left to the adapter ([`Address::from_raw`]).
    pub fn callsign(call: &str, ssid: u8) -> Result<Address, AddressError> {
        if call.is_empty() || call.len() > 6 {
            return Err(AddressError::BadCallsign);
        }
        if ssid > 15 {
            return Err(AddressError::BadSsid);
        }
        let mut v = 0u64;
        for i in 0..6 {
            let code = match call.as_bytes().get(i).map(|c| c.to_ascii_uppercase()) {
                None => 0,
                Some(c @ b'A'..=b'Z') => (c - b'A' + 1) as u64,
                Some(c @ b'0'..=b'9') => (c - b'0' + 27) as u64,
                Some(b'/') => 37,
                Some(b'-') => 38,
                Some(_) => return Err(AddressError::BadCallsign),
            };
            v = (v << 6) | code;
        }
        Ok(Address((v << 4) | ssid as u64))
    }

    /// True for the broadcast address.
    pub fn is_broadcast(self) -> bool {
        self == Address::BROADCAST
    }

    /// The SSID.
    pub fn ssid(self) -> u8 {
        (self.0 & 0xF) as u8
    }

    /// The callsign text, when it is not a hash or broadcast.
    pub fn call_text(self) -> Option<String> {
        if self.is_broadcast() {
            return None;
        }
        let mut s = String::new();
        for i in 0..6 {
            let code = ((self.0 >> (4 + 6 * (5 - i))) & 0x3F) as u8;
            match code {
                0 => break,
                1..=26 => s.push((b'A' + code - 1) as char),
                27..=36 => s.push((b'0' + code - 27) as char),
                37 => s.push('/'),
                38 => s.push('-'),
                _ => return None,
            }
        }
        Some(s)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.call_text() {
            _ if self.is_broadcast() => write!(f, "*"),
            Some(c) if self.ssid() == 0 => write!(f, "{c}"),
            Some(c) => write!(f, "{c}-{}", self.ssid()),
            None => write!(f, "#{:010x}", self.0),
        }
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({self})")
    }
}

// --- Frame types and flags (§8.2, §8.4) --------------------------------------------

/// SPEC §8.4 frame types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameType {
    /// 0, broadcast presence.
    Beacon,
    /// 1, broadcast PDUs.
    Ui,
    /// 2, session setup.
    Connect,
    /// 3, session setup answer.
    ConnectAck,
    /// 4, session data.
    Data,
    /// 5, session acknowledgment.
    Ack,
    /// 6, sounding (session) or link-test reply.
    Sound,
    /// 7, orderly close or refusal.
    Disconnect,
    /// 8, link test or BEACON request.
    Probe,
    /// 9, calibration request (v1.0).
    CalRequest,
    /// 10, reserved for polled mode (v2).
    Poll,
    /// 31, extended.
    Extended,
    /// 11-30, reserved.
    Reserved(u8),
}

impl FrameType {
    /// The 5-bit code.
    pub fn code(self) -> u8 {
        match self {
            FrameType::Beacon => 0,
            FrameType::Ui => 1,
            FrameType::Connect => 2,
            FrameType::ConnectAck => 3,
            FrameType::Data => 4,
            FrameType::Ack => 5,
            FrameType::Sound => 6,
            FrameType::Disconnect => 7,
            FrameType::Probe => 8,
            FrameType::CalRequest => 9,
            FrameType::Poll => 10,
            FrameType::Extended => 31,
            FrameType::Reserved(c) => c,
        }
    }

    /// From the 5-bit code.
    pub fn from_code(c: u8) -> FrameType {
        match c {
            0 => FrameType::Beacon,
            1 => FrameType::Ui,
            2 => FrameType::Connect,
            3 => FrameType::ConnectAck,
            4 => FrameType::Data,
            5 => FrameType::Ack,
            6 => FrameType::Sound,
            7 => FrameType::Disconnect,
            8 => FrameType::Probe,
            9 => FrameType::CalRequest,
            10 => FrameType::Poll,
            31 => FrameType::Extended,
            c => FrameType::Reserved(c & 0x1F),
        }
    }

    /// Frames a session peer must answer (ADR 0010 `reply-required-frames`).
    pub fn requires_reply_in_session(self) -> bool {
        matches!(self, FrameType::Data | FrameType::Sound | FrameType::Disconnect | FrameType::CalRequest)
    }
}

/// The header flags the link layer sets itself (§8.2, ADR 0011
/// `header-field-layout`). Bits 0 (control present), 1 (data present) and 7
/// (block map present) follow from the frame's contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// Bit 2: more fragments follow (broadcast).
    pub more_fragments: bool,
    /// Bit 3: session frame.
    pub session: bool,
    /// Bit 4: width probe follows the header.
    pub probe: bool,
    /// Bit 5: identical repeat of a broadcast frame.
    pub repeat: bool,
    /// Bit 6: first fragment (ADR 0009 `first-fragment-flag`).
    pub first_fragment: bool,
}

impl Flags {
    /// The 8-bit field, given the derived bits.
    pub fn to_bits(self, control_present: bool, data_present: bool, block_map: bool) -> u8 {
        (control_present as u8)
            | (data_present as u8) << 1
            | (self.more_fragments as u8) << 2
            | (self.session as u8) << 3
            | (self.probe as u8) << 4
            | (self.repeat as u8) << 5
            | (self.first_fragment as u8) << 6
            | (block_map as u8) << 7
    }

    /// The link-set flags of an 8-bit field.
    pub fn from_bits(b: u8) -> Flags {
        Flags {
            more_fragments: b & 1 << 2 != 0,
            session: b & 1 << 3 != 0,
            probe: b & 1 << 4 != 0,
            repeat: b & 1 << 5 != 0,
            first_fragment: b & 1 << 6 != 0,
        }
    }
}

// --- Control blocks (Appendix B) ---------------------------------------------------

/// Quantize an SNR in dB to the B.2 field (0.25 dB steps, halves up, saturating).
pub fn snr_to_field(db: f64) -> u8 {
    if db.is_nan() || db <= 0.0 {
        return 0;
    }
    (db.min(SNR_MAX_DB) / SNR_STEP_DB + 0.5).floor().min(255.0) as u8
}

/// The SNR in dB of a B.2 field value.
pub fn snr_from_field(v: u8) -> f64 {
    v as f64 * SNR_STEP_DB
}

/// ACK control block, type 0 (B.2; ADR 0008 `cumulative-ack`, ADR 0011
/// `ack-control-block-layout`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckBlock {
    /// Flag bit 0: tone mask present (always set in a session).
    pub mask_present: bool,
    /// Flag bit 2: re-probe requested.
    pub reprobe: bool,
    /// Flag bit 3: use the long prefix in this direction.
    pub long_prefix: bool,
    /// The receiver's next in-order sequence.
    pub base: Seq,
    /// Bit i set: the receiver holds block base + i (bit 0 always clear).
    pub bitmap: u128,
    /// Rung the sender should use next.
    pub requested_mcs: u8,
    /// Measured SNR, 0.25 dB steps ([`snr_to_field`]).
    pub snr: u8,
    /// Failed data blocks of the peer's last decoded frame.
    pub block_errors: u8,
    /// Usable tones 0-55, valid when `mask_present`.
    pub mask: u64,
    /// Distortion index 0-7.
    pub distortion: u8,
    /// This receiver's ADC clipped during the frame.
    pub adc_clipped: bool,
    /// ID of the mask in this block (ADR 0011 `mask-id`).
    pub mask_id: u8,
}

impl AckBlock {
    /// The all-zero block of a refusal DISCONNECT (ADR 0010 `connect-refusal`).
    pub fn zero() -> AckBlock {
        AckBlock::default()
    }
}

/// B.3 capability bits.
pub mod caps {
    /// Bit 0: widths above 24 tones.
    pub const WIDE: u16 = 1 << 0;
    /// Bit 1: reserved for HARQ, sent as zero.
    pub const HARQ: u16 = 1 << 1;
    /// Bit 2: signatures.
    pub const SIGNATURES: u16 = 1 << 2;
    /// Bit 5: level calibration.
    pub const CALIBRATION: u16 = 1 << 5;
    /// Bit 6: stream multiplexing, sent as zero until its ADR (ADR 0009
    /// `stream-interleaving-at-pdu-boundaries`).
    pub const STREAM_MUX: u16 = 1 << 6;
    /// Bit 7: dictionary fetch.
    pub const DICT_FETCH: u16 = 1 << 7;
}

/// CONNECT / CONNECT-ACK control block, type 1 (B.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectBlock {
    /// Flag bit 0: a second block with more dictionary IDs follows.
    pub more_dicts: bool,
    /// Flag bit 1: resume the session named by `session_id`.
    pub resume: bool,
    /// Session ID.
    pub session_id: u16,
    /// Capability bits ([`caps`]).
    pub capabilities: u16,
    /// Highest tone index transmitted, 23-55.
    pub width_cap: u8,
    /// Rung the sender wants to receive at (0 in CONNECT and PROBE).
    pub requested_mcs: u8,
    /// Max frame time in 0.1 s units, 5-40.
    pub max_frame_time: u8,
    /// Usable tones (CONNECT-ACK), all ones in CONNECT and PROBE.
    pub mask: u64,
    /// Up to four dictionary IDs, zero = empty.
    pub dict_ids: [u32; 4],
}

impl ConnectBlock {
    /// A second block that carries only dictionary IDs (B.3).
    pub fn dictionaries_only(ids: [u32; 4]) -> ConnectBlock {
        ConnectBlock { dict_ids: ids, ..Default::default() }
    }

    /// The advertised maximum frame time.
    pub fn max_frame_duration(&self) -> Duration {
        crate::params::FRAME_TIME_UNIT * self.max_frame_time.clamp(5, 40) as u32
    }
}

/// ID control block, type 5 (B.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdBlock {
    /// Upper-case callsign, at most 10 characters, including any indicator.
    pub callsign: String,
    /// SSID.
    pub ssid: u8,
    /// Six-character Maidenhead locator, when present.
    pub grid: Option<[u8; 6]>,
}

impl IdBlock {
    /// A validated ID block.
    pub fn new(callsign: &str, ssid: u8, grid: Option<[u8; 6]>) -> Result<IdBlock, AddressError> {
        if callsign.is_empty() || callsign.len() > 10 || !callsign.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(AddressError::BadCallsign);
        }
        if ssid > 15 {
            return Err(AddressError::BadSsid);
        }
        Ok(IdBlock { callsign: callsign.to_ascii_uppercase(), ssid, grid })
    }
}

/// Block map control block, type 7 (B.8; ADR 0008 `block-map-control-block`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockMap {
    /// Equal to the header's Sequence field.
    pub base: Seq,
    /// Bit i set: block base + i is carried in this frame.
    pub bitmap: u128,
}

impl BlockMap {
    /// The sequences the map names, in order.
    pub fn seqs(&self) -> Vec<Seq> {
        (0..128).filter(|i| self.bitmap >> i & 1 == 1).map(|i| self.base.plus(i as u16)).collect()
    }
}

/// A control block as the link layer sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlBlock {
    /// Type 0.
    Ack(AckBlock),
    /// Type 1.
    Connect(ConnectBlock),
    /// Type 5.
    Id(IdBlock),
    /// Type 7.
    BlockMap(BlockMap),
    /// Any other type, carried opaquely: types 3, 4 and 6 (CAL-REQUEST,
    /// CAL-REPORT, path), which the link layer does not act on yet
    /// (calibration is v1.0, the path block v1.x), and the reserved types 2
    /// and 8-15, which receivers skip and senders never use (Appendix B, ADR
    /// 0008 `reserved-control-types-after-block-map`).
    Opaque {
        /// Control type.
        ctype: u8,
        /// The block's 240 message bits (type field first, CRC excluded),
        /// first bit in the most significant bit of byte 0.
        message: [u8; 30],
    },
}

impl ControlBlock {
    /// The 4-bit control type.
    pub fn ctype(&self) -> u8 {
        match self {
            ControlBlock::Ack(_) => 0,
            ControlBlock::Connect(_) => 1,
            ControlBlock::Id(_) => 5,
            ControlBlock::BlockMap(_) => 7,
            ControlBlock::Opaque { ctype, .. } => *ctype,
        }
    }

    /// A reserved control type (2, 8-15).
    pub fn is_reserved(&self) -> bool {
        matches!(self.ctype(), 2 | 8..=15)
    }
}

// --- Frames ------------------------------------------------------------------------

/// Transmit parameters that are not header fields but that the PHY needs
/// to build the frame (§5.4, ADR 0011 `session-width`, `mask-id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxParams {
    /// Transmit width W in tones (the probe extent and data tones).
    pub width: u8,
    /// Tone mask applied to control and data blocks (all ones = unmasked).
    pub mask: u64,
    /// Long (2.5 ms) cyclic prefix.
    pub long_prefix: bool,
}

impl Default for TxParams {
    fn default() -> Self {
        TxParams { width: BASE_WIDTH, mask: ALL_TONES, long_prefix: false }
    }
}

/// A frame to transmit (the link layer's view of SPEC §8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkFrame {
    /// Frame type.
    pub ftype: FrameType,
    /// Source address.
    pub src: Address,
    /// Destination address.
    pub dst: Address,
    /// Link-set header flags.
    pub flags: Flags,
    /// Header Sequence field.
    pub seq: Seq,
    /// Payload MCS (0 without data blocks).
    pub mcs: u8,
    /// Header Mask ID.
    pub mask_id: u8,
    /// Control blocks, in order.
    pub controls: Vec<ControlBlock>,
    /// Data blocks, in order.
    pub data: Vec<Block>,
    /// PHY transmit parameters.
    pub tx: TxParams,
}

/// A frame layout rule broken (ADR 0008, ADR 0010 `control-block-order`,
/// `session-establishment-steps`, ADR 0012 `probe-frame-contents`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// More than three control blocks or 128 data blocks.
    TooManyBlocks,
    /// MCS above 6, nonzero without data, or a mask ID above 3.
    BadField(&'static str),
    /// Control blocks missing, extra or out of order for the frame type.
    ControlOrder(&'static str),
    /// A block map that breaks B.8.
    BadBlockMap,
    /// Data blocks in a frame type that carries none.
    UnexpectedData,
}

impl LinkFrame {
    /// Header flag bit 7: the frame carries a block map.
    pub fn has_block_map(&self) -> bool {
        matches!(self.controls.get(1), Some(ControlBlock::BlockMap(_)))
    }

    /// The full 8-bit header flag field.
    pub fn flag_bits(&self) -> u8 {
        self.flags.to_bits(!self.controls.is_empty(), !self.data.is_empty(), self.has_block_map())
    }

    /// The sequence of each data block (ADR 0008 `block-sequence-rules`).
    pub fn data_seqs(&self) -> Vec<Seq> {
        match self.controls.get(1) {
            Some(ControlBlock::BlockMap(m)) => m.seqs(),
            _ => (0..self.data.len()).map(|i| self.seq.plus(i as u16)).collect(),
        }
    }

    /// The first control block, when it is an ACK block.
    pub fn ack(&self) -> Option<&AckBlock> {
        match self.controls.first() {
            Some(ControlBlock::Ack(a)) => Some(a),
            _ => None,
        }
    }

    /// Check the frame against the layout rules a sender must follow.
    pub fn validate(&self) -> Result<(), FrameError> {
        use ControlBlock as C;
        use FrameType as T;
        if self.controls.len() > MAX_CONTROL_BLOCKS || self.data.len() > MAX_DATA_BLOCKS {
            return Err(FrameError::TooManyBlocks);
        }
        if self.mcs as usize >= RUNGS {
            return Err(FrameError::BadField("mcs"));
        }
        if self.data.is_empty() && self.mcs != 0 {
            return Err(FrameError::BadField("mcs without data"));
        }
        if self.mask_id >= MASK_ID_MODULUS || (!self.flags.session && self.mask_id != 0) {
            return Err(FrameError::BadField("mask id"));
        }
        if self.ftype == T::ConnectAck && self.mask_id != 0 {
            return Err(FrameError::BadField("mask id"));
        }
        let c = &self.controls;
        if c.iter().any(|b| b.is_reserved()) {
            return Err(FrameError::ControlOrder("reserved control type"));
        }
        let is = |i: usize, f: fn(&ControlBlock) -> bool| c.get(i).is_some_and(f);
        let ack = |b: &C| matches!(b, C::Ack(_));
        let id = |b: &C| matches!(b, C::Id(_));
        let conn = |b: &C| matches!(b, C::Connect(_));
        let map = |b: &C| matches!(b, C::BlockMap(_));
        // The block map may only be the second block.
        if c.iter().enumerate().any(|(i, b)| map(b) && i != 1) {
            return Err(FrameError::ControlOrder("block map not second"));
        }
        if let Some(C::BlockMap(m)) = c.get(1) {
            if m.base != self.seq || m.bitmap & 1 == 0 || m.bitmap.count_ones() as usize != self.data.len() {
                return Err(FrameError::BadBlockMap);
            }
        }
        let session_order = || -> Result<(), FrameError> {
            if !is(0, ack) {
                return Err(FrameError::ControlOrder("session frame must lead with ACK"));
            }
            let mut i = 1;
            if is(1, map) {
                i = 2;
            }
            // An ID block when due, then others; never ACK, map or B.3 again.
            if is(i, id) {
                i += 1;
            }
            if c[i..].iter().any(|b| ack(b) || map(b) || conn(b) || id(b)) {
                return Err(FrameError::ControlOrder("session control blocks out of order"));
            }
            Ok(())
        };
        match self.ftype {
            T::Data => {
                if !self.flags.session || self.data.is_empty() {
                    return Err(FrameError::ControlOrder("DATA is a session frame with data"));
                }
                session_order()?;
            }
            T::Ack => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                session_order()?;
            }
            T::Sound if self.flags.session => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                session_order()?;
            }
            T::Sound => {
                // Link-test reply (ADR 0012 `probe-link-test-reply`).
                if !self.data.is_empty() || c.len() != 1 || !is(0, ack) || self.seq != Seq(0) {
                    return Err(FrameError::ControlOrder("link-test SOUND carries one ACK block, Sequence 0"));
                }
            }
            T::Disconnect => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                if c.len() != 2 || !is(0, ack) || !is(1, id) {
                    return Err(FrameError::ControlOrder("DISCONNECT carries ACK then ID"));
                }
            }
            T::Connect => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                let more = matches!(c.first(), Some(C::Connect(b)) if b.more_dicts);
                if !is(0, conn) || !is(1, id) || c.len() != 2 + more as usize || (more && !is(2, conn)) {
                    return Err(FrameError::ControlOrder("CONNECT carries B.3, ID[, B.3]"));
                }
                if self.flags.session || !self.flags.probe {
                    return Err(FrameError::BadField("CONNECT flags"));
                }
            }
            T::ConnectAck => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                if !is(0, conn) || !is(1, id) || c.len() > 3 || (c.len() == 3 && !(is(2, conn) || is(2, ack))) {
                    return Err(FrameError::ControlOrder("CONNECT-ACK carries B.3, ID[, B.3 | ACK]"));
                }
                if !self.flags.session || !self.flags.probe {
                    return Err(FrameError::BadField("CONNECT-ACK flags"));
                }
            }
            T::Probe => {
                if !self.data.is_empty() {
                    return Err(FrameError::UnexpectedData);
                }
                if !(c.is_empty() || (c.len() == 1 && is(0, conn))) {
                    return Err(FrameError::ControlOrder("PROBE carries nothing or one B.3"));
                }
            }
            T::Beacon => {
                if !is(0, id) || c[1..].iter().any(|b| ack(b) || map(b) || conn(b) || id(b)) {
                    return Err(FrameError::ControlOrder("BEACON leads with its ID block"));
                }
            }
            T::Ui => {
                if c.iter().any(|b| ack(b) || map(b) || conn(b) || id(b)) || self.data.is_empty() {
                    return Err(FrameError::ControlOrder("UI carries PDUs"));
                }
            }
            T::CalRequest if self.flags.session => session_order()?,
            T::CalRequest | T::Poll | T::Extended | T::Reserved(_) => {}
        }
        Ok(())
    }
}

// --- Received frames ---------------------------------------------------------------

/// A decoded header as the link layer uses it (§8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxHeader {
    /// Frame type.
    pub ftype: FrameType,
    /// Source address.
    pub src: Address,
    /// Destination address.
    pub dst: Address,
    /// Link-set flags.
    pub flags: Flags,
    /// Flag bit 7.
    pub block_map: bool,
    /// Sequence field.
    pub seq: Seq,
    /// Payload MCS.
    pub mcs: u8,
    /// Mask ID.
    pub mask_id: u8,
}

/// What the receiver measured on a frame (§7.1, B.2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurement {
    /// 7 dB-equivalent channel SNR in dB (ADR 0006).
    pub snr_db: f64,
    /// Distortion index 0-7 (B.2).
    pub distortion: u8,
    /// This receiver's ADC clipped during the frame.
    pub adc_clipped: bool,
    /// Per-tone SNR estimates in dB, tone 0 first, for the tones measured
    /// (the base tones from the preamble, the others from a width probe).
    pub tone_snr_db: Option<Vec<f64>>,
    /// The frame carried a width probe.
    pub probe: bool,
}

/// A received frame whose header decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct RxFrame {
    /// The header.
    pub header: RxHeader,
    /// Control blocks in order; `None` for a block that failed its CRC.
    pub controls: Vec<Option<ControlBlock>>,
    /// Data blocks in order; `None` for a block that failed its CRC (or
    /// could not be demapped because the header named an unknown mask).
    pub data: Vec<Option<Block>>,
    /// Measurements.
    pub meas: Measurement,
    /// Time of the frame's last symbol.
    pub ended_at: Instant,
}

impl RxFrame {
    /// The leading ACK block, when present and decoded.
    pub fn ack(&self) -> Option<&AckBlock> {
        match self.controls.first() {
            Some(Some(ControlBlock::Ack(a))) => Some(a),
            _ => None,
        }
    }

    /// The first decoded control block of a type.
    pub fn find(&self, ctype: u8) -> Option<&ControlBlock> {
        self.controls.iter().flatten().find(|b| b.ctype() == ctype)
    }

    /// The sequence of each data block, or `None` when they cannot be known
    /// (flag bit 7 set and the block map missing, undecodable or invalid:
    /// every data block then counts as not received, ADR 0008
    /// `block-map-control-block`).
    pub fn block_seqs(&self) -> Option<Vec<Seq>> {
        let n = self.data.len();
        if !self.header.block_map {
            return Some((0..n).map(|i| self.header.seq.plus(i as u16)).collect());
        }
        match self.controls.get(1) {
            Some(Some(ControlBlock::BlockMap(m)))
                if m.base == self.header.seq && m.bitmap & 1 == 1 && m.bitmap.count_ones() as usize == n =>
            {
                Some(m.seqs())
            }
            _ => None,
        }
    }

    /// Build the receive view of a transmitted frame with every block
    /// decoded (for tests and the simulator).
    pub fn perfect(f: &LinkFrame, meas: Measurement, ended_at: Instant) -> RxFrame {
        RxFrame {
            header: RxHeader {
                ftype: f.ftype,
                src: f.src,
                dst: f.dst,
                flags: f.flags,
                block_map: f.has_block_map(),
                seq: f.seq,
                mcs: f.mcs,
                mask_id: f.mask_id,
            },
            controls: f.controls.iter().cloned().map(Some).collect(),
            data: f.data.iter().copied().map(Some).collect(),
            meas,
            ended_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Address {
        Address::callsign(s, 0).unwrap()
    }

    fn session_frame(controls: Vec<ControlBlock>, ndata: usize) -> LinkFrame {
        LinkFrame {
            ftype: if ndata > 0 { FrameType::Data } else { FrameType::Ack },
            src: call("N0AAA"),
            dst: call("N0BBB"),
            flags: Flags { session: true, ..Default::default() },
            seq: Seq(10),
            mcs: if ndata > 0 { 2 } else { 0 },
            mask_id: 0,
            controls,
            data: vec![[0u8; BLOCK_PAYLOAD]; ndata],
            tx: TxParams::default(),
        }
    }

    #[test]
    fn address_round_trip() {
        let a = Address::callsign("n0call", 7).unwrap();
        assert_eq!(a.call_text().as_deref(), Some("N0CALL"));
        assert_eq!(a.ssid(), 7);
        assert_eq!(a.to_string(), "N0CALL-7");
        assert!(Address::callsign("TOOLONG", 0).is_err());
        assert!(Address::BROADCAST.is_broadcast());
    }

    #[test]
    fn snr_field_quantization() {
        assert_eq!(snr_to_field(10.125), 41);
        assert_eq!(snr_to_field(-3.0), 0);
        assert_eq!(snr_to_field(99.0), 255);
        assert_eq!(snr_from_field(41), 10.25);
    }

    #[test]
    fn session_control_order() {
        let ack = ControlBlock::Ack(AckBlock::default());
        let id = ControlBlock::Id(IdBlock::new("N0AAA", 0, None).unwrap());
        let map = ControlBlock::BlockMap(BlockMap { base: Seq(10), bitmap: 0b1011 });
        assert!(session_frame(vec![ack.clone()], 3).validate().is_ok());
        assert!(session_frame(vec![ack.clone(), map.clone(), id.clone()], 3).validate().is_ok());
        assert!(session_frame(vec![ack.clone(), id.clone()], 0).validate().is_ok());
        assert_eq!(
            session_frame(vec![id.clone(), ack.clone()], 3).validate(),
            Err(FrameError::ControlOrder("session frame must lead with ACK"))
        );
        assert_eq!(
            session_frame(vec![ack.clone(), id.clone(), map.clone()], 3).validate(),
            Err(FrameError::ControlOrder("block map not second"))
        );
        // Map with the wrong popcount.
        assert_eq!(session_frame(vec![ack.clone(), map.clone()], 2).validate(), Err(FrameError::BadBlockMap));
        let f = session_frame(vec![ack, map], 3);
        assert_eq!(f.data_seqs(), vec![Seq(10), Seq(11), Seq(13)]);
        assert_eq!(f.flag_bits(), 0b1000_1011);
    }

    #[test]
    fn rx_block_map_rules() {
        let ack = ControlBlock::Ack(AckBlock::default());
        let map = ControlBlock::BlockMap(BlockMap { base: Seq(65535), bitmap: 0b101 });
        let f = session_frame(vec![ack, map], 2);
        let mut f = f;
        f.seq = Seq(65535);
        let mut rx = RxFrame::perfect(&f, Measurement::default(), Instant::ZERO);
        assert_eq!(rx.block_seqs(), Some(vec![Seq(65535), Seq(1)]));
        rx.controls[1] = None;
        assert_eq!(rx.block_seqs(), None);
    }
}
