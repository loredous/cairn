//! Receive pipeline under machine load: the full receiver (header-first
//! detection and decoding, as `cairn-modem listen`) on audio played at
//! real-time pace through the mock capture device, while N threads burn CPU.
//! The mock device captures by the wall clock whatever the scheduler does
//! and its audio thread wakes once a period, like ALSA's: if it wakes later
//! than the buffer allows, the stream overruns and samples are lost.
//!
//! Reports per configuration: xruns, gaps and the time lost, the capture
//! ring's high-water mark, frames decoded (and those with a gap), and the
//! decode latency (last sample at the processing thread to result) and
//! receive turnaround (last sample captured to result, SPEC 7.3 budget
//! 100 ms) as p50 / p99 / max.
//!
//! ```sh
//! cargo run --release -p cairn-modem --example rx_load -- --burn 8 --seconds 60 --matrix
//! cargo run --release -p cairn-modem --example rx_load -- --wav capture.wav --burn 16 --no-rt --buffer-ms 20
//! cargo run --release -p cairn-modem --example rx_load -- --burn 16 --mem 256 --matrix   # plus memory churn
//! ```
//!
//! `--matrix` runs four configurations back to back: the old capture path
//! (no real-time priority, a 20 ms buffer of two 10 ms periods, as cpal
//! opens it), each change alone, and both (real-time priority and the
//! 200 ms native ALSA buffer).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use cairn_audio::{InputStream, MockCapture, RtConfig};
use cairn_modem::headers::{HeaderSpec, build_header_frame};
use cairn_modem::listen::{self, HeaderFirst, InputSource, ListenOptions, Output};
use cairn_phy::channel::Rng;
use cairn_phy::detector::DetectorConfig;
use cairn_phy::fields::{Address, frame_type};
use cairn_phy::{LinkState, RxOptions, Transmitter};

const FS: usize = 48_000;

struct Args {
    wav: Option<String>,
    seconds: f64,
    burn: usize,
    rt: bool,
    rt_prio: u8,
    buffer_ms: f64,
    period: usize,
    threads: usize,
    matrix: bool,
    mem_mb: usize,
}

fn parse() -> Args {
    let mut a = Args {
        wav: None,
        seconds: 60.0,
        burn: std::thread::available_parallelism().map_or(4, |n| n.get()),
        rt: true,
        rt_prio: 60,
        buffer_ms: 200.0,
        period: 480,
        threads: 4,
        matrix: false,
        mem_mb: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--wav" => a.wav = Some(v()),
            "--seconds" => a.seconds = v().parse().expect("--seconds"),
            "--burn" => a.burn = v().parse().expect("--burn"),
            "--no-rt" => a.rt = false,
            "--rt-prio" => a.rt_prio = v().parse().expect("--rt-prio"),
            "--buffer-ms" => a.buffer_ms = v().parse().expect("--buffer-ms"),
            "--period" => a.period = v().parse().expect("--period"),
            "--threads" => a.threads = v().parse().expect("--threads"),
            "--matrix" => a.matrix = true,
            "--mem" => a.mem_mb = v().parse().expect("--mem"),
            "-h" | "--help" => {
                println!(
                    "rx_load [--wav F] [--seconds S] [--burn N] [--no-rt] [--rt-prio P] [--buffer-ms B] [--period F] [--threads T] [--mem MB] [--matrix]"
                );
                std::process::exit(0);
            }
            _ => panic!("unknown argument {k}"),
        }
    }
    a
}

/// UI broadcasts at MCS 0-4 and widths 24/32 with short gaps, in noise
/// (about 25 dB SNR), for `seconds`.
fn synthetic(seconds: f64) -> Vec<f32> {
    let me = Address::new("N0LOAD", 1).unwrap();
    let mut tx = Transmitter::new();
    let mut rng = Rng::new(42);
    let n = (seconds * FS as f64) as usize;
    let mut x = vec![0.0f64; FS / 4];
    let mut k = 0u16;
    while x.len() < n {
        let mcs = (k % 5) as u8;
        let width = if k % 3 == 2 { 32 } else { 24 };
        let blocks = [3, 4, 5, 6, 8][mcs as usize];
        let spec = HeaderSpec::broadcast(me.clone(), frame_type::UI, k, width, mcs, blocks);
        let (f, _) = build_header_frame(&mut tx, &spec, 1000 + k as u64).expect("frame");
        x.extend(f.output().iter().map(|v| v * 0.5));
        x.extend(std::iter::repeat_n(0.0, FS / 4 + (k as usize % 4) * FS / 8));
        k += 1;
    }
    x.truncate(n);
    for v in &mut x {
        *v += 0.01 * rng.normal();
    }
    x.iter().map(|&v| v as f32).collect()
}

/// Busy threads at normal priority until `stop`. With `mem_mb`, each one
/// also maps, touches and unmaps that much memory over and over (page
/// faults, zeroing, TLB shootdowns across every core: the kernel work a
/// parallel compile adds to the CPU load).
fn burners(n: usize, mem_mb: usize, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<u64>> {
    (0..n)
        .map(|i| {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(format!("burn-{i}"))
                .spawn(move || {
                    let mut x = 1.0f64 + i as f64;
                    let mut n = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        if mem_mb > 0 {
                            let mut v = vec![0u8; mem_mb << 20];
                            for p in v.iter_mut().step_by(4096) {
                                *p = n as u8;
                            }
                            n += v[v.len() / 2] as u64 & 1;
                        }
                        for _ in 0..10_000 {
                            x = (x * 1.000_001 + 0.5).sqrt() + 1.0;
                        }
                        n += 1;
                    }
                    n + x as u64 % 2
                })
                .expect("spawn burner")
        })
        .collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

struct Row {
    label: String,
    capture_rt: String,
    processing_rt: String,
    xruns: u64,
    gaps: u64,
    lost_ms: f64,
    overflows: u64,
    high_ms: f64,
    frames: u64,
    complete: u64,
    gapped: u64,
    latency: (f64, f64, f64),
    turnaround: (f64, f64, f64),
}

fn run(x: &[f32], a: &Args, rt: bool, buffer_ms: f64) -> Row {
    let rt_cfg = RtConfig { enabled: rt, audio_priority: a.rt_prio };
    let mut m = MockCapture::new(x.to_vec());
    m.paced = true;
    m.period = a.period;
    m.buffer = ((buffer_ms * FS as f64 / 1e3).round() as usize).max(2 * a.period);
    m.rt = rt_cfg.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let burn = burners(a.burn, a.mem_mb, stop.clone());
    // Let the burners take the cores first.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let input = InputStream::mock(m).expect("mock capture");
    let capture_rt = input.rt.as_ref().map_or("?".into(), |g| g.to_string());
    let mut src = InputSource::new(input, None);
    let opts = ListenOptions {
        detector: DetectorConfig::default(),
        rx: RxOptions { threads: a.threads, ..Default::default() },
        stats_every: 0,
        boost: true,
        rt: rt_cfg,
    };
    let link = LinkState { width: 24, ..Default::default() };
    let mut lat = Vec::new();
    let mut turn = Vec::new();
    let mut processing_rt = String::from("normal");
    let t0 = Instant::now();
    let totals =
        listen::run(&mut src, Box::new(HeaderFirst::new(link, RxOptions::default())), opts, &mut |o| match o {
            Output::Frame(f) => {
                lat.push(f.latency_ms);
                if let Some(t) = f.turnaround_ms {
                    turn.push(t);
                }
            }
            Output::Note(n) if n.starts_with("processing thread") => {
                processing_rt = n.replace("processing thread: ", "")
            }
            _ => {}
        });
    let wall = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for b in burn {
        let _ = b.join();
    }
    let s = src.input.stats();
    let label = format!(
        "{} rt, buffer {:.0} ms",
        if rt { "with" } else { "no" },
        buffer_ms.max(2e3 * a.period as f64 / FS as f64)
    );
    eprintln!("  {label}: {:.1} s of audio in {wall:.1} s", totals.samples as f64 / FS as f64);
    let tri = |v: &mut Vec<f64>| (pct(v, 0.5), pct(v, 0.99), pct(v, 1.0));
    Row {
        label,
        capture_rt,
        processing_rt,
        xruns: s.xruns,
        gaps: s.gaps,
        lost_ms: s.gap_ns as f64 / 1e6,
        overflows: s.overflow_events,
        high_ms: s.ring_high_ms(),
        frames: totals.frames,
        complete: totals.complete,
        gapped: totals.gapped,
        latency: tri(&mut lat),
        turnaround: tri(&mut turn),
    }
}

fn main() {
    let a = parse();
    let x = match &a.wav {
        Some(p) => cairn_phy::wav::read(std::path::Path::new(p))
            .unwrap_or_else(|e| panic!("{p}: {e}"))
            .iter()
            .map(|&v| v as f32)
            .collect(),
        None => synthetic(a.seconds),
    };
    let x: Vec<f32> = x.into_iter().take((a.seconds * FS as f64) as usize).collect();
    println!(
        "rx_load: {:.0} s of audio at real-time pace, {} burner threads{} on {} cores, period {} ({:.1} ms), {} decoder threads",
        x.len() as f64 / FS as f64,
        a.burn,
        if a.mem_mb > 0 { format!(" (each churning {} MB)", a.mem_mb) } else { String::new() },
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        a.period,
        a.period as f64 * 1e3 / FS as f64,
        a.threads
    );
    let configs: Vec<(bool, f64)> = if a.matrix {
        let old = 2e3 * a.period as f64 / FS as f64;
        vec![(false, old), (false, a.buffer_ms), (true, old), (true, a.buffer_ms)]
    } else {
        vec![(a.rt, a.buffer_ms)]
    };
    let rows: Vec<Row> = configs.iter().map(|&(rt, b)| run(&x, &a, rt, b)).collect();
    println!(
        "\n| configuration | capture thread | processing | xruns | gaps (ms lost) | overflows | ring high-water | frames (complete, gapped) | decode latency p50/p99/max | turnaround p50/p99/max |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} ({:.1}) | {} | {:.0} ms | {} ({}, {}) | {:.0}/{:.0}/{:.0} ms | {:.0}/{:.0}/{:.0} ms |",
            r.label,
            r.capture_rt,
            r.processing_rt,
            r.xruns,
            r.gaps,
            r.lost_ms,
            r.overflows,
            r.high_ms,
            r.frames,
            r.complete,
            r.gapped,
            r.latency.0,
            r.latency.1,
            r.latency.2,
            r.turnaround.0,
            r.turnaround.1,
            r.turnaround.2
        );
    }
}
