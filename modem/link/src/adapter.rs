//! Where the link layer meets the PHY's field types.
//!
//! `cairn-link` keeps its own frame representation ([`crate::frame`]) so it
//! builds and tests without `cairn-phy`. Wiring the two takes one
//! implementation of [`FieldCodec`] in the crate that depends on both
//! (`cairn-modem`), converting field by field. The mapping onto the
//! `cairn_phy` API of PR #21 (branch `phy/link-fields`, ADRs 0008-0012):
//!
//! | `cairn-link` | `cairn_phy` |
//! | --- | --- |
//! | [`LinkFrame`] `ftype`, `src`, `dst`, `seq`, `mcs`, `mask_id`, [`crate::Flags`] | `fields::Header::new(type, src, dst)` + its flag fields (`first_fragment`, `block_map`, `mask_id`, ...); counts from the block lists |
//! | [`crate::frame::RxHeader`] | `fields::Header` (decoded) |
//! | [`crate::Address`] `raw()` / `from_raw` | `fields::Address` (40-bit field; long-callsign hashing stays in the PHY) |
//! | [`crate::AckBlock`] (`base`, `bitmap`, `mask_id`, ...) | `fields::AckBlock` (`base_sequence`, bitmap, `mask_id`; `AckBlock::cumulative`) |
//! | [`crate::ConnectBlock`] | `fields::ConnectBlock` (`dictionaries`, `link_test`, `is_dictionary_only`) |
//! | [`crate::IdBlock`] | `fields::IdBlock` |
//! | [`crate::BlockMap`] (`base`, `bitmap`) | `fields::BlockMapBlock` (`base_sequence`, `bitmap`); `ControlBlock::BlockMap` |
//! | `ControlBlock::Opaque` / `Unknown` | CAL-REQUEST, CAL-REPORT, path blocks / skipped reserved types |
//! | [`crate::Block`] | a data block's 126 payload bytes (the PHY appends the CRC-16) |
//! | [`crate::TxParams`] (`width`, `mask`, `long_prefix`) | `frame::LinkState` (`LinkState::session(..)`, `LinkState::probe_to(cap)`) |
//! | [`crate::Seq::diff`] | `fields::serial_diff` |
//! | [`crate::arq::Sender::select`] / [`crate::RxFrame::block_seqs`] | `fields::plan_blocks` / `fields::block_sequences` (the same B.8 rules) |
//! | [`LinkFrame::validate`] | `fields::check_controls` (control-block order per frame type) |
//! | [`crate::Station::rx_mask`] | `rx::LinkSource::link_for(&Header)` (`LinkState::mask_for`) |
//! | [`crate::pdu`] | `pdu::{Pdu, BlockPacker, BlockParser, fragment, Reassembler}`: bit-exact with `vectors/link/pdu_blocks.json` (see the tests); field names match (`pdu_type`, `dictionary_id`); here `crc: None` asks the packer to compute the CRC of a raw body |
//! | [`crate::FrameModel`] | the PHY budget (`params::blocks_per_frame`, symbol counts) |
//!
//! On receive the modem decodes the header, asks [`crate::Station::rx_mask`]
//! (through a `LinkSource` closure) for the mask its Mask ID names, decodes
//! the blocks, and hands an [`RxFrame`] with per-block results
//! (`RxResult::delivered`, `unknown_mask`) to the station. On transmit it
//! converts the [`LinkFrame`] and calls `Transmitter::build_frame(&Header,
//! &[ControlBlock], &[Vec<u8>], &LinkState)`.
//!
//! The readings settled on that branch are the ones used here: a block map
//! is accepted only as the second control block; CONNECT and CONNECT-ACK
//! decode unmasked by frame type; a nonzero Mask ID outside a session is
//! ignored on receipt and refused on send ([`LinkFrame::validate`]); the
//! resume ACK block inside CONNECT-ACK carries mask ID 0.

use crate::frame::{LinkFrame, Measurement, RxFrame, RxHeader, TxParams};
use crate::time::Instant;

/// Converts between the link layer's frames and a PHY's field types.
pub trait FieldCodec {
    /// The PHY's frame to transmit (header, control blocks, data payloads
    /// and the transmit parameters).
    type TxFrame;
    /// The PHY's decoded header.
    type Header;
    /// The PHY's receive result (header plus per-block outcomes and
    /// measurements).
    type RxResult;
    /// Conversion errors (a field out of range, a reserved type).
    type Error;

    /// A link frame to the PHY's representation.
    fn to_phy(&self, frame: &LinkFrame) -> Result<Self::TxFrame, Self::Error>;

    /// A decoded PHY header to the link layer's view.
    fn header(&self, header: &Self::Header) -> Result<RxHeader, Self::Error>;

    /// A PHY receive result to the link layer's received frame, with the
    /// measurements the modem took and the time of the last symbol.
    fn to_link(&self, rx: &Self::RxResult, meas: Measurement, ended_at: Instant) -> Result<RxFrame, Self::Error>;
}

/// The transmit parameters a frame asks of the PHY.
pub fn tx_params(frame: &LinkFrame) -> TxParams {
    frame.tx
}

#[cfg(feature = "phy")]
pub use phy::*;

#[cfg(feature = "phy")]
mod phy {
    //! The [`FieldCodec`](super::FieldCodec) for `cairn_phy` (feature `phy`).

    use cairn_phy::fields as pf;
    use cairn_phy::frame::LinkState;
    use cairn_phy::rx::RxResult;

    use super::FieldCodec;
    use crate::frame::{
        AckBlock, Address, Block, BlockMap, ConnectBlock, ControlBlock, Flags, FrameType, IdBlock, LinkFrame,
        Measurement, RxFrame, RxHeader,
    };
    use crate::params::{ALL_TONES, BLOCK_PAYLOAD};
    use crate::seq::Seq;
    use crate::time::Instant;

    /// Conversion errors.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum AdapterError {
        /// A field the PHY refused.
        Field(pf::FieldError),
        /// The receive result has no decoded header.
        NoHeader,
        /// An ID block the link layer cannot hold (grid not 4 or 6 characters).
        Id,
    }

    impl From<pf::FieldError> for AdapterError {
        fn from(e: pf::FieldError) -> Self {
            AdapterError::Field(e)
        }
    }

    /// An address to the PHY's.
    pub fn address_to_phy(a: Address) -> Result<pf::Address, AdapterError> {
        Ok(pf::Address::decode(a.raw())?)
    }

    /// A PHY address to the link layer's.
    pub fn address_from_phy(a: &pf::Address) -> Address {
        Address::from_raw(a.encode()).expect("a 40-bit field")
    }

    /// The header of a link frame.
    pub fn header_to_phy(f: &LinkFrame) -> Result<pf::Header, AdapterError> {
        let mut h = pf::Header::new(f.ftype.code(), address_to_phy(f.src)?, address_to_phy(f.dst)?);
        h.sequence = f.seq.0;
        h.mcs = f.mcs;
        h.n_control = f.controls.len();
        h.n_data = f.data.len();
        h.more_fragments = f.flags.more_fragments;
        h.session = f.flags.session;
        h.probe = f.flags.probe;
        h.repeat = f.flags.repeat;
        h.first_fragment = f.flags.first_fragment;
        h.block_map = f.has_block_map();
        h.mask_id = f.mask_id;
        Ok(h)
    }

    /// A decoded PHY header to the link layer's view.
    pub fn header_from_phy(h: &pf::Header) -> RxHeader {
        RxHeader {
            ftype: FrameType::from_code(h.frame_type),
            src: address_from_phy(&h.source),
            dst: address_from_phy(&h.destination),
            flags: Flags {
                more_fragments: h.more_fragments,
                session: h.session,
                probe: h.probe,
                repeat: h.repeat,
                first_fragment: h.first_fragment,
            },
            block_map: h.block_map,
            seq: Seq(h.sequence),
            mcs: h.mcs,
            // A nonzero Mask ID outside a session is ignored on receipt.
            mask_id: if h.session { h.mask_id } else { 0 },
        }
    }

    fn bits_to_bytes(bits: &[u8]) -> [u8; 30] {
        let mut out = [0u8; 30];
        for (i, &b) in bits.iter().take(240).enumerate() {
            out[i / 8] |= (b & 1) << (7 - i % 8);
        }
        out
    }

    fn bytes_to_bits(bytes: &[u8; 30]) -> Vec<u8> {
        (0..240).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
    }

    /// A control block to the PHY's.
    pub fn control_to_phy(c: &ControlBlock) -> Result<pf::ControlBlock, AdapterError> {
        Ok(match c {
            ControlBlock::Ack(a) => pf::ControlBlock::Ack(pf::AckBlock {
                base_sequence: a.base.0,
                bitmap: a.bitmap,
                requested_mcs: a.requested_mcs,
                snr_q: a.snr,
                block_errors: a.block_errors,
                tone_mask: a.mask_present.then_some(a.mask),
                reprobe: a.reprobe,
                long_prefix: a.long_prefix,
                distortion: a.distortion,
                adc_clipped: a.adc_clipped,
                mask_id: a.mask_id,
            }),
            ControlBlock::Connect(b) => pf::ControlBlock::Connect(pf::ConnectBlock {
                session_id: b.session_id,
                capabilities: b.capabilities,
                width_cap: b.width_cap,
                requested_mcs: b.requested_mcs,
                max_frame_time: b.max_frame_time,
                tone_mask: b.mask,
                dictionary_ids: b.dict_ids,
                more_dictionaries: b.more_dicts,
                resume: b.resume,
            }),
            ControlBlock::Id(i) => pf::ControlBlock::Id(pf::IdBlock {
                callsign: i.callsign.clone(),
                ssid: i.ssid,
                grid: i.grid.map(|g| String::from_utf8_lossy(&g).trim_end().to_string()),
            }),
            ControlBlock::BlockMap(m) => {
                pf::ControlBlock::BlockMap(pf::BlockMapBlock { base_sequence: m.base.0, bitmap: m.bitmap })
            }
            ControlBlock::Opaque { message, .. } => {
                pf::ControlBlock::unpack(&cairn_phy::crc::append_crc(&bytes_to_bits(message)))?
            }
        })
    }

    /// A PHY control block to the link layer's.
    pub fn control_from_phy(c: &pf::ControlBlock) -> Result<ControlBlock, AdapterError> {
        Ok(match c {
            pf::ControlBlock::Ack(a) => ControlBlock::Ack(AckBlock {
                mask_present: a.tone_mask.is_some(),
                reprobe: a.reprobe,
                long_prefix: a.long_prefix,
                base: Seq(a.base_sequence),
                bitmap: a.bitmap,
                requested_mcs: a.requested_mcs,
                snr: a.snr_q,
                block_errors: a.block_errors,
                mask: a.tone_mask.unwrap_or(0),
                distortion: a.distortion,
                adc_clipped: a.adc_clipped,
                mask_id: a.mask_id,
            }),
            pf::ControlBlock::Connect(b) => ControlBlock::Connect(ConnectBlock {
                more_dicts: b.more_dictionaries,
                resume: b.resume,
                session_id: b.session_id,
                capabilities: b.capabilities,
                width_cap: b.width_cap,
                requested_mcs: b.requested_mcs,
                max_frame_time: b.max_frame_time,
                mask: b.tone_mask,
                dict_ids: b.dictionary_ids,
            }),
            pf::ControlBlock::Id(i) => {
                let grid = match &i.grid {
                    None => None,
                    Some(g) if g.len() == 4 || g.len() == 6 => {
                        let mut a = [b' '; 6];
                        a[..g.len()].copy_from_slice(g.as_bytes());
                        Some(a)
                    }
                    Some(_) => return Err(AdapterError::Id),
                };
                ControlBlock::Id(IdBlock { callsign: i.callsign.clone(), ssid: i.ssid, grid })
            }
            pf::ControlBlock::BlockMap(m) => {
                ControlBlock::BlockMap(BlockMap { base: Seq(m.base_sequence), bitmap: m.bitmap })
            }
            other => {
                ControlBlock::Opaque { ctype: other.control_type(), message: bits_to_bytes(&other.message_bits()?) }
            }
        })
    }

    /// The PHY link state a frame is transmitted with: its width, the mask
    /// it applies (with its ID), its prefix, and a probe to its width.
    pub fn tx_link_state(f: &LinkFrame) -> LinkState {
        let width = f.tx.width as usize;
        let in_width = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 } & ALL_TONES;
        let usable = (f.tx.mask & in_width != in_width).then(|| pf::mask_to_tones(f.tx.mask & in_width));
        LinkState {
            width,
            usable,
            cp: Some(if f.tx.long_prefix { cairn_phy::params::CP_LONG } else { cairn_phy::params::CP_SHORT }),
            cap: Some(width),
            mask_id: f.mask_id,
            pending: None,
        }
    }

    /// A frame ready for `cairn_phy::tx::Transmitter::build_frame`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct PhyTxFrame {
        /// The header.
        pub header: pf::Header,
        /// The control blocks.
        pub controls: Vec<pf::ControlBlock>,
        /// The data payloads as information bits (1,008 each, most significant
        /// bit first), as `Transmitter::build_frame` takes them.
        pub data: Vec<Vec<u8>>,
        /// The link state.
        pub link: LinkState,
    }

    /// The `cairn_phy` codec.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PhyCodec;

    impl FieldCodec for PhyCodec {
        type TxFrame = PhyTxFrame;
        type Header = pf::Header;
        type RxResult = RxResult;
        type Error = AdapterError;

        fn to_phy(&self, f: &LinkFrame) -> Result<PhyTxFrame, AdapterError> {
            let header = header_to_phy(f)?;
            let controls = f.controls.iter().map(control_to_phy).collect::<Result<Vec<_>, _>>()?;
            pf::check_controls(&header, &controls)?;
            let data = f.data.iter().map(cairn_phy::pdu::block_bits).collect();
            Ok(PhyTxFrame { header, controls, data, link: tx_link_state(f) })
        }

        fn header(&self, h: &pf::Header) -> Result<RxHeader, AdapterError> {
            Ok(header_from_phy(h))
        }

        fn to_link(&self, rx: &RxResult, meas: Measurement, ended_at: Instant) -> Result<RxFrame, AdapterError> {
            let h = rx.header.as_ref().ok_or(AdapterError::NoHeader)?;
            let masked_out = rx.unknown_mask.is_some();
            let controls = rx
                .controls
                .iter()
                .map(|c| if masked_out { None } else { c.as_ref().and_then(|c| control_from_phy(c).ok()) })
                .collect();
            let data = rx
                .block_ok
                .iter()
                .zip(&rx.block_bits)
                .map(|(&ok, bits)| {
                    (ok && !masked_out).then(|| {
                        let p = cairn_phy::pdu::bits_to_payload(bits);
                        let mut b: Block = [0; BLOCK_PAYLOAD];
                        b.copy_from_slice(&p[..BLOCK_PAYLOAD]);
                        b
                    })
                })
                .collect();
            Ok(RxFrame { header: header_from_phy(h), controls, data, meas, ended_at })
        }
    }
}
