//! Cairn physical layer (SPEC 5-7, ADRs 0003-0005): the Rust port of the
//! Stage 1 simulator's waveform, codes, bit mapping, preamble detector and
//! receiver (`sim/cairn_sim/`), checked bit for bit against `vectors/`.
//!
//! Header and control block fields (SPEC 8.2, Appendix B) are here too, so a
//! receiver can decode a frame header first without being told its
//! configuration. No audio, PTT or link layer: samples in, samples out, at
//! 48 kHz.

#![warn(missing_docs)]

pub mod channel;
pub mod crc;
pub mod detector;
pub mod dsp;
pub mod fields;
pub mod frame;
pub mod gold;
pub mod interleave;
pub mod ldpc;
pub mod mapping;
pub mod ofdm;
pub mod params;
pub mod preamble;
pub mod rx;
pub mod tx;
pub mod wav;

pub use fields::{Address, ControlBlock, FieldError, Header};
pub use frame::{ConfigError, FrameConfig, LinkState};
pub use rx::{Receiver, RxOptions, RxResult};
pub use tx::{Transmitter, TxFrame};
