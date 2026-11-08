//! The Cairn Stage 2 modem (SPEC 21.3 steps 1-6): transmit sequencing with
//! measured TXDelay and TXTail, the continuous listener, test frames and the
//! level-setting tone; the runtime that wires the link layer (`cairn-link`)
//! to the PHY with carrier detect ([`runtime`]), the KISS server and a
//! control interface ([`kiss`], [`control`], [`driver`]), and a simulator
//! that runs several complete runtimes on one audio channel ([`twosim`]).
//! The operator's side (SPEC 13.4, 13.6): the config file and rig profiles
//! ([`config`]), PTT detection ([`detect`]), the guided setup ([`setup`])
//! and the self-test ([`selftest`]) over a mockable terminal and hardware
//! ([`hw`]), and unattended operation ([`unattended`]).
//! The `cairn-modem` binary wires these to sound cards, PTT backends and WAV
//! files.

#![warn(missing_docs)]

pub mod agc;
pub mod boost;
pub mod config;
pub mod control;
pub mod detect;
pub mod driver;
pub mod frames;
pub mod headers;
pub mod hostapi;
pub mod hw;
pub mod kiss;
pub mod listen;
pub mod nprng;
pub mod phymodel;
pub mod runtime;
pub mod rxdir;
pub mod selftest;
pub mod setup;
pub mod twosim;
pub mod txseq;
pub mod unattended;
