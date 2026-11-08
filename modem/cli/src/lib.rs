//! The Cairn Stage 2 modem (SPEC 21.3 steps 1-6): transmit sequencing with
//! measured TXDelay and TXTail, the continuous listener, test frames and the
//! level-setting tone; the runtime that wires the link layer (`cairn-link`)
//! to the PHY with carrier detect ([`runtime`]), the KISS server and a
//! control interface ([`kiss`], [`control`], [`driver`]), and a simulator
//! that runs several complete runtimes on one audio channel ([`twosim`]).
//! The `cairn-modem` binary wires these to sound cards, PTT backends and WAV
//! files.

#![warn(missing_docs)]

pub mod agc;
pub mod boost;
pub mod control;
pub mod driver;
pub mod frames;
pub mod headers;
pub mod hostapi;
pub mod kiss;
pub mod listen;
pub mod nprng;
pub mod phymodel;
pub mod runtime;
pub mod rxdir;
pub mod twosim;
pub mod txseq;
