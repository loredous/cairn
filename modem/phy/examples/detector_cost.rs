//! CPU cost of the streaming detectors on noise, per second of audio:
//! `cargo run --release --example detector_cost [seconds] [chunk]`.

use std::time::Instant;

use cairn_phy::channel::Rng;
use cairn_phy::detector::{Detector, DetectorConfig, RescanDetector};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let secs: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let chunk: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(480);
    let mut rng = Rng::new(1);
    let x: Vec<f64> = (0..secs * 48_000).map(|_| 0.01 * rng.normal()).collect();
    let t = Instant::now();
    let mut det = Detector::new(DetectorConfig::default());
    let mut n = 0;
    for c in x.chunks(chunk) {
        n += det.process(c).len();
    }
    let inc = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut old = RescanDetector::new(DetectorConfig::default());
    for c in x.chunks(chunk) {
        n += old.process(c).len();
    }
    let res = t.elapsed().as_secs_f64();
    println!(
        "{secs} s of noise in chunks of {chunk}: incremental {:.1} ms/s ({:.2}% of a core), rescanning {:.1} ms/s ({:.2}%); {n} detections",
        inc * 1e3 / secs as f64,
        inc * 100.0 / secs as f64,
        res * 1e3 / secs as f64,
        res * 100.0 / secs as f64
    );
}
