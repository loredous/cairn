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

/// ADR 0006: the reported SNR is the channel's, excluding clip-and-filter's
/// designed distortion, within 1 dB up to at least 40 dB.
#[test]
fn channel_snr_excludes_designed_clipping() {
    let mut rng = Rng::new(10);
    for (width, mcs, n) in [(24, 0, 4), (24, 4, 8), (56, 6, 12)] {
        let cfg = FrameConfig::new(width, mcs, n).unwrap();
        for snr in [12.0, 25.0, 35.0, 40.0, 45.0] {
            let header = rng.bits(128);
            let payloads: Vec<Vec<u8>> = (0..n).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
            let frame = Transmitter::new().build(&cfg, &header, &payloads);
            // Channel SNR as seen by the data symbols: preamble and header
            // carry only 24 tones, so at wider widths the frame average is lower.
            let data = &frame.samples[cfg.first_data() * cfg.stride()..];
            let p = data.iter().map(|v| v * v).sum::<f64>() / data.len() as f64;
            let mut x = vec![0.0; 500];
            x.extend_from_slice(&frame.samples);
            x.extend(vec![0.0; 500]);
            let y = awgn(&x, snr, width, &mut rng, Some(p));
            let r = Receiver::new().receive(&y, &cfg, 500, &RxOptions::default());
            let c = r.channel_snr.as_ref().unwrap();
            let old = {
                let t = &cfg.data_active_tones;
                10.0 * (t.iter().map(|&k| 10f64.powf(r.snr_db[k] / 10.0)).sum::<f64>() / t.len() as f64).log10()
            };
            eprintln!(
                "w{width} mcs{mcs} channel {snr:4.1} dB: rebuilt {:5.2} dB ({} known symbols), decoder estimate {old:5.2} dB",
                c.effective_db, c.known_symbols
            );
            assert!(r.block_ok.iter().all(|&v| v) || snr < 20.0);
            if c.fallback {
                // Below the rung's threshold every data block may fail; the
                // fallback then errs low but never reads more than 1 dB high.
                assert!(!r.block_ok.iter().any(|&v| v) && snr < 20.0, "w{width} mcs{mcs} at {snr} dB");
                assert!(c.effective_db <= snr + 1.0, "w{width} mcs{mcs} at {snr} dB: {}", c.effective_db);
                continue;
            }
            assert!((c.effective_db - snr).abs() < 1.0, "w{width} mcs{mcs} at {snr} dB: {}", c.effective_db);
            let snr7 = r.snr_7db_equivalent(&cfg);
            assert!((snr7 - (snr + params::clip_offset_db(mcs))).abs() < 1.0);
        }
    }
}

#[test]
fn header_only_frames() {
    // No data blocks: preamble, header (and probe) only, clipped at 7 dB (SPEC 7.1).
    let mut rng = Rng::new(9);
    for (width, mcs) in [(24, 0), (56, 6)] {
        let cfg = FrameConfig::new(width, mcs, 0).unwrap();
        assert_eq!(cfg.clip_db, params::CLIP_DB_REFERENCE);
        assert_eq!(cfg.data_symbols(), 0);
        run(&cfg, &mut rng, None);
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

/// The level settling at the start of a transmission, as the 2026-11-07
/// over-the-air captures show it: 1 - (1 - g0) exp(-t / tau) from the first
/// preamble sample, g0 -14 dB, tau 19 ms, signal and noise alike.
fn onset(n: usize, start: usize) -> Vec<f64> {
    let g0 = 10f64.powf(-14.0 / 20.0);
    (0..n).map(|i| 1.0 - (1.0 - g0) * (-(i.saturating_sub(start) as f64 / 48_000.0) / 0.019).exp()).collect()
}

/// Short frames (header only, a control block, one data block) through white
/// noise with the level settling: the clock estimate stays near its prior
/// (two pilots over a few symbols measure it to hundreds of ppm) and the
/// channel SNR reads the channel's (ADR 0006 interpretation
/// `level-settling-at-frame-start`), as in the simulator's `test_drift.py`.
#[test]
fn short_frames_clock_and_snr_with_the_level_settling() {
    let mut rng = Rng::new(11);
    let snr = 20.0;
    for (mcs, n_blocks, n_control) in [(0u8, 0usize, 0usize), (0, 0, 1), (2, 1, 1)] {
        let cfg = FrameConfig::new(24, mcs, n_blocks).unwrap().with_controls(n_control).unwrap();
        let mut got = Vec::new();
        for _ in 0..6 {
            let header = rng.bits(128);
            let controls: Vec<Vec<u8>> = (0..n_control).map(|_| rng.bits(params::CONTROL_INFO_BITS - 16)).collect();
            let payloads: Vec<Vec<u8>> = (0..n_blocks).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
            let frame = Transmitter::new().build_with_controls(&cfg, &header, &controls, &payloads);
            let last = if n_blocks > 0 {
                cfg.first_data()
            } else if n_control > 0 {
                cfg.first_control()
            } else {
                params::PREAMBLE_SYMBOLS
            };
            let tail = &frame.samples[last * cfg.stride()..];
            let p = tail.iter().map(|v| v * v).sum::<f64>() / tail.len() as f64;
            let mut x = vec![0.0; 600];
            x.extend_from_slice(&frame.samples);
            x.extend(vec![0.0; 600]);
            let noisy = awgn(&x, snr - (cfg.clip_db - params::CLIP_DB_REFERENCE), 24, &mut rng, Some(p));
            let y: Vec<f64> = noisy.iter().zip(onset(x.len(), 600)).map(|(v, g)| v * g).collect();
            let r = Receiver::new().receive(&y, &cfg, 600, &RxOptions::default());
            assert!(r.header_ok && r.block_ok.iter().all(|&b| b) && r.control_ok.iter().all(|&b| b), "{cfg:?}");
            assert!(!r.channel_snr.as_ref().unwrap().fallback, "{cfg:?}");
            let (ppm, _) = r.clock_ppm(&cfg);
            assert!(ppm.abs() < 60.0, "{cfg:?}: clock {ppm} ppm");
            got.push(r.snr_7db_equivalent(&cfg));
        }
        let mean = got.iter().sum::<f64>() / got.len() as f64;
        eprintln!("mcs {mcs} blocks {n_blocks} controls {n_control}: SNR {got:.1?} dB at {snr} dB");
        assert!((mean - snr).abs() < 1.0, "{cfg:?}: mean SNR {mean} at {snr} dB");
        assert!(got.iter().all(|&s| s <= snr + 1.5), "{cfg:?}: {got:?}");
    }
}
