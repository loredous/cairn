//! Real frames received over the air (`fixtures/offair/`, see its README):
//! two stations, K0JLB-1 and K0JLB-2, each through an AIOC into a handheld,
//! 2026-11-07. Each frame type decodes; the clock-offset estimate of a short
//! frame stays consistent with the direction's 4 s frame (before the fix a
//! header-only frame read -115 and -206 ppm, a one-block frame -505 ppm); and
//! the channel SNR of a short frame reads within 1.5 dB of the 4 s frame on
//! the same path, never more than 1 dB above it (before: up to 7 dB low).

mod common;

use std::path::PathBuf;

use cairn_phy::rx::CLOCK_BOUND_PPM;
use cairn_phy::{ClockPrior, LinkState, Receiver, RxOptions, RxResult, wav};
use common::*;
use serde_json::Value;

/// Short frames' SNR against the 4 s frame of the same path, dB.
const SNR_TOLERANCE_DB: f64 = 1.5;
/// ... of which at most this much above it (ADR 0006 never over-reports by more).
const SNR_OVER_DB: f64 = 1.0;
/// A decodable short frame's clock offset against the 4 s frame's, ppm, with the default prior.
const CLOCK_TOLERANCE_PPM: f64 = 30.0;
/// ... and with the 4 s frame's estimate as the peer's prior (sd [`PRIOR_SD_PPM`]).
const PRIOR_CLOCK_TOLERANCE_PPM: f64 = 5.0;
const PRIOR_SD_PPM: f64 = 3.0;

fn dir() -> PathBuf {
    modem_dir().join("fixtures").join("offair")
}

/// The one frame in a fixture WAV, detected and decoded header first as a
/// station in a 24-tone session would.
fn decode(name: &str, opts: &RxOptions) -> RxResult {
    let x = wav::read(&dir().join(name)).unwrap();
    let link = LinkState::from_cap(24, None);
    Receiver::new().detect_and_receive(&x, &link, opts).unwrap_or_else(|| panic!("{name}: no frame detected"))
}

fn clock_ppm(r: &RxResult) -> (f64, f64) {
    r.clock_ppm(r.config.as_ref().expect("decoded frame"))
}

fn snr(r: &RxResult) -> f64 {
    r.snr_7db_equivalent(r.config.as_ref().unwrap()).min(63.75)
}

fn check_content(name: &str, f: &Value, r: &RxResult, data_decodable: bool) {
    let header = r.header.as_ref().unwrap_or_else(|| panic!("{name}: header"));
    let want = hex_to_bits(f["header_message_hex"].as_str().unwrap(), 128);
    assert_eq!(header.message_bits().unwrap(), want, "{name}: header fields");
    let controls: Vec<&str> = f["control_hex"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(r.control_ok.len(), controls.len(), "{name}");
    for (i, c) in controls.iter().enumerate() {
        assert!(r.control_ok[i], "{name}: control block {i}");
        assert_eq!(r.control_bits[i][..240], hex_to_bits(c, 240)[..], "{name}: control block {i}");
    }
    let payloads: Vec<&str> = f["payload_hex"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(r.block_ok.len(), payloads.len(), "{name}");
    for (i, p) in payloads.iter().enumerate() {
        if data_decodable {
            assert!(r.block_ok[i], "{name}: data block {i}");
            assert_eq!(r.block_bits[i][..1008], hex_to_bits(p, 1008)[..], "{name}: data block {i}");
        }
    }
    if !data_decodable {
        // 26.6 ms of the block's audio is missing from the capture.
        assert!(!r.block_ok.iter().any(|&b| b), "{name}: a block decoded across the capture gap");
    }
}

#[test]
fn offair_frames_decode_with_consistent_clock_and_snr() {
    let index = read_json(&dir().join("offair.json"));
    let opts = RxOptions::default();
    for (direction, d) in index["directions"].as_object().unwrap() {
        let long = &d["long_frame"];
        let (long_ppm, long_snr) = (long["clock_ppm"].as_f64().unwrap(), long["snr_db"].as_f64().unwrap());
        for f in d["frames"].as_array().unwrap() {
            let name = f["wav"].as_str().unwrap();
            let data_decodable = f["data_decodable"].as_bool().unwrap();
            let r = decode(name, &opts);
            check_content(name, f, &r, data_decodable);
            let (ppm, sd) = clock_ppm(&r);
            let s = snr(&r);
            eprintln!(
                "{name}: clock {ppm:+.1} +- {sd:.1} ppm (4 s frame {long_ppm:+.1}), SNR {s:.1} dB (4 s frame {long_snr:.1})"
            );
            assert!(ppm.abs() <= CLOCK_BOUND_PPM + 1e-9, "{name}: clock {ppm} ppm");
            if f["kind"] == "4s" {
                // The reference itself: the index holds what the full capture gave.
                assert!((ppm - long_ppm).abs() < 0.1 && sd < 2.0, "{direction} 4 s frame: clock {ppm} +- {sd}");
                assert!((s - long_snr).abs() < 0.15, "{direction} 4 s frame: SNR {s}");
                continue;
            }
            if !data_decodable {
                // Nothing to measure the SNR against: the fallback, which errs low.
                assert!(r.channel_snr.as_ref().unwrap().fallback, "{name}");
                assert!(s <= long_snr + SNR_OVER_DB, "{name}: SNR {s}");
                continue;
            }
            assert!((ppm - long_ppm).abs() <= CLOCK_TOLERANCE_PPM, "{name}: clock {ppm} vs {long_ppm} ppm");
            assert!((ppm - long_ppm).abs() <= 3.0 * sd + 3.0, "{name}: clock {ppm} +- {sd} vs {long_ppm} ppm");
            assert!(!r.channel_snr.as_ref().unwrap().fallback, "{name}: SNR fell back");
            assert!((s - long_snr).abs() <= SNR_TOLERANCE_DB, "{name}: SNR {s} vs {long_snr} dB");
            assert!(s - long_snr <= SNR_OVER_DB, "{name}: SNR {s} over-reports {long_snr} dB");
        }
    }
}

/// The tracked data level against the one every known symbol gives, dB.
const LEVEL_TOLERANCE_DB: f64 = 0.5;
/// Every captured frame's data sat at least this far above the S2 level, dB.
const RAMP_DB: f64 = 3.0;

#[test]
fn offair_level_tracking() {
    // The captured frames' data sat 3-6 dB above the level the S2 pair
    // measured (the radios' level still rising under the preamble). The level
    // tracker finds it as the simulator's does (the same fit, to rounding),
    // within LEVEL_TOLERANCE_DB of what every known symbol gives.
    let index = read_json(&dir().join("offair.json"));
    for d in index["directions"].as_object().unwrap().values() {
        for f in d["frames"].as_array().unwrap() {
            let name = f["wav"].as_str().unwrap();
            let r = decode(name, &RxOptions::default());
            let want = &f["python_level"];
            let lv = &r.level;
            assert_eq!(lv.applied, want["applied"].as_bool().unwrap(), "{name}");
            let close = |a: f64, b: f64| (a - b).abs() <= 1e-6 * b.abs().max(1.0);
            let segs = f64s(&want["segments"]);
            assert_eq!(lv.segments.len(), segs.len(), "{name}");
            for (a, b) in lv.segments.iter().zip(&segs) {
                assert!(close(*a, *b), "{name}: segment level {a} vs simulator {b}");
            }
            assert!(close(lv.settle, want["settle"].as_f64().unwrap()), "{name}: settling term");
            for (a, b) in lv.level.iter().zip(&f64s(&want["level"])) {
                assert!(close(*a, *b), "{name}: row level {a} vs simulator {b}");
            }
            assert_eq!(r.block_ok.iter().filter(|&&b| b).count() as u64, f["blocks_ok"].as_u64().unwrap(), "{name}");
            eprintln!(
                "{name}: data level {:+.2} dB (known symbols {:?}), settling {:+.3}, tau {} ms",
                lv.data_db(),
                f["data_gain_db"].as_f64(),
                lv.settle,
                lv.tau * 1e3
            );
            if f["data_decodable"].as_bool().unwrap() && f["data_gain_db"].is_f64() {
                let truth = f["data_gain_db"].as_f64().unwrap();
                assert!(truth > RAMP_DB, "{name}");
                assert!(lv.settle < 0.0, "{name}: the level was still rising after S2");
                assert!(
                    (lv.data_db() - truth).abs() < LEVEL_TOLERANCE_DB,
                    "{name}: level {} vs {truth} dB",
                    lv.data_db()
                );
            }
        }
    }
}

#[test]
fn offair_level_tracking_decodes_the_long_frame() {
    // B to A: the 4 s frame's data sat 6.2 dB above the S2 level. Untracked,
    // two of its 13 MCS 2 (16-QAM) blocks fail; tracked, all decode.
    let index = read_json(&dir().join("offair.json"));
    let f = index["directions"]["BtoA"]["frames"].as_array().unwrap().iter().find(|f| f["kind"] == "4s").unwrap();
    let name = f["wav"].as_str().unwrap();
    let r = decode(name, &RxOptions::default());
    check_content(name, f, &r, true);
    let off = decode(name, &RxOptions { level_tracking: false, ..Default::default() });
    let ok = off.block_ok.iter().filter(|&&b| b).count();
    assert_eq!(ok as u64, f["blocks_ok_untracked"].as_u64().unwrap());
    assert_eq!(ok, 11);
    assert!(!off.level.applied);
}

#[test]
fn offair_clock_prior_from_the_link() {
    // A link layer that measured the peer on a long frame supplies it as the
    // prior: short frames then read within a few ppm of it, and still decode.
    let index = read_json(&dir().join("offair.json"));
    for d in index["directions"].as_object().unwrap().values() {
        let long_ppm = d["long_frame"]["clock_ppm"].as_f64().unwrap();
        let opts = RxOptions { clock_prior: ClockPrior { ppm: long_ppm, sd_ppm: PRIOR_SD_PPM }, ..Default::default() };
        for f in d["frames"].as_array().unwrap() {
            let name = f["wav"].as_str().unwrap();
            let data_decodable = f["data_decodable"].as_bool().unwrap();
            let r = decode(name, &opts);
            check_content(name, f, &r, data_decodable);
            let (ppm, sd) = clock_ppm(&r);
            eprintln!("{name}: clock with the peer prior {ppm:+.1} +- {sd:.1} ppm (4 s frame {long_ppm:+.1})");
            assert!((ppm - long_ppm).abs() <= PRIOR_CLOCK_TOLERANCE_PPM, "{name}: clock {ppm} vs {long_ppm} ppm");
            assert!(sd <= PRIOR_SD_PPM + 1e-9, "{name}");
        }
    }
}

#[test]
fn offair_clock_prior_per_peer_from_the_link_source() {
    // The same prior given per header by a LinkSource (the sender is known
    // only once the header has decoded).
    struct Peer(LinkState, ClockPrior);
    impl cairn_phy::LinkSource for Peer {
        fn link_for(&self, _: &cairn_phy::Header) -> Option<LinkState> {
            Some(self.0.clone())
        }
        fn clock_prior(&self, _: &cairn_phy::Header) -> Option<ClockPrior> {
            Some(self.1)
        }
    }
    let index = read_json(&dir().join("offair.json"));
    let d = &index["directions"]["AtoB"];
    let long_ppm = d["long_frame"]["clock_ppm"].as_f64().unwrap();
    let peer = Peer(LinkState::from_cap(24, None), ClockPrior { ppm: long_ppm, sd_ppm: PRIOR_SD_PPM });
    let x = wav::read(&dir().join("AtoB_probe.wav")).unwrap();
    let r = Receiver::new().detect_and_receive(&x, &peer, &RxOptions::default()).unwrap();
    let (ppm, _) = clock_ppm(&r);
    assert!((ppm - long_ppm).abs() <= PRIOR_CLOCK_TOLERANCE_PPM, "clock {ppm} vs {long_ppm} ppm");
}
