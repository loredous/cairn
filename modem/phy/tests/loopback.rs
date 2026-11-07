//! Rust transmitter to Rust receiver: every rung at several widths, both
//! prefixes, with and without a mask, through an ideal channel (SPEC 21.2
//! `ofdm` acceptance), found by the detector; and a little noise.

use cairn_phy::channel::{Rng, awgn};
use cairn_phy::detector::{DetectorConfig, detect};
use cairn_phy::{FrameConfig, Receiver, RxOptions, Transmitter, params};

fn run(cfg: &FrameConfig, rng: &mut Rng, snr_db: Option<f64>) {
    let header = rng.bits(128);
    let payloads: Vec<Vec<u8>> = (0..cfg.n_blocks).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
    let frame = Transmitter::new().build(cfg, &header, &payloads);
    let guard = 700;
    let mut x = vec![0.0; guard];
    x.extend(frame.output());
    x.extend(vec![0.0; guard]);
    if let Some(snr) = snr_db {
        let p = frame.output().iter().map(|v| v * v).sum::<f64>() / frame.samples.len() as f64;
        x = awgn(&x, snr, cfg.width, rng, Some(p));
    }
    let dets = detect(&x, &DetectorConfig::default());
    assert_eq!(dets.len(), 1, "{cfg:?}");
    assert_eq!(dets[0].cp, cfg.cp);
    assert!((dets[0].start as i64 - guard as i64).abs() <= 1, "start {}", dets[0].start);
    let r = Receiver::new().receive(&x, cfg, dets[0].start, &RxOptions::default());
    assert!(r.header_ok);
    assert_eq!(r.header_bits, frame.header_bits);
    for (i, (ok, bits)) in r.block_ok.iter().zip(&r.block_bits).enumerate() {
        assert!(*ok && *bits == frame.block_bits[i], "block {i} of {cfg:?}");
    }
}

#[test]
fn every_rung_and_width_ideal_channel() {
    let mut rng = Rng::new(7);
    for mcs in 0..7u8 {
        for (width, cp, usable) in [
            (24, 60, None),
            (24, 120, Some((3..56).collect::<Vec<_>>())),
            (30, 60, Some((0..56).filter(|&k| k != 9 && k != 25).collect())),
            (41, 120, None),
            (56, 60, None),
            (56, 120, Some((0..56).filter(|k| ![0, 1, 2, 37, 45, 46].contains(k)).collect())),
        ] {
            let cfg = FrameConfig::new(width, mcs, 3).unwrap().with_cp(cp).unwrap().with_mask(usable).unwrap();
            run(&cfg, &mut rng, None);
        }
    }
}

#[test]
fn awgn_above_threshold() {
    let mut rng = Rng::new(8);
    for mcs in 0..7u8 {
        let snr = params::RUNGS[mcs as usize].threshold_db + 4.0 - params::clip_offset_db(mcs);
        let cfg = FrameConfig::new(24, mcs, 4).unwrap();
        run(&cfg, &mut rng, Some(snr));
    }
}
