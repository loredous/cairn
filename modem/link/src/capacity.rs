//! Frame capacity and on-air time: how many data blocks fit a frame of a
//! given shape within an on-air limit (SPEC §7.3, §10.2, Appendix A; ADR
//! 0010 `frame-time-definition`).
//!
//! The link layer asks a [`FrameModel`] supplied by the caller; the PHY's
//! budget gives the real numbers. [`AppendixA`] is the simple symbol-count
//! model of Appendix A, used by the simulator and as a default.

use std::time::Duration;

use crate::params::{
    BITS_PER_POINT, BLOCK_PAYLOAD, CODED_BLOCK_BITS, CONTROL_SYMBOLS, HEADER_SYMBOLS, MAX_DATA_BLOCKS, PILOT_TONES,
    PREAMBLE_SYMBOLS, PROBE_SYMBOLS, REPEATER_ACCESS_DELAY_DEFAULT, RX_TURNAROUND, SYMBOL_LONG, SYMBOL_SHORT,
    TXDELAY_DEFAULT, TXTAIL_DEFAULT, UNMASKABLE_TONES,
};

/// The shape of a frame, as far as its length depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameShape {
    /// Data rung.
    pub mcs: u8,
    /// Transmit width in tones.
    pub width: u8,
    /// Tone mask applied to the data blocks.
    pub mask: u64,
    /// Control blocks.
    pub controls: usize,
    /// Data blocks.
    pub data: usize,
    /// Width probe present.
    pub probe: bool,
    /// Long cyclic prefix.
    pub long_prefix: bool,
}

/// Data tones of a width under a mask: the tones below `width`, less the
/// pilots, less the masked tones (pilots 3 and 20 cannot be masked; §5.4).
pub fn data_tones(width: u8, mask: u64) -> u32 {
    (0..width.min(56))
        .filter(|t| !PILOT_TONES.contains(t))
        .filter(|t| mask >> t & 1 == 1 || UNMASKABLE_TONES.contains(t))
        .count() as u32
}

/// On-air time and capacity of frames. Implemented by the PHY's budget; the
/// link layer only asks.
pub trait FrameModel {
    /// PTT-on time of a frame of this shape: TXDelay, any repeater access
    /// delay, every symbol and TXTail (ADR 0010 `frame-time-definition`).
    fn on_air_time(&self, shape: &FrameShape) -> Duration;

    /// The most data blocks (0-128) a frame of this shape (ignoring
    /// `shape.data`) can carry within `limit`.
    fn max_data_blocks(&self, shape: &FrameShape, limit: Duration) -> usize {
        let fits = |n: usize| self.on_air_time(&FrameShape { data: n, ..*shape }) <= limit;
        if !fits(0) {
            return 0;
        }
        let (mut lo, mut hi) = (0usize, MAX_DATA_BLOCKS + 1);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if fits(mid) { lo = mid } else { hi = mid }
        }
        lo
    }
}

/// The Appendix A model: symbol counts of A.1, block geometry of A.2/A.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendixA {
    /// TXDelay.
    pub txdelay: Duration,
    /// TXTail.
    pub txtail: Duration,
    /// Repeater access delay (zero in simplex).
    pub access_delay: Duration,
}

impl Default for AppendixA {
    fn default() -> Self {
        AppendixA { txdelay: TXDELAY_DEFAULT, txtail: TXTAIL_DEFAULT, access_delay: Duration::ZERO }
    }
}

impl AppendixA {
    /// Simplex with a given TXDelay.
    pub fn with_txdelay(txdelay: Duration) -> Self {
        AppendixA { txdelay, ..Default::default() }
    }

    /// Repeater mode with the default access delay.
    pub fn repeater() -> Self {
        AppendixA { access_delay: REPEATER_ACCESS_DELAY_DEFAULT, ..Default::default() }
    }

    /// Symbols of a frame of this shape.
    pub fn symbols(shape: &FrameShape) -> u32 {
        let fixed = PREAMBLE_SYMBOLS + HEADER_SYMBOLS + if shape.probe { PROBE_SYMBOLS } else { 0 };
        let control = CONTROL_SYMBOLS * shape.controls as u32;
        let data = if shape.data == 0 {
            0
        } else {
            let m = shape.mcs.min(6) as usize;
            let per_symbol = data_tones(shape.width, shape.mask).max(1) * BITS_PER_POINT[m];
            (shape.data as u32 * CODED_BLOCK_BITS[m]).div_ceil(per_symbol)
        };
        fixed + control + data
    }

    /// Time to the frame's last symbol from keying (on-air time less TXTail).
    pub fn time_to_last_symbol(&self, shape: &FrameShape) -> Duration {
        self.on_air_time(shape) - self.txtail
    }

    /// Time from keying to the first preamble sample.
    pub fn lead_in(&self) -> Duration {
        self.txdelay + self.access_delay
    }
}

impl FrameModel for AppendixA {
    fn on_air_time(&self, shape: &FrameShape) -> Duration {
        let sym = if shape.long_prefix { SYMBOL_LONG } else { SYMBOL_SHORT };
        self.txdelay + self.access_delay + sym * Self::symbols(shape) + self.txtail
    }
}

/// Expected net payload rate of one direction at a rung, as the link test
/// reports it (§9.6): full frames within `limit` with one control block,
/// plus a peer ACK cycle of turnaround and an ACK frame (Appendix A).
pub fn expected_net_kbps(model: &dyn FrameModel, mcs: u8, width: u8, mask: u64, limit: Duration) -> f64 {
    let shape = FrameShape { mcs, width, mask, controls: 1, data: 0, probe: width > 24, long_prefix: false };
    let n = model.max_data_blocks(&shape, limit);
    if n == 0 {
        return 0.0;
    }
    let frame = model.on_air_time(&FrameShape { data: n, ..shape });
    let ack = model.on_air_time(&FrameShape { mcs: 0, controls: 1, data: 0, probe: false, ..shape });
    let cycle = frame + RX_TURNAROUND + ack;
    (n * BLOCK_PAYLOAD * 8) as f64 / cycle.as_secs_f64() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ALL_TONES;

    fn shape(mcs: u8, width: u8) -> FrameShape {
        FrameShape { mcs, width, mask: ALL_TONES, controls: 1, data: 0, probe: width > 24, long_prefix: false }
    }

    #[test]
    fn appendix_a2_blocks_per_frame() {
        let m = AppendixA::default();
        let s4 = Duration::from_secs(4);
        let s2 = Duration::from_secs(2);
        // A.2, 24 tones: 3, 4 (2 s frames), 13, 20, 27, 34, 40.
        let expect24 = [3, 4, 13, 20, 27, 34, 40];
        let expect56 = [7, 10, 32, 48, 64, 80, 96];
        for mcs in 0..7u8 {
            let lim = if mcs <= 1 { s2 } else { s4 };
            assert_eq!(m.max_data_blocks(&shape(mcs, 24), lim), expect24[mcs as usize], "mcs {mcs} at 24");
            assert_eq!(m.max_data_blocks(&shape(mcs, 56), lim), expect56[mcs as usize], "mcs {mcs} at 56");
        }
    }

    #[test]
    fn ack_frame_time() {
        // A.1: an ACK frame is 259 ms on air after TXDelay (plus TXTail).
        let m = AppendixA::default();
        let t = m.on_air_time(&FrameShape { data: 0, ..shape(0, 24) });
        assert_eq!(t, Duration::from_micros(150_000 + 258_750 + 10_000));
    }

    #[test]
    fn net_rate_close_to_appendix() {
        let m = AppendixA::default();
        let r = expected_net_kbps(&m, 3, 24, ALL_TONES, Duration::from_secs(4));
        assert!((r - 4.55).abs() < 0.1, "{r}");
    }

    #[test]
    fn mask_removes_data_tones() {
        assert_eq!(data_tones(24, ALL_TONES), 22);
        assert_eq!(data_tones(24, ALL_TONES & !(1 << 5) & !(1 << 3)), 21);
    }
}
