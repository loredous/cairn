//! The link layer's [`FrameModel`] from the PHY's own frame geometry:
//! a frame's on-air time is TXDelay, any repeater access delay, the samples
//! `cairn_phy::FrameConfig` gives for its shape, and TXTail (SPEC 7.3,
//! ADR 0010 `frame-time-definition`). It agrees with the Appendix A symbol
//! counts (`cairn_link::AppendixA`) on every rung and width with up to two
//! control blocks (tested below; three control blocks share a run one
//! symbol shorter than A.1's 3 x 12), but it is the transmitter's real
//! geometry, so a change in the PHY's
//! layout reaches the link layer's frame filling.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use cairn_link::{FrameModel, FrameShape};
use cairn_phy::FrameConfig;
use cairn_phy::params::{CP_LONG, CP_SHORT, FS};

/// A frame shape as a hash key.
type ShapeKey = (u8, u8, u64, usize, usize, bool, bool);

/// Frame on-air times from `cairn_phy` geometry.
#[derive(Debug)]
pub struct PhyFrameModel {
    /// Key to first preamble sample.
    pub txdelay: Duration,
    /// Last sample to release.
    pub txtail: Duration,
    /// Repeater access delay (zero in simplex).
    pub access_delay: Duration,
    cache: RefCell<HashMap<ShapeKey, Duration>>,
}

impl PhyFrameModel {
    /// A model with these timings.
    pub fn new(txdelay: Duration, txtail: Duration, access_delay: Duration) -> Self {
        PhyFrameModel { txdelay, txtail, access_delay, cache: RefCell::new(HashMap::new()) }
    }

    /// Samples of a frame of this shape, or `None` when the PHY cannot build it.
    pub fn samples(shape: &FrameShape) -> Option<usize> {
        let mcs = if shape.data == 0 { 0 } else { shape.mcs.min(6) };
        let width = shape.width.clamp(24, 56) as usize;
        let usable = mask_tones(shape.mask, width);
        let cfg = FrameConfig::new(width, mcs, shape.data)
            .and_then(|c| c.with_controls(shape.controls.min(3)))
            .and_then(|c| c.with_cp(if shape.long_prefix { CP_LONG } else { CP_SHORT }))
            .and_then(|c| c.with_mask(usable))
            .ok()?;
        // A wide frame always probes (ADR 0007); `shape.probe` asks for it otherwise.
        let probe = shape.probe || cfg.needs_probe();
        let cfg = cfg.with_probe(probe).ok()?;
        Some(cfg.n_samples())
    }
}

/// The usable tones of a mask below `width`, `None` when every tone is usable.
pub fn mask_tones(mask: u64, width: usize) -> Option<Vec<usize>> {
    let in_width = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
    if mask & in_width == in_width { None } else { Some((0..width).filter(|&t| mask >> t & 1 == 1).collect()) }
}

impl FrameModel for PhyFrameModel {
    fn on_air_time(&self, shape: &FrameShape) -> Duration {
        let key = (shape.mcs, shape.width, shape.mask, shape.controls, shape.data, shape.probe, shape.long_prefix);
        if let Some(d) = self.cache.borrow().get(&key) {
            return *d;
        }
        // A shape the PHY refuses (more than 128 blocks) never fits.
        let d = match Self::samples(shape) {
            Some(n) => {
                self.txdelay
                    + self.access_delay
                    + Duration::from_micros((n as u64 * 1_000_000).div_ceil(FS as u64))
                    + self.txtail
            }
            None => Duration::from_secs(3600),
        };
        let mut c = self.cache.borrow_mut();
        if c.len() > 4096 {
            c.clear();
        }
        c.insert(key, d);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_link::AppendixA;
    use cairn_link::params::ALL_TONES;

    fn shape(mcs: u8, width: u8, controls: usize) -> FrameShape {
        FrameShape { mcs, width, mask: ALL_TONES, controls, data: 0, probe: width > 24, long_prefix: false }
    }

    #[test]
    fn agrees_with_appendix_a() {
        let phy = PhyFrameModel::new(Duration::from_millis(150), Duration::from_millis(10), Duration::ZERO);
        let a = AppendixA::default();
        for width in [24u8, 30, 40, 56] {
            for mcs in 0..7u8 {
                for controls in 0..=3 {
                    for lim in [Duration::from_secs(2), Duration::from_secs(4)] {
                        let s = shape(mcs, width, controls);
                        let (p, q) = (phy.max_data_blocks(&s, lim), a.max_data_blocks(&s, lim));
                        if controls < 3 {
                            assert_eq!(p, q, "MCS {mcs} width {width} controls {controls} limit {lim:?}");
                        } else {
                            // The PHY packs control blocks into one run: three
                            // take 35 symbols at 24 tones, not A.1's 3 x 12.
                            assert!(p == q || p == q + 1, "MCS {mcs} width {width}: {p} vs {q}");
                        }
                    }
                }
            }
        }
        // A.2 at 24 tones, one control block.
        let expect = [3, 4, 13, 20, 27, 34, 40];
        for mcs in 0..7u8 {
            let lim = Duration::from_secs(if mcs <= 1 { 2 } else { 4 });
            assert_eq!(phy.max_data_blocks(&shape(mcs, 24, 1), lim), expect[mcs as usize]);
        }
    }

    #[test]
    fn on_air_time_of_an_ack_frame() {
        let phy = PhyFrameModel::new(Duration::from_millis(150), Duration::from_millis(10), Duration::ZERO);
        let t = phy.on_air_time(&shape(0, 24, 1));
        assert_eq!(t, Duration::from_micros(150_000 + 258_750 + 10_000));
    }
}
