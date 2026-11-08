//! Real-time audio for the Cairn modem (SPEC 5.1, 7.1, 13.5, 21.3 step 1).
//!
//! - Devices are chosen by a name substring ([`find_device`]), matched
//!   against the ALSA PCM id and its description, ignoring case and
//!   punctuation, never by index.
//! - Streams run at 48 kHz. Mono i16 or f32 is preferred; other channel
//!   counts work (input takes the first channel, output feeds every one).
//! - Capture runs on native ALSA where built in (the `alsa` feature), with
//!   the period and the buffer set separately (10 ms in 200 ms by default),
//!   else on cpal (two periods of buffer); playback runs on cpal.
//! - Audio threads only move samples through preallocated lock-free ring
//!   buffers (`rtrb`) and update atomics: no allocation, no locks, no I/O.
//!   They ask for real-time priority ([`rt`]: `SCHED_FIFO` directly, or
//!   through rtkit), and say what they got.
//! - Capture is gap-free in time: lost or dropped samples come back as
//!   exactly as many zeros, reported as [`AudioGap`]s, so later frames keep
//!   their timing and a frame across a gap can be flagged.
//! - Counters: backend xruns, input ring overflows and high-water mark,
//!   input timestamp gaps, output underruns during a transmission.
//! - [`OutputPort`] reports when each queued sample reaches the DAC, from
//!   the callback's playback timestamps, so the transmitter can measure its
//!   TXDelay and drain instead of guessing with sleeps.
//! - [`LevelMeter`] gives RMS, peak, DC and the clip flag (7.1 receive
//!   side); [`mixer`] sets the capture gain for receive AGC (13.5).
//! - [`MockCapture`] stands in for a sound card in tests and load runs.

#![warn(missing_docs)]

#[cfg(all(target_os = "linux", feature = "alsa"))]
mod alsa_in;
mod capture;
mod device;
mod level;
pub mod mixer;
mod mock;
pub mod rt;
mod stream;

pub use capture::{AudioGap, GapCause, InputStats, InputStream};
pub use device::{DeviceInfo, Direction, find_device, list_devices, normalize};
pub use level::{CLIP_LEVEL, LevelMeter, Levels, dbfs};
pub use mixer::{MixerControl, MockMixer};
pub use mock::MockCapture;
pub use rt::{RtConfig, RtGrant, ThreadRole};
pub use stream::{CaptureBackend, OutputPort, OutputStats, OutputStream, StreamOptions, TimingEvent, TimingTrack};

/// The waveform's sample rate (SPEC 5.1).
pub const SAMPLE_RATE: u32 = 48_000;

/// Audio errors.
#[derive(Debug)]
pub enum AudioError {
    /// No device matched, or the match was ambiguous.
    Device(String),
    /// The device cannot do 48 kHz in a format we handle.
    Config(String),
    /// The backend failed.
    Backend(String),
    /// The stream stopped (device unplugged or a fatal backend error).
    StreamDead,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Device(m) | AudioError::Config(m) | AudioError::Backend(m) => f.write_str(m),
            AudioError::StreamDead => f.write_str("the audio stream stopped (device unplugged or failed)"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<cpal::Error> for AudioError {
    fn from(e: cpal::Error) -> Self {
        AudioError::Backend(format!("{} ({})", e, e.kind()))
    }
}
