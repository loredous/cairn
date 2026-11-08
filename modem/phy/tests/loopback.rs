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

/// A 32-tone frame with its width probe, tones 0-2 swamped by interferers
/// (0 dB there): the rebuilt per-tone SNR is finite on every tone, measured
/// on every probed tone above 23 (even in a header-only frame, where only
/// the two probe symbols cover them), and tones 3-4 next to the bad ones read
/// their own SNR (mask decisions use these values). As the simulator's
/// `test_per_tone_snr_on_probed_tones_next_to_bad_tones`.
#[test]
fn per_tone_snr_on_probed_tones_next_to_bad_tones() {
    let snr = 20.0;
    for n_blocks in [4usize, 0] {
        let mut rng = Rng::new(77);
        let cfg = FrameConfig::new(32, 0, n_blocks).unwrap().with_probe(true).unwrap();
        let header = rng.bits(128);
        let payloads: Vec<Vec<u8>> = (0..n_blocks).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
        let frame = Transmitter::new().build(&cfg, &header, &payloads);
        let last = if n_blocks > 0 { cfg.first_data() } else { cfg.first_probe() };
        let tail = &frame.samples[last * cfg.stride()..];
        let p = tail.iter().map(|v| v * v).sum::<f64>() / tail.len() as f64;
        let mut x = vec![0.0; 600];
        x.extend_from_slice(&frame.samples);
        x.extend(vec![0.0; 600]);
        let mut y = awgn(&x, snr, cfg.width, &mut rng, Some(p));
        for k in 0..3 {
            let phase = 2.0 * std::f64::consts::PI * rng.uniform();
            let a = (2.0 * p / cfg.width as f64).sqrt();
            for (i, v) in y.iter_mut().enumerate() {
                *v += a * (2.0 * std::f64::consts::PI * params::tone_freq(k) * i as f64 / 48_000.0 + phase).cos();
            }
        }
        let r = Receiver::new().receive(&y, &cfg, 600, &RxOptions::default());
        assert!(r.header_ok && r.block_ok.iter().all(|&b| b), "{n_blocks} blocks");
        let c = r.channel_snr.as_ref().unwrap();
        assert!(!c.fallback);
        let per = &c.per_tone_db; // average-referenced; MCS 0 has no clip offset
        eprintln!("{n_blocks} blocks: per-tone {per:.1?}");
        assert_eq!(per.len(), 32);
        assert!(per.iter().all(|v| v.is_finite()), "{per:?}");
        assert!(per[..3].iter().all(|&v| v < 6.0), "{:?}", &per[..3]);
        // A header-only frame measures each base tone on 9 symbols: +-1.7 dB
        // (the simulator over 30 seeds: tones 3-4 average 19.3 dB, unbiased).
        let near = if n_blocks > 0 { 2.5 } else { 5.0 };
        assert!(per[3..5].iter().all(|&v| (v - snr).abs() < near), "{:?}", &per[3..5]);
        let tol = if n_blocks > 0 { 3.0 } else { 6.0 };
        assert!(per[24..].iter().all(|&v| (v - snr).abs() < tol), "{:?}", &per[24..]);
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

/// The capture-like ramp of `sim/scripts/level_eval.py`: g0 -30 dB, tau 40 ms
/// from the first preamble sample, which leaves the S2 pair 4.9 dB low (the
/// 2026-11-07 captures: 3.3-6.3 dB).
fn capture_ramp(n: usize, start: usize) -> Vec<f64> {
    let g0 = 10f64.powf(-30.0 / 20.0);
    (0..n).map(|i| 1.0 - (1.0 - g0) * (-(i.saturating_sub(start) as f64 / 48_000.0) / 0.040).exp()).collect()
}

/// 64-QAM frames (MCS 4 and 5, 24 and 32 tones) 2 dB above their 1%
/// threshold, through white noise with the capture-like ramp on signal and
/// noise: the S2 estimate is 4.9 dB low, so without level tracking nothing
/// decodes; with it every block does, and the tracked data level is the
/// ramp's. Without a ramp, tracking changes no block (the simulator's
/// `test_level.py`).
#[test]
fn level_ramp_64qam() {
    let mut rng = Rng::new(21);
    let guard = 400;
    for (mcs, width, snr) in [(4u8, 24usize, 18.0), (5, 24, 21.0), (4, 32, 18.0)] {
        let cfg = FrameConfig::new(width, mcs, 4).unwrap().with_probe(width > 24).unwrap();
        for trial in 0..2 {
            let header = rng.bits(128);
            let payloads: Vec<Vec<u8>> = (0..4).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
            let frame = Transmitter::new().build(&cfg, &header, &payloads);
            let mut x = vec![0.0; guard];
            x.extend(frame.output());
            x.extend(vec![0.0; guard]);
            let p = frame.output().iter().map(|v| v * v).sum::<f64>() / frame.samples.len() as f64;
            let noisy = awgn(&x, snr, width, &mut rng, Some(p));
            let ramped: Vec<f64> = noisy.iter().zip(capture_ramp(x.len(), guard)).map(|(v, g)| v * g).collect();
            let off = RxOptions { level_tracking: false, header_channel: false, ..Default::default() };
            let good = |r: &cairn_phy::RxResult| {
                r.block_ok
                    .iter()
                    .zip(&r.block_bits)
                    .zip(&frame.block_bits)
                    .filter(|((ok, b), t)| **ok && b == t)
                    .count()
            };
            let mut rx = Receiver::new();
            let on = rx.receive(&ramped, &cfg, guard, &RxOptions::default());
            assert_eq!(good(&on), 4, "mcs {mcs} x{width} trial {trial}: tracked");
            assert_eq!(good(&rx.receive(&ramped, &cfg, guard, &off)), 0, "mcs {mcs} x{width} trial {trial}");
            assert!(on.level.applied && on.level.settle < 0.0);
            assert!((on.level.data_db() - 4.9).abs() < 0.6, "level {} dB", on.level.data_db());
            // No ramp: the same blocks with and without tracking.
            let plain = rx.receive(&noisy, &cfg, guard, &RxOptions::default());
            assert_eq!(good(&plain), good(&rx.receive(&noisy, &cfg, guard, &off)), "mcs {mcs} x{width}");
            assert_eq!(good(&plain), 4);
        }
    }
}

/// The base tones' channel re-estimated from the decoded header (nine known
/// symbols per tone instead of S2's two): near the MCS 2 threshold, with and
/// without the capture-like ramp, at 24 and 32 tones, more blocks decode on
/// the same frames than with the S2 estimate alone; a frame whose header
/// fails is decoded as before (the simulator's `test_header_channel.py`).
#[test]
fn header_channel_estimate() {
    let mut rng = Rng::new(31);
    let guard = 400;
    let s2_only = RxOptions { header_channel: false, ..Default::default() };
    for (width, ramp) in [(24usize, false), (24, true), (32, false)] {
        let cfg = FrameConfig::new(width, 2, 8).unwrap().with_probe(width > 24).unwrap();
        let (mut on, mut off) = (0, 0);
        for _ in 0..6 {
            let header = rng.bits(128);
            let payloads: Vec<Vec<u8>> = (0..8).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
            let frame = Transmitter::new().build(&cfg, &header, &payloads);
            let mut x = vec![0.0; guard];
            x.extend(frame.output());
            x.extend(vec![0.0; guard]);
            let p = frame.output().iter().map(|v| v * v).sum::<f64>() / frame.samples.len() as f64;
            let mut y = awgn(&x, 7.0, width, &mut rng, Some(p));
            if ramp {
                y = y.iter().zip(capture_ramp(x.len(), guard)).map(|(v, g)| v * g).collect();
            }
            let good = |r: &cairn_phy::RxResult| {
                r.block_ok
                    .iter()
                    .zip(&r.block_bits)
                    .zip(&frame.block_bits)
                    .filter(|((ok, b), t)| **ok && b == t)
                    .count()
            };
            let mut rx = Receiver::new();
            on += good(&rx.receive(&y, &cfg, guard, &RxOptions::default()));
            off += good(&rx.receive(&y, &cfg, guard, &s2_only));
        }
        assert!(on > off, "x{width} ramp {ramp}: {on} vs {off} of 48 blocks");
    }
    // Below the header's threshold nothing is known to re-estimate from.
    let cfg = FrameConfig::new(24, 0, 1).unwrap();
    let frame = Transmitter::new().build(&cfg, &rng.bits(128), &[rng.bits(params::DATA_PAYLOAD_BITS)]);
    let mut x = vec![0.0; guard];
    x.extend(frame.output());
    x.extend(vec![0.0; guard]);
    let p = frame.output().iter().map(|v| v * v).sum::<f64>() / frame.samples.len() as f64;
    let mut failed = 0;
    for _ in 0..6 {
        let y = awgn(&x, -6.0, 24, &mut rng, Some(p));
        let mut rx = Receiver::new();
        let a = rx.receive(&y, &cfg, guard, &RxOptions::default());
        if !a.header_ok {
            let b = rx.receive(&y, &cfg, guard, &s2_only);
            assert_eq!((a.snr_db, a.block_ok, a.level), (b.snr_db, b.block_ok, b.level));
            failed += 1;
        }
    }
    assert!(failed > 0);
}
