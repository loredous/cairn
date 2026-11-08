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

use crate::frame::{LinkFrame, RxFrame, RxHeader, TxParams};

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

    /// A PHY receive result to the link layer's received frame.
    fn to_link(&self, rx: &Self::RxResult) -> Result<RxFrame, Self::Error>;
}

/// The transmit parameters a frame asks of the PHY.
pub fn tx_params(frame: &LinkFrame) -> TxParams {
    frame.tx
}
