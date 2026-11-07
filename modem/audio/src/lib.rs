//! Real-time audio for the Cairn modem (SPEC 5.1, 7.1, 21.3 step 1).
//!
//! - Devices are chosen by a name substring ([`find_device`]), matched
//!   against the ALSA PCM id and its description, ignoring case and
//!   punctuation, never by index.
//! - Streams run at 48 kHz. Mono i16 or f32 is preferred; other channel
//!   counts work (input takes the first channel, output feeds every one).
//! - Callbacks only move samples through preallocated lock-free ring
//!   buffers (`rtrb`) and update atomics: no allocation, no locks, no I/O.
//! - Counters: backend xruns, input ring overflows, input timestamp gaps,
//!   output underruns during a transmission.
//! - [`OutputPort`] reports when each queued sample reaches the DAC, from
//!   the callback's playback timestamps, so the transmitter can measure its
//!   TXDelay and drain instead of guessing with sleeps.
//! - [`LevelMeter`] gives RMS, peak, DC and the clip flag (7.1 receive side).

#![warn(missing_docs)]

mod device;
mod level;
mod stream;

pub use device::{DeviceInfo, Direction, find_device, list_devices, normalize};
pub use level::{CLIP_LEVEL, LevelMeter, Levels, dbfs};
pub use stream::{
    InputStats, InputStream, OutputPort, OutputStats, OutputStream, StreamOptions, TimingEvent, TimingTrack,
};

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
