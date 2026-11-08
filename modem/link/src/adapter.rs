//! Where the link layer meets the PHY's field types.
//!
//! `cairn-link` keeps its own frame representation ([`crate::frame`]) so it
//! builds and tests without `cairn-phy`. Wiring the two takes one
//! implementation of [`FieldCodec`] in the crate that depends on both
//! (`cairn-modem`), converting field by field:
//!
//! | `cairn-link` | `cairn-phy::fields` (SPEC §8.2, Appendix B) |
//! | --- | --- |
//! | [`LinkFrame`] header fields + [`LinkFrame::flag_bits`] | `Header` (version 1, counts derived from the blocks) |
//! | [`crate::frame::AckBlock`] | `AckBlock` (cumulative base, bitmap, mask ID; ADR 0008/0011) |
//! | [`crate::frame::ConnectBlock`] | `ConnectBlock` (B.3) |
//! | [`crate::frame::IdBlock`] | `IdBlock` (B.6) |
//! | [`crate::frame::BlockMap`] | the control-type-7 block (B.8) |
//! | `ControlBlock::Opaque` | CAL-REQUEST, CAL-REPORT, path blocks |
//! | [`crate::frame::Block`] | a data block's 126 payload bytes (the PHY appends the CRC-16) |
//! | [`TxParams`] | `LinkState` / `FrameConfig` (width, mask, prefix) |
//! | [`crate::capacity::FrameModel`] | the PHY budget (`blocks_per_frame`, symbol counts) |
//!
//! On receive the modem decodes the header, asks
//! [`crate::Station::rx_mask`] for the mask its Mask ID names, decodes the
//! blocks, and hands an [`RxFrame`] with per-block results to the station.

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
