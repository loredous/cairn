//! Where a frame's decode time goes: `cargo run --release --example rx_cost`.
//! For 4 s frames at 24 and 56 tones, MCS 0, 2, 4, 6, 3 dB above the rung's
//! threshold: the median time of `receive` with and without the ADR 0006
//! rebuild, on 1 and 4 decoder threads.

use std::time::Instant;

use cairn_phy::channel::{Rng, awgn};
use cairn_phy::{FrameConfig, Receiver, RxOptions, Transmitter, params};

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let reps = 15;
    let thr = [3.5, 6.0, 8.5, 12.5, 17.5, 21.0, 25.5];
    println!("width mcs blocks | rx 1 thread | rx 4 threads | no rebuild, 4 threads (ms, median of {reps})");
    for width in [24, 56] {
        for mcs in [0u8, 2, 4, 6] {
            let cfg0 = FrameConfig::new(width, mcs, 1).unwrap();
            let frame_s = if mcs <= 1 { 2.0 } else { 4.0 };
            let avail = params::block_symbols_available(frame_s, 60) - cfg0.probe_symbols();
            let n = avail * cfg0.rung().bits_per_point * cfg0.data_tones.len() / cfg0.rung().coded_bits;
            let cfg = FrameConfig::new(width, mcs, n).unwrap();
            let mut rng = Rng::new(7);
            let pl: Vec<Vec<u8>> = (0..n).map(|_| rng.bits(1008)).collect();
            let f = Transmitter::new().build(&cfg, &rng.bits(128), &pl);
            let mut x = vec![0.0; 1000];
            x.extend_from_slice(&f.samples);
            x.extend(vec![0.0; 1000]);
            let p = f.samples.iter().map(|v| v * v).sum::<f64>() / f.samples.len() as f64;
            let y = awgn(&x, thr[mcs as usize] + 3.0, width, &mut rng, Some(p));
            let mut rx = Receiver::new();
            let time = |rx: &mut Receiver, o: &RxOptions| {
                median(
                    (0..reps)
                        .map(|_| {
                            let t = Instant::now();
                            std::hint::black_box(rx.receive(&y, &cfg, 1000, o));
                            t.elapsed().as_secs_f64() * 1e3
                        })
                        .collect(),
                )
            };
            let one = time(&mut rx, &RxOptions::default());
            let four = time(&mut rx, &RxOptions { threads: 4, ..Default::default() });
            let quick = time(&mut rx, &RxOptions { threads: 4, rebuild_snr: false, ..Default::default() });
            println!("{width:5} {mcs:3} {n:6} | {one:11.1} | {four:12.1} | {quick:10.1}");
        }
    }
}
