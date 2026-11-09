//! The rate adapter on the weak 70 cm link of 2026-11-08 (`sim/findings.md`,
//! "A weak 70 cm link"): 32 tones, the radios' dead low tones, and the
//! sender's comb of 1 kHz harmonics, whose level moved by 8 dB between the
//! four setups of the evening while the floor under it moved by 5 dB. Each
//! setup is held for a minute, as on air (the reported SNR held to 0.3 dB
//! between the frames of a setup). Throughput is set against an oracle that
//! knows each setup's channel and picks the best rung and mask for it.

mod common;

use std::time::Duration;

use cairn_link::capacity::{AppendixA, expected_net_kbps};
use cairn_link::frame::caps;
use cairn_link::params::BLOCK_PAYLOAD;
use cairn_link::params::{ALL_TONES, RUNG_THRESHOLDS_DB};
use cairn_link::{FrameType, HostCommand, Instant};
use common::*;

/// The four 70 cm A->B setups: the floor under the lines (the reported SNR
/// with the lines removed, MCS 4 frames, 7 dB-equivalent) and the lines
/// against one tone of 29 (`sim` `CAPTURE_COMBS`, 7 dB-equivalent).
const SETUPS: [(&str, f64, [f64; 4]); 4] = [
    ("first session", 22.4, [-9.0, -5.2, -5.2, -24.5]),
    ("new antenna", 24.4, [-13.6, -9.4, -8.5, -28.5]),
    ("other radio", 22.5, [-10.8, -6.5, -5.6, -25.5]),
    ("antenna moved", 18.9, [-6.1, -1.8, -0.8, -20.6]),
];
const HOLD_S: f64 = 60.0;
/// The radios' low tones (2026-11-08, as `scenarios.rs`).
const LOW_TONES: [(u8, f64); 3] = [(0, 45.0), (1, 27.0), (2, 20.0)];
/// The tones either side of the 1, 2 and 3 kHz lines.
const COMB_TONES: [u8; 6] = [6, 7, 16, 17, 26, 27];

fn connect(sim: &mut Sim, a: usize, b: usize, at: Instant) {
    let peer = sim.addr(b);
    sim.host(a, at, HostCommand::Connect { peer, new_session: false });
}

/// Per-tone SNR of a setup at 32 tones after a canceller of `cancel_db`.
fn tone_snr(floor: f64, comb: &[f64], cancel_db: f64) -> Vec<f64> {
    let noise = comb_noise(comb, 32, cancel_db);
    (0..32)
        .map(|k| {
            let depth = LOW_TONES.iter().find(|(t, _)| usize::from(*t) == k).map_or(0.0, |x| x.1);
            -10.0 * (noise[k] + 10f64.powf(-(floor - depth) / 10.0)).log10()
        })
        .collect()
}

/// The oracle's net rate (kbps) and rung for a setup: the best rung and
/// mask (tones 0-2 dropped, with or without the tones beside the lines),
/// rate times the fraction of blocks that pass, on the simulator's model.
fn oracle(floor: f64, comb: &[f64], cancel_db: f64) -> (f64, u8) {
    let snr = tone_snr(floor, comb, cancel_db);
    let model = AppendixA::with_txdelay(Duration::from_millis(150));
    let low: u64 = 0b111;
    let near: u64 = COMB_TONES.iter().fold(0, |m, &t| m | 1 << t);
    let mut best = (0.0, 0);
    for mask in [ALL_TONES & !low, ALL_TONES & !low & !near] {
        let used: Vec<f64> = (0..32).filter(|&k| mask >> k & 1 == 1).map(|k| snr[k]).collect();
        for mcs in 0..7u8 {
            let eff = effective_snr(&used, ESM_BETA[mcs as usize]);
            let ok = 1.0 - bler(eff, RUNG_THRESHOLDS_DB[mcs as usize]);
            let limit = Duration::from_secs(if mcs <= 1 { 2 } else { 4 });
            let rate = expected_net_kbps(&model, mcs, 32, mask, limit) * ok;
            if rate > best.0 {
                best = (rate, mcs);
            }
        }
    }
    best
}

struct Run {
    /// Per setup: payload kbps delivered, DATA frames, frames with more
    /// than 20% of their blocks failed, rungs used.
    setups: Vec<(f64, usize, usize, Vec<u8>)>,
}

fn run(seed: u64, cancel_db: f64) -> Run {
    let mut sim = pair_with(seed, SETUPS[0].1, |c| {
        c.track_masks = true;
        c.width_cap = 31;
        c.capabilities |= caps::WIDE;
    });
    sim.tone_powers = true;
    sim.low_tone_spill_db = Some(22.0);
    sim.line_cancel_db = cancel_db;
    for (a, b) in [(0, 1), (1, 0)] {
        sim.impair(t(0.0), a, b, LOW_TONES.to_vec());
    }
    for (i, (_, floor, comb)) in SETUPS.iter().enumerate() {
        let at = t(i as f64 * HOLD_S);
        sim.snr_step(at, 0, 1, *floor);
        sim.comb(at, 0, 1, comb.to_vec());
    }
    connect(&mut sim, 0, 1, t(0.1));
    send_chunks(&mut sim, 0, t(0.2), &payload(2_000_000, seed), 1000);
    let end = SETUPS.len() as f64 * HOLD_S;
    sim.run_until(t(end + 1.0));
    assert_clean(&sim);
    let frames: Vec<(Instant, u8)> = sim
        .frames_from(0)
        .iter()
        .filter_map(|a| a.frame.as_ref().filter(|f| f.ftype == FrameType::Data).map(|f| (a.start, f.mcs)))
        .collect();
    let rx = &sim.stations[1].rx_data;
    let setups = (0..SETUPS.len())
        .map(|i| {
            // The first 10 s of each setup belong to the change, not the setup.
            let (from, to) = (t(i as f64 * HOLD_S + 10.0), t((i + 1) as f64 * HOLD_S));
            let inside: Vec<_> = rx.iter().filter(|(at, _)| *at >= from && *at < to).collect();
            let ok: usize = inside.iter().map(|(_, v)| v.iter().filter(|b| b.1).count()).sum();
            let bad =
                inside.iter().filter(|(_, v)| v.iter().filter(|b| !b.1).count() as f64 > 0.2 * v.len() as f64).count();
            let rungs = frames.iter().filter(|(s, _)| *s >= from && *s < to).map(|f| f.1).collect();
            let kbps = (ok * BLOCK_PAYLOAD * 8) as f64 / (HOLD_S - 10.0) / 1000.0;
            (kbps, inside.len(), bad, rungs)
        })
        .collect();
    Run { setups }
}

#[test]
fn comb_leakage_reproduces_the_70cm_tones() {
    // First setup, MCS 4 (the lines 1 dB stronger against a rung clipped at
    // 8 dB): the tones either side of 1 and 2 kHz measured 10.6-10.9 and
    // 7.8-8.0 dB on air, the others about 18-20 dB.
    let comb: Vec<f64> = SETUPS[0].2.iter().map(|c| c + 1.0).collect();
    let s = tone_snr(19.5, &comb, 0.0);
    for (k, want) in [(6, 10.8), (7, 10.8), (16, 7.9), (17, 7.9), (11, 19.0)] {
        assert!((s[k] - want).abs() < 1.0, "tone {k}: {:.1} dB, on air {want}", s[k]);
    }
    // The canceller leaves them at the floor.
    let c = tone_snr(19.5, &comb, LINE_CANCEL_DB);
    assert!(COMB_TONES.iter().all(|&k| c[usize::from(k)] > 19.0), "{c:?}");
}

#[test]
fn rate_adapter_on_the_weak_70cm_link() {
    for (label, cancel) in [("receiver without the line canceller", 0.0), ("with it", LINE_CANCEL_DB)] {
        let mut total = (0.0, 0.0);
        let mut bad_total = 0;
        for seed in [71u64, 72, 73] {
            let r = run(seed, cancel);
            for (i, (name, floor, comb)) in SETUPS.iter().enumerate() {
                let (kbps, n, bad, rungs) = &r.setups[i];
                let (best, best_mcs) = oracle(*floor, comb, cancel);
                eprintln!(
                    "{label}, seed {seed}, {name}: {kbps:.2} kbps over {n} frames ({bad} over 20% failed), \
                     rungs {:?}; oracle MCS {best_mcs} {best:.2} kbps",
                    rungs.iter().fold(vec![], |mut v: Vec<u8>, &m| {
                        if v.last() != Some(&m) {
                            v.push(m)
                        }
                        v
                    })
                );
                total.0 += kbps;
                total.1 += best;
                bad_total += bad;
            }
        }
        eprintln!("{label}: {:.0}% of the oracle, {bad_total} frames over 20% failed", 100.0 * total.0 / total.1);
        // The adapter must not keep stepping into rungs that fail: across
        // 3 seeds x 4 setups x 50 s (about 125 frames), two at most. It runs
        // a rung below the oracle where the SNR sits within the step-up
        // margin of the next rung's threshold (§10.1), so it gets 84% of the
        // oracle with the canceller, 67% without (2026-11-09).
        assert!(bad_total <= 2, "{label}: {bad_total} frames over 20% failed");
        let floor = if cancel > 0.0 { 0.8 } else { 0.6 };
        assert!(total.0 >= floor * total.1, "{label}: {:.2} of {:.2} kbps", total.0, total.1);
    }
}
