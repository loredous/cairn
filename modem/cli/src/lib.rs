//! The Cairn Stage 2 modem's real-time layer (SPEC 21.3 steps 1-4, without
//! the link layer): transmit sequencing with measured TXDelay and TXTail,
//! the continuous listener, test frames and the level-setting tone. The
//! `cairn-modem` binary wires these to sound cards, PTT backends and WAV
//! files.

#![warn(missing_docs)]

pub mod boost;
pub mod frames;
pub mod headers;
pub mod listen;
pub mod nprng;
pub mod txseq;
