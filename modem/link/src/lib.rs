//! Cairn link layer (SPEC §8-§12, ADRs 0008-0012), sans-IO.
//!
//! The link layer consumes events (decoded frames, header-only detections,
//! raw DCD edges, transmit completions, host commands, timer ticks), each
//! stamped with the caller's virtual `now`, and returns actions (transmit
//! this frame now, deliver this PDU, tell the host this, wake me at this
//! time). It never touches audio, the PHY or a clock. [`Station`] is the
//! entry point.

#![warn(missing_docs)]

pub mod access;
pub mod adapt;
pub mod adapter;
pub mod arq;
pub mod broadcast;
pub mod capacity;
pub mod frame;
pub mod linktest;
pub mod params;
pub mod pdu;
pub mod resume;
pub mod rng;
pub mod seq;
pub mod session;
pub mod station;
pub mod time;

pub use capacity::{AppendixA, FrameModel, FrameShape};
pub use frame::{
    AckBlock, Address, Block, BlockMap, ConnectBlock, ControlBlock, Flags, FrameType, IdBlock, LinkFrame, Measurement,
    RxFrame, RxHeader, TxParams,
};
pub use pdu::Pdu;
pub use seq::Seq;
pub use station::{
    Action, ChannelInfo, Config, ConnectFailure, Event, HostCommand, HostEvent, RxLink, SessionInfo, Station, WaitFor,
    WaitReason, Waiting, Warning,
};
pub use time::Instant;
