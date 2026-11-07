//! The Rust detector and receiver against the Stage 1 simulator on the
//! fixtures written by `modem/tools/gen_rx_fixtures.py`: the same 16-bit
//! audio, the same detections, the same blocks decoded. Also the Rust
//! clip-and-filter against the simulator's on `fixtures/tx/`.

mod common;

use cairn_phy::detector::{self, DetectorConfig};
use cairn_phy::{Receiver, RxOptions, Transmitter, wav};
use common::*;

fn fixtures(dir: &str, ext: &str) -> Vec<std::path::PathBuf> {
    let d = modem_dir().join("fixtures").join(dir);
    let mut v: Vec<_> = std::fs::read_dir(&d)
        .unwrap_or_else(|e| panic!("{}: {e}", d.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    v.sort();
    v
}

#[test]
fn clip_and_filter_matches_simulator() {
    let paths = fixtures("tx", "json");
    assert_eq!(paths.len(), 3);
    for p in paths {
        let doc = read_json(&p);
        let cfg = fixture_config(&doc);
        assert_eq!(doc["clip_db"].as_f64().unwrap(), cfg.clip_db);
        let (h, pl) = fixture_inputs(&doc);
        let frame = Transmitter::new().build(&cfg, &h, &pl);
        let want = read_f64(&p.with_extension("f64"));
        assert_eq!(want.len(), frame.samples.len());
        let worst = want.iter().zip(&frame.samples).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        eprintln!("{}: clip-and-filter max |diff| {worst:.2e}", doc["name"]);
        assert!(worst < 1e-9, "{}: {worst:e}", doc["name"]);
        // SPEC 7.1: PAPR at most the clip level on every symbol.
        // (The final clip is iterated 12 times, as in the simulator, so the
        // bound holds to within a few thousandths of a dB.)
        let papr = cairn_phy::ofdm::papr_db(&frame.samples, cfg.cp);
        let over = papr.iter().map(|v| v - cfg.clip_db).fold(f64::MIN, f64::max);
        eprintln!("{}: worst PAPR - clip level {over:.2e} dB", doc["name"]);
        assert!(over < 0.01);
    }
}

#[test]
fn receiver_matches_simulator() {
    let paths = fixtures("rx", "json");
    assert!(paths.len() >= 16);
    let mut rx = Receiver::new();
    let (mut blocks, mut agree, mut rust_ok, mut py_ok) = (0, 0, 0, 0);
    for p in paths {
        let doc = read_json(&p);
        let name = doc["name"].as_str().unwrap().to_string();
        let cfg = fixture_config(&doc);
        let samples = wav::read(&p.with_extension("wav")).unwrap();
        let (_, payloads) = fixture_inputs(&doc);
        let py = &doc["python"];

        // Detector: same detections, same starts and prefixes.
        let dets = detector::detect(&samples, &DetectorConfig::default());
        let py_dets = py["detections"].as_array().unwrap();
        assert_eq!(dets.len(), py_dets.len(), "{name}: detection count");
        for (d, pd) in dets.iter().zip(py_dets) {
            assert_eq!(d.start as u64, pd["start"].as_u64().unwrap(), "{name}: start");
            assert_eq!(d.cp as u64, pd["cp"].as_u64().unwrap(), "{name}: prefix");
            assert_eq!(d.ppm, pd["ppm"].as_f64().unwrap(), "{name}: ppm");
            assert!((d.quality - pd["quality"].as_f64().unwrap()).abs() < 1e-9, "{name}: quality");
            assert!((d.autocorr - pd["autocorr"].as_f64().unwrap()).abs() < 1e-9, "{name}: autocorr");
            assert!((d.coarse_start - pd["coarse_start"].as_f64().unwrap()).abs() < 1e-6, "{name}: coarse");
        }

        // Receiver, from the simulator's start.
        let prx = &py["rx"];
        let start = prx["start"].as_u64().unwrap() as usize;
        let r = rx.receive(&samples, &cfg, start, &RxOptions::default());
        let correct: Vec<bool> = r
            .block_ok
            .iter()
            .zip(&r.block_bits)
            .zip(&payloads)
            .map(|((&ok, b), p)| ok && &b[..1008] == p.as_slice())
            .collect();
        let py_correct: Vec<bool> =
            prx["block_correct"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
        assert_eq!(r.header_ok, prx["header_ok"].as_bool().unwrap(), "{name}: header");
        let py_snr = f64s(&prx["snr_db"]);
        let snr_diff = r.snr_db.iter().zip(&py_snr).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        let py_iter: Vec<usize> =
            prx["iterations"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
        eprintln!(
            "{name}: rust {}/{} python {}/{} blocks; snr max |diff| {snr_diff:.1e} dB; iterations {:?} vs {:?}",
            correct.iter().filter(|&&v| v).count(),
            correct.len(),
            py_correct.iter().filter(|&&v| v).count(),
            py_correct.len(),
            r.iterations,
            py_iter
        );
        assert!(snr_diff < 1e-6, "{name}: per-tone SNR differs by {snr_diff} dB");
        let near_threshold = name.contains("snr7");
        if !near_threshold {
            assert!(correct.iter().all(|&v| v), "{name}: Rust failed a block");
            assert_eq!(correct, py_correct, "{name}");
        }
        blocks += correct.len();
        agree += correct.iter().zip(&py_correct).filter(|(a, b)| a == b).count();
        rust_ok += correct.iter().filter(|&&v| v).count();
        py_ok += py_correct.iter().filter(|&&v| v).count();
    }
    eprintln!("blocks {blocks}: agree {agree}, rust ok {rust_ok}, python ok {py_ok}");
    assert!(blocks - agree <= 1);
}
