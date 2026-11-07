//! Single-core timing of a full 4 s frame: build (encode, map, synthesize,
//! clip-and-filter), detect (the SPEC 6.1 detector over the frame), and
//! receive (estimation, demapping, decoding) at three SNRs: 1 dB above the
//! rung's threshold, 3 dB below it (most blocks fail after 30 iterations, the
//! decoder's worst case), and clean.
//!
//!     cargo run --release --example bench [-- --reps N]
//!
//! Times are the fastest of N repetitions (default 5).

use std::time::{Duration, Instant};

use cairn_phy::channel::{Rng, awgn};
use cairn_phy::detector::{DetectorConfig, detect};
use cairn_phy::{FrameConfig, Receiver, RxOptions, Transmitter, params};

/// Fastest of the repetitions, in ms: other jobs on the machine only ever
/// add time, so the minimum is the best estimate of the compute cost.
fn best(v: Vec<Duration>) -> f64 {
    v.into_iter().min().unwrap().as_secs_f64() * 1e3
}

fn main() {
    let reps: usize = std::env::args().skip_while(|a| a != "--reps").nth(1).map_or(5, |s| s.parse().unwrap());
    println!(
        "| Width | MCS | Blocks | Frame (s) | TX (ms) | Detect (ms) | RX +1 dB (ms) | RX -3 dB (ms) | RX clean (ms) | Worst RX / frame |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut rng = Rng::new(1);
    let mut txr = Transmitter::new();
    let mut rx = Receiver::new();
    let opts = RxOptions::default();
    for width in [24, 56] {
        for mcs in 0..7u8 {
            let n = params::blocks_per_frame(mcs, width, 4.0);
            let cfg = FrameConfig::new(width, mcs, n).unwrap();
            let header = rng.bits(128);
            let payloads: Vec<Vec<u8>> = (0..n).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
            let mut t_tx = Vec::new();
            let mut frame = None;
            for _ in 0..reps {
                let t = Instant::now();
                frame = Some(txr.build(&cfg, &header, &payloads));
                t_tx.push(t.elapsed());
            }
            let frame = frame.unwrap();
            let guard = 2000;
            let mut x = vec![0.0; guard];
            x.extend_from_slice(&frame.samples);
            x.extend(vec![0.0; guard]);
            let p = frame.samples.iter().map(|v| v * v).sum::<f64>() / frame.samples.len() as f64;
            let thr = params::RUNGS[mcs as usize].threshold_db - params::clip_offset_db(mcs);
            let mut t_det = Vec::new();
            let mut t_rx = [Vec::new(), Vec::new(), Vec::new()];
            for (i, snr) in [Some(thr + 1.0), Some(thr - 3.0), None].into_iter().enumerate() {
                for _ in 0..reps {
                    let y = match snr {
                        Some(s) => awgn(&x, s, width, &mut rng, Some(p)),
                        None => x.clone(),
                    };
                    let start = if i == 0 {
                        let t = Instant::now();
                        let d = detect(&y, &DetectorConfig::default());
                        t_det.push(t.elapsed());
                        d.first().map_or(guard, |d| d.start)
                    } else {
                        guard
                    };
                    let t = Instant::now();
                    let r = rx.receive(&y, &cfg, start, &opts);
                    t_rx[i].push(t.elapsed());
                    std::hint::black_box(r);
                }
            }
            let dur = frame.samples.len() as f64 / params::FS as f64;
            let [a, b, c] = t_rx.map(best);
            let det = best(t_det);
            println!(
                "| {width} | {mcs} | {n} | {dur:.2} | {:.1} | {:.1} | {a:.1} | {b:.1} | {c:.1} | {:.3} |",
                best(t_tx),
                det,
                (b + det) / 1e3 / dur
            );
        }
    }
}
