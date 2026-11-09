//! Carrier detect (SPEC 7.2) over a recording: every quieting edge, and
//! the quieting rises counted by where they fall.
//!
//! ```sh
//! cargo run --release --example dcd_wav -- cap.wav [--band 2800-3200] [--width 24] [--legacy] [-v]
//!     [--confirm HOPS] [--min-off S] [--min-on S]
//! ```
//!
//! The reference for "where" is offline: a carrier is on while the median
//! (over 50 ms) of the 4.3-5.0 kHz band power, above every tone of a link up
//! to 40 tones, is 15 dB or more below that band's open-squelch median (the
//! radio's discriminator noise there drops by 40 dB under any carrier, and a
//! frame's own spectrum stays 30 dB below the noise). A rise within 150 ms
//! of a reference carrier edge counts as a transition, one inside a carrier
//! as "during" (a dropout and its recovery), the rest as idle (a false
//! assert on open-squelch noise).

use std::path::PathBuf;

use cairn_phy::dcd::{BandPass, Dcd, DcdConfig, DcdSource, HOP};
use cairn_phy::params::FS;

fn main() {
    let mut path = None;
    let mut cfg = DcdConfig::default();
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--band" => {
                let v = args.next().expect("--band LO-HI");
                let (lo, hi) = v.split_once('-').expect("--band LO-HI");
                cfg.quieting_band_hz = (lo.parse().expect("Hz"), hi.parse().expect("Hz"));
            }
            "--width" => {
                let band = cfg.quieting_band_hz;
                let w: usize = args.next().expect("--width N").parse().expect("tones");
                let legacy = cfg.recovery_max_s == 0.0;
                cfg = DcdConfig::for_width(w);
                if legacy {
                    cfg = cfg.legacy();
                }
                if band != DcdConfig::default().quieting_band_hz {
                    cfg.quieting_band_hz = band;
                }
            }
            "--legacy" => cfg = cfg.legacy(),
            "--confirm" => cfg.release_confirm_hops = args.next().expect("--confirm N").parse().expect("hops"),
            "--min-off" => cfg.min_off_s = args.next().expect("--min-off S").parse().expect("s"),
            "--min-on" => cfg.min_on_s = args.next().expect("--min-on S").parse().expect("s"),
            "-v" => verbose = true,
            _ => path = Some(PathBuf::from(a)),
        }
    }
    let path = path.expect("a WAV file");
    let x = cairn_phy::wav::read(&path).expect("read WAV");
    let hops = x.len() / HOP;

    // Offline carrier reference.
    let mut bp = BandPass::butterworth(4, 4300.0, 5000.0, FS as f64);
    let mut p = Vec::with_capacity(hops);
    for h in 0..hops {
        let s: f64 = x[h * HOP..(h + 1) * HOP].iter().map(|&v| bp.process(v).powi(2)).sum();
        p.push(10.0 * (s / HOP as f64).max(1e-30).log10());
    }
    let mut sorted = p.clone();
    sorted.sort_by(f64::total_cmp);
    let noise = sorted[sorted.len() * 3 / 4];
    let carrier: Vec<bool> = (0..hops)
        .map(|h| {
            let mut w: Vec<f64> = p[h.saturating_sub(2)..(h + 3).min(hops)].to_vec();
            w.sort_by(f64::total_cmp);
            w[w.len() / 2] < noise - 15.0
        })
        .collect();
    let mut edges_ref = Vec::new();
    for h in 1..hops {
        if carrier[h] != carrier[h - 1] {
            edges_ref.push(h);
        }
    }
    let n_carriers = edges_ref.iter().filter(|&&h| carrier[h]).count();
    let carrier_s = carrier.iter().filter(|&&c| c).count() as f64 * HOP as f64 / FS as f64;

    let mut dcd = Dcd::new(cfg.clone());
    let mut rises = [0usize; 3];
    let mut edges_total = 0;
    for (i, chunk) in x.chunks(HOP).enumerate() {
        for e in dcd.process(chunk) {
            if e.source != DcdSource::Quieting {
                continue;
            }
            edges_total += 1;
            let h = (e.at / HOP).saturating_sub(1).min(hops - 1);
            let near = edges_ref.iter().any(|&r| r.abs_diff(h) <= 15);
            let kind = if near {
                1
            } else if carrier[h] {
                2
            } else {
                0
            };
            if e.up {
                rises[kind] += 1;
            }
            if verbose {
                println!(
                    "{:8.3} s  quieting {}  ({}; floor {})",
                    e.at as f64 / FS as f64,
                    if e.up { "up  " } else { "down" },
                    ["idle", "transition", "during carrier"][kind],
                    dcd.floor_dbfs().map_or("unarmed".into(), |f| format!("{f:.1} dB"))
                );
            }
        }
        let _ = i;
    }
    println!(
        "{}: {:.1} s, band {:.0}-{:.0} Hz{}; reference: {} carriers, {:.1} s keyed, noise {:.1} dB at 4.3-5 kHz",
        path.display(),
        x.len() as f64 / FS as f64,
        cfg.quieting_band_hz.0,
        cfg.quieting_band_hz.1,
        if cfg.recovery_max_s == 0.0 { " (legacy)" } else { "" },
        n_carriers,
        carrier_s,
        noise
    );
    println!(
        "quieting rises: {} idle, {} at transitions, {} during carriers ({} edges); floor {}",
        rises[0],
        rises[1],
        rises[2],
        edges_total,
        dcd.floor_dbfs().map_or("unarmed".into(), |f| format!("{f:.1} dB"))
    );
}
