//! Block error rate of the Rust port in AWGN, run the way the simulator's
//! `link_sim.bler_point` runs it (16 blocks per frame, 400-sample guards,
//! signal power over the padded frame, the SPEC 6.1 detector, a missed
//! preamble or failed header counting every block as an error), for the
//! cross-check against `modem/tools/bler_python.py`.
//!
//!     cargo run --release --example bler -- MCS SNR_DB[,SNR_DB...] [--blocks N] [--width W] [--threads T]

use cairn_phy::channel::{Rng, awgn};
use cairn_phy::detector::{DetectorConfig, detect};
use cairn_phy::{FrameConfig, Receiver, RxOptions, Transmitter, params};

const GUARD: usize = 400;
const BLOCKS_PER_FRAME: usize = 16;

/// (blocks, block errors, header errors, missed frames) over `frames` frames.
fn run(cfg: &FrameConfig, snr: f64, frames: std::ops::Range<u64>) -> (usize, usize, usize, usize) {
    let (mut blocks, mut errors, mut hdr, mut missed) = (0, 0, 0, 0);
    let mut txr = Transmitter::new();
    let mut rx = Receiver::new();
    for f in frames {
        let mut rng = Rng::new(0xB1E5 ^ (f << 8) ^ (snr * 100.0) as u64);
        let header = rng.bits(128);
        let payloads: Vec<Vec<u8>> = (0..cfg.n_blocks).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
        let frame = txr.build(cfg, &header, &payloads);
        let mut x = vec![0.0; GUARD];
        x.extend_from_slice(&frame.samples);
        x.extend(vec![0.0; GUARD]);
        let y = awgn(&x, snr, cfg.width, &mut rng, None);
        blocks += cfg.n_blocks;
        let det = detect(&y, &DetectorConfig::default())
            .into_iter()
            .filter(|d| d.cp == cfg.cp && (d.start as i64 - GUARD as i64).unsigned_abs() < cfg.stride() as u64)
            .max_by(|a, b| a.quality.total_cmp(&b.quality));
        let Some(det) = det else {
            missed += 1;
            errors += cfg.n_blocks;
            continue;
        };
        let r = rx.receive(&y, cfg, det.start, &RxOptions::default());
        if !r.header_ok {
            hdr += 1;
            errors += cfg.n_blocks;
            continue;
        }
        errors +=
            r.block_ok.iter().zip(&r.block_bits).zip(&frame.block_bits).filter(|((ok, b), t)| !**ok || b != t).count();
    }
    (blocks, errors, hdr, missed)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str, default: usize| -> usize {
        args.iter().position(|a| a == name).map_or(default, |i| args[i + 1].parse().unwrap())
    };
    let mcs: u8 = args[1].parse().unwrap();
    let snrs: Vec<f64> = args[2].split(',').map(|s| s.parse().unwrap()).collect();
    let n_blocks = opt("--blocks", 6400);
    let width = opt("--width", 24);
    let threads = opt("--threads", 1).max(1);
    let cfg = FrameConfig::new(width, mcs, BLOCKS_PER_FRAME).unwrap();
    let n_frames = n_blocks.div_ceil(BLOCKS_PER_FRAME) as u64;
    for snr in snrs {
        let per = n_frames.div_ceil(threads as u64);
        let parts: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads as u64)
                .map(|t| {
                    let cfg = &cfg;
                    s.spawn(move || run(cfg, snr, (t * per).min(n_frames)..((t + 1) * per).min(n_frames)))
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let (b, e, h, m) = parts.iter().fold((0, 0, 0, 0), |a, p| (a.0 + p.0, a.1 + p.1, a.2 + p.2, a.3 + p.3));
        println!(
            "{{\"mcs\": {mcs}, \"width\": {width}, \"snr_db\": {snr}, \"blocks\": {b}, \"block_errors\": {e}, \"header_errors\": {h}, \"missed_frames\": {m}, \"bler\": {}}}",
            e as f64 / b as f64
        );
    }
}
