//! Carrier detect (SPEC 7.2, 14.1): false alarms on open-squelch noise,
//! quieting latency, AFSK detection and rejection, the preamble detector's
//! early confirmation, and chunking independence. The measurements are
//! printed (`cargo test --release --test dcd -- --nocapture`); the 10-minute
//! runs are `#[ignore]`d.

use cairn_phy::channel::Rng;
use cairn_phy::dcd::testsig::{
    Interferer, NoiseKind, OpenSquelchNoise, bell202, hdlc_bits, interferer, tone, voice_like,
};
use cairn_phy::dcd::{Dcd, DcdConfig, DcdEdge, DcdSource, DcdState, HOP, HeldDcd};
use cairn_phy::detector::{Detector, DetectorConfig};
use cairn_phy::{FrameConfig, Transmitter, params};

const FS: usize = 48_000;
/// Open-squelch noise level used throughout: -12 dBFS RMS (0.25).
const NOISE_RMS: f64 = 0.25;

fn run(dcd: &mut Dcd, x: &[f64], chunk: usize) -> Vec<DcdEdge> {
    x.chunks(chunk).flat_map(|c| dcd.process(c)).collect()
}

fn ups(edges: &[DcdEdge], source: DcdSource) -> Vec<usize> {
    edges.iter().filter(|e| e.source == source && e.up).map(|e| e.at).collect()
}

fn downs(edges: &[DcdEdge], source: DcdSource) -> Vec<usize> {
    edges.iter().filter(|e| e.source == source && !e.up).map(|e| e.at).collect()
}

fn ms(samples: f64) -> f64 {
    samples * 1000.0 / FS as f64
}

/// min / median / max.
fn summary(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!("{:.0}/{:.0}/{:.0}", v[0], v[v.len() / 2], v[v.len() - 1])
}

fn noise_variants(seed: u64) -> Vec<(&'static str, OpenSquelchNoise)> {
    vec![
        ("white 300-3000", OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed))),
        ("tilted 300-3000", OpenSquelchNoise::new(NoiseKind::Tilted, Rng::new(seed + 1))),
        ("white 200-4000", OpenSquelchNoise::wide(NoiseKind::White, Rng::new(seed + 2))),
        ("tilted 200-4000", OpenSquelchNoise::wide(NoiseKind::Tilted, Rng::new(seed + 3))),
    ]
}

/// Quieting and AFSK up edges, and early preamble confirmations, on
/// `minutes` of each open-squelch noise variant.
fn false_alarms(minutes: f64, seed: u64, preamble: bool) -> Vec<(&'static str, usize, usize, usize)> {
    let n = (minutes * 60.0 * FS as f64) as usize;
    let mut out = Vec::new();
    for (name, mut ns) in noise_variants(seed) {
        let mut dcd = Dcd::new(DcdConfig::default());
        let mut det = Detector::new(DetectorConfig::default());
        let (mut q, mut a, mut p) = (0, 0, 0);
        let mut chunk = vec![0.0; HOP];
        for _ in 0..n / HOP {
            ns.fill(&mut chunk);
            chunk.iter_mut().for_each(|v| *v *= NOISE_RMS);
            for e in dcd.process(&chunk) {
                match (e.source, e.up) {
                    (DcdSource::Quieting, true) => q += 1,
                    (DcdSource::Afsk, true) => a += 1,
                    _ => {}
                }
            }
            if preamble {
                det.process(&chunk);
                p += usize::from(det.take_early().is_some());
            }
        }
        assert!(dcd.floor_dbfs().is_some(), "{name}: quieting armed on open-squelch noise");
        println!(
            "{name:16} {minutes:.0} min: quieting {q}, AFSK {a}, preamble early {} (floor {:.1} dB)",
            if preamble { p.to_string() } else { "-".into() },
            dcd.floor_dbfs().unwrap()
        );
        out.push((name, q, a, p));
    }
    out
}

#[test]
fn quiet_on_open_squelch_noise() {
    for (name, q, a, p) in false_alarms(2.0, 11, true) {
        assert!(q == 0 && a == 0 && p == 0, "{name}: {q} quieting, {a} AFSK, {p} preamble false alarms in 2 min");
    }
}

/// SPEC 14.1: at most 1 false carrier detect per 10 minutes, per variant.
#[test]
#[ignore]
fn quiet_on_open_squelch_noise_10_minutes() {
    for seed in [101, 202] {
        for (name, q, a, p) in false_alarms(10.0, seed, true) {
            assert!(q + a + p <= 1, "{name}: {q} quieting, {a} AFSK, {p} preamble false alarms in 10 min");
        }
    }
}

/// Noise whose level steps by `step_db` up and down every `period_s`, or
/// drifts sinusoidally by +-`step_db` over `period_s` when `drift`.
fn level_changes(minutes: f64, step_db: f64, period_s: f64, drift: bool, kind: NoiseKind) -> usize {
    let n = (minutes * 60.0 * FS as f64) as usize;
    let mut ns = OpenSquelchNoise::new(kind, Rng::new(77));
    let mut dcd = Dcd::new(DcdConfig::default());
    let mut count = 0;
    let mut chunk = vec![0.0; HOP];
    for h in 0..n / HOP {
        let t = (h * HOP) as f64 / FS as f64;
        let db = if drift {
            step_db * (2.0 * std::f64::consts::PI * t / period_s).sin()
        } else if ((t / period_s) as usize) % 2 == 1 {
            -step_db
        } else {
            0.0
        };
        let g = NOISE_RMS * 10f64.powf(db / 20.0);
        ns.fill(&mut chunk);
        chunk.iter_mut().for_each(|v| *v *= g);
        count += ups(&dcd.process(&chunk), DcdSource::Quieting).len();
    }
    count
}

#[test]
fn quieting_level_changes() {
    // A slow drift must never trigger.
    let drift = level_changes(2.0, 3.0, 60.0, true, NoiseKind::White);
    println!("+-3 dB drift over 60 s, 2 min: {drift} quieting asserts");
    assert_eq!(drift, 0);
    // A 2 dB step down is absorbed.
    let small = level_changes(2.0, 2.0, 30.0, false, NoiseKind::White);
    println!("2 dB steps every 30 s, 2 min: {small} quieting asserts");
    assert_eq!(small, 0);
}

/// Receive AGC steps (SPEC 13.5): the gain moves 3 dB at a time between
/// frames and the modem tells carrier detect (`Dcd::level_step`) at the
/// moment it sets the mixer; the step reaches the samples a little later
/// (here 30 ms, the device's buffer). Returns quieting asserts.
fn agc_steps(notify: bool, step_db: f64, kind: NoiseKind) -> usize {
    let mut ns = OpenSquelchNoise::new(kind, Rng::new(91));
    let mut dcd = Dcd::new(DcdConfig::default());
    let mut chunk = vec![0.0; HOP];
    let mut gain_db = 0.0;
    let mut count = 0;
    let delay_hops = 3;
    let mut pending: Option<(usize, f64)> = None;
    for h in 0..(120 * FS / HOP) {
        // Every 5 s after the first 2: down, down, up, up, ...
        let t = h * HOP;
        if t >= 2 * FS && (t - 2 * FS) % (5 * FS) == 0 {
            let k = (t - 2 * FS) / (5 * FS);
            let d = if k % 4 < 2 { -step_db } else { step_db };
            if notify {
                dcd.level_step(d);
            }
            pending = Some((h + delay_hops, d));
        }
        if let Some((_, d)) = pending.take_if(|p| p.0 == h) {
            gain_db += d;
        }
        let g = NOISE_RMS * 10f64.powf(gain_db / 20.0);
        ns.fill(&mut chunk);
        chunk.iter_mut().for_each(|v| *v *= g);
        count += ups(&dcd.process(&chunk), DcdSource::Quieting).len();
    }
    count
}

#[test]
fn quieting_follows_agc_gain_steps() {
    for kind in [NoiseKind::White, NoiseKind::Tilted] {
        let told = agc_steps(true, 3.0, kind);
        let told6 = agc_steps(true, 6.0, kind);
        let blind = agc_steps(false, 6.0, kind);
        println!(
            "{kind:?}: 3 dB AGC steps told {told}, 6 dB told {told6}, 6 dB not told {blind} quieting asserts (2 min)"
        );
        assert_eq!(told + told6, 0, "{kind:?}: a gain step the detector was told about is no carrier");
        assert!(blind > 0, "{kind:?}: the test steps are big enough to matter");
    }
}

/// Steps of 1-4 dB down and back up every 30 s for 10 minutes (10 downward
/// steps), white and tilted noise.
#[test]
#[ignore]
fn quieting_level_steps_10_minutes() {
    for kind in [NoiseKind::White, NoiseKind::Tilted] {
        for step in [1.0, 2.0, 3.0, 4.0] {
            let c = level_changes(10.0, step, 30.0, false, kind);
            println!("{kind:?}: {step} dB steps every 30 s, 10 min: {c} quieting asserts");
        }
        let c = level_changes(10.0, 3.0, 60.0, true, kind);
        println!("{kind:?}: +-3 dB drift over 60 s, 10 min: {c} quieting asserts");
        assert_eq!(c, 0);
    }
}

fn cairn_frame(rng: &mut Rng, mcs: u8, blocks: usize) -> Vec<f64> {
    let cfg = FrameConfig::new(24, mcs, blocks).unwrap();
    let header = rng.bits(128);
    let payloads: Vec<Vec<u8>> = (0..blocks).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect();
    Transmitter::new().build(&cfg, &header, &payloads).output()
}

#[derive(Clone, Copy, Debug)]
enum Content {
    None,
    Frame,
    Voice,
}

/// Open-squelch noise, then a carrier for `carrier_s` (noise down by
/// `drop_db`, with `content` on top), then noise again. Returns the quieting
/// up latency after the drop and the down latency after the noise returns
/// (ms), or None for a miss.
fn carrier_trial(
    variant: usize,
    drop_db: f64,
    content: Content,
    carrier_s: f64,
    seed: u64,
) -> (Option<f64>, Option<f64>, (usize, f64)) {
    let mut rng = Rng::new(seed);
    let lead = 3 * FS + (seed as usize * 131) % HOP;
    let len = (carrier_s * FS as f64) as usize;
    let tail = 2 * FS;
    let mut ns = noise_variants(seed).swap_remove(variant).1;
    let mut x: Vec<f64> = ns.generate(lead + len + tail).iter().map(|v| v * NOISE_RMS).collect();
    let g = 10f64.powf(-drop_db / 20.0);
    x[lead..lead + len].iter_mut().for_each(|v| *v *= g);
    let on_top = match content {
        Content::None => Vec::new(),
        Content::Frame => cairn_frame(&mut rng, 3, 6),
        Content::Voice => voice_like(len, &mut rng).iter().map(|v| v * 0.1).collect(),
    };
    for (i, v) in on_top.iter().take(len).enumerate() {
        x[lead + i] += v;
    }
    let mut dcd = Dcd::new(DcdConfig::default());
    let edges = run(&mut dcd, &x, HOP);
    let u = ups(&edges, DcdSource::Quieting);
    let d = downs(&edges, DcdSource::Quieting);
    let up = u.iter().find(|&&t| t >= lead).map(|&t| ms((t - lead) as f64));
    let down = d.iter().find(|&&t| t >= lead + len).map(|&t| ms((t - (lead + len)) as f64));
    // Spurious edges: ups before the carrier, or extra releases inside it.
    // Spurious edges: ups before the carrier, and releases inside it (with
    // the longest dropout).
    let mut spurious = (u.iter().filter(|&&t| t < lead).count(), 0.0f64);
    for &t in d.iter().filter(|&&t| t > lead && t < lead + len) {
        spurious.0 += 1;
        let back = u.iter().find(|&&x| x > t).copied().unwrap_or(t).min(lead + len);
        spurious.1 = spurious.1.max(ms((back - t) as f64));
    }
    (up, down, spurious)
}

#[test]
fn quieting_carrier_latency() {
    let names = ["white 300-3000", "tilted 300-3000", "white 200-4000", "tilted 200-4000"];
    for (variant, name) in names.iter().enumerate() {
        for drop in [8.0, 10.0, 20.0] {
            for content in [Content::None, Content::Frame, Content::Voice] {
                let (mut ups, mut downs) = (Vec::new(), Vec::new());
                let (mut misses, mut spurious, mut dropout) = (0, 0, 0.0f64);
                for trial in 0..6 {
                    let (u, d, s) = carrier_trial(variant, drop, content, 1.2, 1000 + trial);
                    spurious += s.0;
                    dropout = dropout.max(s.1);
                    match (u, d) {
                        (Some(u), Some(d)) => {
                            ups.push(u);
                            downs.push(d);
                        }
                        _ => misses += 1,
                    }
                }
                println!(
                    "{name:16} drop {drop:2} dB + {content:5?}: up {} ms, down {} ms after noise returns; {misses} missed, {spurious} spurious (longest dropout {dropout:.0} ms)",
                    summary(&mut ups),
                    summary(&mut downs)
                );
                // A bare carrier: up within 70 ms, down within 45 ms (20 ms
                // typical); anything 20 dB deep is caught whatever it carries.
                if drop >= 10.0 && matches!(content, Content::None) {
                    assert_eq!(misses, 0, "{name} {drop} dB {content:?}");
                    assert!(ups.iter().all(|&u| u <= 70.0), "{name} {drop} dB {content:?}: up {ups:?}");
                    let bound = if drop >= 20.0 { 25.0 } else { 45.0 };
                    assert!(downs.iter().all(|&d| d <= bound), "{name} {drop} dB {content:?}: down {downs:?}");
                }
                if drop >= 20.0 {
                    assert_eq!(misses, 0, "{name} {drop} dB {content:?}");
                }
            }
        }
    }
}

#[test]
fn quieting_unarmed_without_open_squelch_noise() {
    // A sound card with no radio: -71 dBFS RMS noise and a -46 dBFS DC offset,
    // with the noise dropping and coming back.
    let mut ns = OpenSquelchNoise::wide(NoiseKind::White, Rng::new(5));
    let rms = 10f64.powf(-71.0 / 20.0);
    let dc = 10f64.powf(-46.0 / 20.0);
    let mut x: Vec<f64> = ns.generate(30 * FS).iter().map(|v| dc + rms * v).collect();
    for v in &mut x[10 * FS..12 * FS] {
        *v = dc + (*v - dc) * 0.01;
    }
    let mut dcd = Dcd::new(DcdConfig::default());
    let edges = run(&mut dcd, &x, HOP);
    assert!(ups(&edges, DcdSource::Quieting).is_empty());
    assert_eq!(dcd.floor_dbfs(), None);
    // Squelch closed then opened: arms once open-squelch noise appears.
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(6));
    let noise: Vec<f64> = ns.generate(3 * FS).iter().map(|v| v * NOISE_RMS).collect();
    dcd.process(&noise);
    assert!(dcd.floor_dbfs().is_some());
}

#[test]
fn quieting_mute() {
    // Noise, own transmission (muted: crosstalk of a Cairn frame, then
    // silence), unmute into noise: no assert. Then mute again and unmute
    // into a carrier (quieted noise): asserts.
    let mut rng = Rng::new(8);
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(9));
    let mut noise = |n: usize, g: f64| -> Vec<f64> { ns.generate(n).iter().map(|v| v * g).collect() };
    let mut dcd = Dcd::new(DcdConfig::default());
    let mut edges = run(&mut dcd, &noise(2 * FS, NOISE_RMS), HOP);
    let floor = dcd.floor_dbfs().unwrap();
    edges.extend(dcd.set_muted(true));
    let crosstalk: Vec<f64> = cairn_frame(&mut rng, 2, 4).iter().map(|v| v * 0.01).collect();
    edges.extend(run(&mut dcd, &crosstalk, HOP));
    edges.extend(run(&mut dcd, &vec![0.0; FS / 2], HOP));
    assert_eq!(dcd.floor_dbfs(), Some(floor), "no learning while muted");
    edges.extend(dcd.set_muted(false));
    edges.extend(run(&mut dcd, &noise(2 * FS, NOISE_RMS), HOP));
    assert!(ups(&edges, DcdSource::Quieting).is_empty(), "{edges:?}");
    assert!(!dcd.state().quieting);
    // Asserted, then muted: released at once.
    let pos = dcd.position();
    let e = run(&mut dcd, &noise(FS / 2, NOISE_RMS * 0.1), HOP);
    let up = ups(&e, DcdSource::Quieting);
    assert_eq!(up.len(), 1);
    println!("quieting up {:.0} ms after a 20 dB drop", ms((up[0] - pos) as f64));
    let rel = dcd.set_muted(true);
    assert_eq!(rel.len(), 1);
    assert!(!rel[0].up && rel[0].source == DcdSource::Quieting && !dcd.state().quieting);
    run(&mut dcd, &vec![0.0; FS / 2], HOP);
    // Unmute into a carrier: it reads as the radio's own transmit tail until
    // `recovery_max_s`, then asserts.
    dcd.set_muted(false);
    let pos = dcd.position();
    let e = run(&mut dcd, &noise(2 * FS, NOISE_RMS * 0.1), HOP);
    let up = ups(&e, DcdSource::Quieting);
    assert_eq!(up.len(), 1, "{e:?}");
    let after = ms((up[0] - pos) as f64);
    println!("unmuted into a carrier: quieting up {after:.0} ms after unmute");
    let max = DcdConfig::default().recovery_max_s * 1e3;
    assert!(after > max && after < max + 150.0, "{after}");
}

/// Noise, own PTT (muted), then the radio's own transmit tail (silence for
/// `tail_s` after unmute, the receiver deaf until the transmitter really
/// drops), then noise again; then a peer's carrier. Returns the quieting
/// rises during the tail and noise, the recovery reported, and the peer
/// carrier's up latency.
fn own_tail_trial(cfg: DcdConfig, tail_s: f64, seed: u64) -> (usize, Option<f64>, Option<f64>) {
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed));
    let mut noise = |n: usize, g: f64| -> Vec<f64> { ns.generate(n).iter().map(|v| v * g).collect() };
    let mut dcd = Dcd::new(cfg);
    run(&mut dcd, &noise(3 * FS, NOISE_RMS), HOP);
    dcd.set_muted(true);
    run(&mut dcd, &vec![0.0; FS], HOP);
    dcd.set_muted(false);
    let tail = (tail_s * FS as f64) as usize;
    // The transmitter's tail: near silence, then the key-down transient
    // (noise back over 20 ms, a few dB low at first).
    let mut x: Vec<f64> = noise(tail, NOISE_RMS * 0.003);
    let ramp = noise(FS / 50, NOISE_RMS);
    x.extend(ramp.iter().enumerate().map(|(i, v)| v * i as f64 / ramp.len() as f64));
    x.extend(noise(FS / 10, NOISE_RMS * 0.7));
    x.extend(noise(2 * FS, NOISE_RMS));
    let e = run(&mut dcd, &x, HOP);
    let rises = ups(&e, DcdSource::Quieting).len();
    let rec = dcd.take_recovery();
    let pos = dcd.position();
    let e = run(&mut dcd, &noise(FS / 2, NOISE_RMS * 0.03), HOP);
    let up = ups(&e, DcdSource::Quieting).first().map(|&t| ms((t - pos) as f64));
    (rises, rec, up)
}

#[test]
fn quieting_ignores_the_radios_own_transmit_tail() {
    // Measured on the air (2026-11-07): the receiver stays silent 0.64-0.73 s
    // after PTT release (the transmitter's tail), then the noise returns.
    let mut legacy_rises = 0;
    for (k, tail) in [0.0, 0.1, 0.3, 0.65, 0.73, 1.2].into_iter().enumerate() {
        let (rises, rec, up) = own_tail_trial(DcdConfig::default(), tail, 40 + k as u64);
        let (lr, _, _) = own_tail_trial(DcdConfig::default().legacy(), tail, 40 + k as u64);
        legacy_rises += lr;
        println!(
            "own tail {:.0} ms: {rises} rises (legacy {lr}), recovered after {:?} ms, a peer carrier then up after {:?} ms",
            tail * 1e3,
            rec.map(|r| (r * 1e3).round()),
            up.map(f64::round)
        );
        assert_eq!(rises, 0, "tail {tail}");
        let rec = rec.expect("recovery reported");
        assert!((rec - tail.max(0.05)).abs() < 0.04, "tail {tail}: recovery {rec}");
        assert!(up.is_some_and(|u| u <= 70.0), "tail {tail}: {up:?}");
    }
    assert!(legacy_rises >= 4, "the legacy detector read the tails as carriers: {legacy_rises}");
}

#[test]
fn quieting_carrier_end_does_not_chatter() {
    // A deep carrier (40 dB of quieting) whose end brings the noise back a
    // few dB low for 120 ms with 10 ms dips, as on the air: one rise per
    // carrier, released within 25 ms of the noise returning.
    let (mut new, mut old) = (0, 0);
    let mut downs_ms = Vec::new();
    for seed in 0..20u64 {
        let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(500 + seed));
        let mut x: Vec<f64> = ns.generate(3 * FS).iter().map(|v| v * NOISE_RMS).collect();
        let lead = x.len();
        x.extend(ns.generate(FS).iter().map(|v| v * NOISE_RMS * 0.01));
        let end = x.len();
        let mut back: Vec<f64> = ns.generate(FS / 8).iter().map(|v| v * NOISE_RMS * 0.7).collect();
        for d in [3, 6, 9] {
            back[d * HOP..(d + 1) * HOP].iter_mut().for_each(|v| *v *= 0.55);
        }
        x.extend(back);
        x.extend(ns.generate(2 * FS).iter().map(|v| v * NOISE_RMS));
        for legacy in [false, true] {
            let cfg = if legacy { DcdConfig::default().legacy() } else { DcdConfig::default() };
            let mut dcd = Dcd::new(cfg);
            let e = run(&mut dcd, &x, HOP);
            let u = ups(&e, DcdSource::Quieting);
            assert!(u.first().is_some_and(|&t| t > lead && t < lead + FS / 10), "{u:?}");
            if legacy {
                old += u.len();
            } else {
                new += u.len();
                let d = downs(&e, DcdSource::Quieting);
                downs_ms.push(ms((d[0] - end) as f64));
            }
        }
    }
    println!(
        "carrier ends with a dipping return: {new} rises in 20 carriers ({old} legacy); release {} ms",
        summary(&mut downs_ms)
    );
    assert_eq!(new, 20);
    assert!(old > new, "the legacy detector chattered here");
    assert!(downs_ms.iter().all(|&d| d <= 25.0), "{downs_ms:?}");
}

/// A Bell 202 burst: `flags` flags, a 30-byte frame, 2 flags.
fn afsk_burst(rng: &mut Rng, flags: usize, amplitude: f64, twist_db: f64) -> Vec<f64> {
    let payload: Vec<u8> = (0..30).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
    bell202(&hdlc_bits(&payload, flags, 2), amplitude, twist_db)
}

/// AFSK at `snr_db` (burst power over open-squelch noise power, the noise
/// continuing at the same level through the burst: a weak, unquieted
/// signal). Returns (up latency from burst start, release latency after its
/// end, drops inside the burst), ms.
fn afsk_trial(snr_db: f64, twist_db: f64, seed: u64) -> (Option<f64>, Option<f64>, usize) {
    let mut rng = Rng::new(seed);
    let s = afsk_burst(&mut rng, 30, 0.3, twist_db);
    let ps = s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64;
    let lead = FS + (seed as usize * 97) % HOP;
    let n = lead + s.len() + FS / 2;
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed + 1));
    let sd = (ps / 10f64.powf(snr_db / 10.0)).sqrt();
    let mut x: Vec<f64> = ns.generate(n).iter().map(|v| v * sd).collect();
    for (i, v) in s.iter().enumerate() {
        x[lead + i] += v;
    }
    let mut dcd = Dcd::new(DcdConfig::default());
    let edges = run(&mut dcd, &x, HOP);
    let (u, d) = (ups(&edges, DcdSource::Afsk), downs(&edges, DcdSource::Afsk));
    let end = lead + s.len();
    assert!(u.iter().all(|&t| t > lead), "AFSK before the burst");
    let up = u.first().map(|&t| ms((t - lead) as f64));
    let release = d.iter().find(|&&t| t >= end).map(|&t| ms((t - end) as f64));
    let drops = d.iter().filter(|&&t| t < end).count();
    (up, release, drops)
}

#[test]
fn afsk_detection_latency() {
    for snr in [30.0, 20.0, 15.0, 10.0] {
        for twist in [0.0, -6.0, 6.0, -9.0, 9.0] {
            let (mut up, mut rel) = (Vec::new(), Vec::new());
            let (mut misses, mut drops) = (0, 0);
            for trial in 0..10 {
                let (u, r, d) = afsk_trial(snr, twist, 500 + trial);
                drops += d;
                match u {
                    Some(u) => up.push(u),
                    None => misses += 1,
                }
                rel.extend(r);
            }
            println!(
                "AFSK {snr:2} dB twist {twist:+3}: up {} ms, release {} ms after the end; {misses} missed, {drops} drops",
                summary(&mut up),
                summary(&mut rel)
            );
            if twist.abs() <= 6.0 {
                assert_eq!(misses, 0, "{snr} dB twist {twist}");
                assert!(up.iter().all(|&u| u <= 100.0), "{snr} dB twist {twist}: {up:?}");
                assert!(rel.iter().all(|&r| r <= 40.0), "{snr} dB twist {twist}: {rel:?}");
            }
        }
    }
}

#[test]
fn afsk_after_open_squelch_noise() {
    // The usual case: open-squelch noise, then a packet station keys up
    // (the noise quiets by 20 dB) and sends flags at 25 dB SNR; then it
    // unkeys and the noise returns.
    let mut lat = Vec::new();
    let mut rel = Vec::new();
    for seed in 0..10u64 {
        let mut rng = Rng::new(seed + 40);
        let s = afsk_burst(&mut rng, 25, 0.3, if seed % 2 == 0 { 0.0 } else { 6.0 });
        let lead = 2 * FS + (seed as usize * 61) % HOP;
        let mut ns = OpenSquelchNoise::new(NoiseKind::Tilted, Rng::new(seed));
        let mut x: Vec<f64> = ns.generate(lead + s.len() + FS).iter().map(|v| v * NOISE_RMS).collect();
        for (i, v) in s.iter().enumerate() {
            x[lead + i] = x[lead + i] * 0.1 + v;
        }
        let mut dcd = Dcd::new(DcdConfig::default());
        let edges = run(&mut dcd, &x, HOP);
        let u = ups(&edges, DcdSource::Afsk);
        assert_eq!(u.len(), 1, "{edges:?}");
        lat.push(ms((u[0] - lead) as f64));
        let end = lead + s.len();
        let d = downs(&edges, DcdSource::Afsk);
        rel.push(ms((d[0] - end) as f64));
    }
    println!("AFSK after open-squelch noise: up {} ms, release {} ms", summary(&mut lat), summary(&mut rel));
    assert!(lat.iter().all(|&l| l <= 100.0));
}

fn afsk_ups(x: &[f64]) -> usize {
    let mut dcd = Dcd::new(DcdConfig::default());
    ups(&run(&mut dcd, x, HOP), DcdSource::Afsk).len()
}

#[test]
fn afsk_rejects_other_signals() {
    // Voice-like audio, alone and over noise.
    for seed in 0..2u64 {
        let mut rng = Rng::new(300 + seed);
        let v: Vec<f64> = voice_like(60 * FS, &mut rng).iter().map(|v| v * 0.1).collect();
        assert_eq!(afsk_ups(&v), 0, "voice seed {seed}");
        let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed));
        let vn: Vec<f64> = v.iter().map(|s| s + 0.01 * ns.sample()).collect();
        assert_eq!(afsk_ups(&vn), 0, "voice + noise seed {seed}");
    }
    // Cairn OFDM frames at every rung, over light noise.
    let mut rng = Rng::new(31);
    let mut x = Vec::new();
    for mcs in 0..=6u8 {
        x.extend(cairn_frame(&mut rng, mcs, 4));
        x.extend(vec![0.0; FS / 10]);
    }
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(32));
    let x: Vec<f64> = x.iter().map(|v| v + 0.003 * ns.sample()).collect();
    assert_eq!(afsk_ups(&x), 0, "OFDM");
    // Tones, DTMF, CW, alone and over noise; steady 1200 and 2200 Hz tones.
    for k in Interferer::ALL {
        for tnr_db in [0.0, 10.0, 40.0] {
            let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(33));
            let a = NOISE_RMS * 10f64.powf(tnr_db / 20.0);
            let x: Vec<f64> = interferer(k, 0, 30 * FS).iter().map(|v| a * v + NOISE_RMS * ns.sample()).collect();
            assert_eq!(afsk_ups(&x), 0, "{k:?} at {tnr_db} dB over noise");
        }
    }
    for f in [1200.0, 2200.0] {
        assert_eq!(afsk_ups(&tone(f, 0.3, 10 * FS)), 0, "{f} Hz tone");
    }
}

/// Voice-like audio for `minutes` (several seeds) and the simulator's
/// interferers over noise: AFSK must stay quiet.
#[test]
#[ignore]
fn afsk_rejects_voice_10_minutes() {
    let mut total = 0;
    for seed in 0..4u64 {
        let mut rng = Rng::new(700 + seed);
        let v: Vec<f64> = voice_like(150 * FS, &mut rng).iter().map(|v| v * 0.1).collect();
        let a = afsk_ups(&v);
        let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed));
        let vn: Vec<f64> = v.iter().map(|s| s + 0.02 * ns.sample()).collect();
        let b = afsk_ups(&vn);
        println!("voice seed {seed}, 2.5 min: {a} AFSK; over noise: {b}");
        total += a + b;
    }
    println!("voice-like, 10 min alone + 10 min over noise: {total} AFSK asserts");
    assert_eq!(total, 0);
}

/// Result of [`early_trial`].
struct EarlyTrial {
    /// take_early's index and the samples fed after the frame start when it
    /// returned it.
    early: Option<(usize, usize)>,
    /// Further take_early returns during the frame.
    extra: usize,
    /// The final detection's start and the samples fed after the frame start
    /// when it was returned.
    detection: Option<(usize, usize)>,
    lead: usize,
}

/// A frame at `snr_db` (frame mean power over white open-squelch noise
/// power) after a second of noise, fed in 10 ms chunks.
fn early_trial(snr_db: f64, seed: u64) -> EarlyTrial {
    let mut rng = Rng::new(seed);
    let frame = cairn_frame(&mut rng, 1, 3);
    let pf = frame.iter().map(|v| v * v).sum::<f64>() / frame.len() as f64;
    let lead = FS + (seed as usize * 37) % HOP;
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(seed + 9));
    let sd = (pf / 10f64.powf(snr_db / 10.0)).sqrt();
    let mut x: Vec<f64> = ns.generate(lead + frame.len() + FS / 2).iter().map(|v| v * sd).collect();
    for (i, v) in frame.iter().enumerate() {
        x[lead + i] += v;
    }
    let mut det = Detector::new(DetectorConfig::default());
    let mut t = EarlyTrial { early: None, extra: 0, detection: None, lead };
    for c in x.chunks(HOP) {
        let d = det.process(c);
        if let Some(e) = det.take_early() {
            if t.early.is_none() {
                t.early = Some((e, det.position() - lead));
            } else {
                t.extra += 1;
            }
        }
        if let Some(d) = d.first()
            && t.detection.is_none()
        {
            t.detection = Some((d.start, det.position() - lead));
        }
    }
    t
}

#[test]
fn preamble_early_confirmation() {
    for snr in [20.0, 10.0, 5.0, 2.0] {
        let (mut el, mut dl) = (Vec::new(), Vec::new());
        let mut misses = 0;
        let mut extra = 0;
        for seed in 0..8u64 {
            let t = early_trial(snr, 60 + seed);
            extra += t.extra;
            match (t.early, t.detection) {
                (Some((e, el_)), Some((s, dl_))) => {
                    // The early index is the coarse frame start.
                    assert!((e as i64 - t.lead as i64).abs() <= 24, "early {e} vs lead {}", t.lead);
                    assert!((s as i64 - t.lead as i64).abs() <= 2);
                    el.push(ms(el_ as f64));
                    dl.push(ms(dl_ as f64));
                }
                (None, Some(_)) => panic!("{snr} dB: detection without early confirmation"),
                _ => misses += 1,
            }
        }
        println!(
            "preamble at {snr:2} dB: take_early {} ms after the frame start, final detection {} ms ({misses} missed, {extra} repeats)",
            summary(&mut el),
            summary(&mut dl)
        );
        if snr >= 10.0 {
            assert_eq!(misses, 0);
            assert!(el.iter().all(|&l| l <= 60.0), "{el:?}");
        }
    }
}

#[test]
fn chunking_independence() {
    // Noise, an AFSK burst, a quieting carrier with a frame, noise.
    let mut rng = Rng::new(3);
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(4));
    let mut x: Vec<f64> = ns.generate(6 * FS).iter().map(|v| v * NOISE_RMS).collect();
    let burst = afsk_burst(&mut rng, 20, 0.3, 3.0);
    for (i, v) in burst.iter().enumerate() {
        x[FS + 123 + i] = x[FS + 123 + i] * 0.1 + v;
    }
    let frame = cairn_frame(&mut rng, 2, 2);
    for (i, v) in frame.iter().enumerate() {
        x[3 * FS + 7 + i] = x[3 * FS + 7 + i] * 0.1 + v;
    }
    let reference = run(&mut Dcd::new(DcdConfig::default()), &x, HOP);
    assert!(
        ups(&reference, DcdSource::Afsk).len() == 1 && ups(&reference, DcdSource::Quieting).len() >= 2,
        "{reference:?}"
    );
    for chunk in [1, 7, 4096, 48_001] {
        assert_eq!(run(&mut Dcd::new(DcdConfig::default()), &x, chunk), reference, "chunk {chunk}");
    }
}

#[test]
fn caller_driven_detectors_and_hold() {
    let mut dcd = Dcd::new(DcdConfig::default());
    assert!(!dcd.raw());
    let e = dcd.set_preamble(true, 100).unwrap();
    assert_eq!(e, DcdEdge { at: 100, source: DcdSource::Preamble, up: true });
    assert_eq!(dcd.set_preamble(true, 110), None);
    assert!(dcd.raw() && dcd.state().preamble);
    assert_eq!(dcd.set_cos(None, 120), None);
    assert!(dcd.set_cos(Some(true), 130).unwrap().up);
    assert!(!dcd.set_cos(None, 140).unwrap().up);
    assert!(!dcd.set_preamble(false, 150).unwrap().up);
    assert!(!dcd.raw());

    let mut held = HeldDcd::new(4800);
    assert!(!held.held(0));
    held.update(DcdState { afsk: true, ..Default::default() }, 1000);
    assert!(held.held(1000) && held.held(50_000));
    held.update(DcdState::default(), 2000);
    assert!(held.held(6799) && !held.held(6800));
    held.update(DcdState::default(), 7000);
    assert!(!held.held(7000));
}

#[test]
fn quieting_band_for_width() {
    assert_eq!(DcdConfig::for_width(24).quieting_band_hz, (2800.0, 3200.0));
    assert_eq!(DcdConfig::for_width(32).quieting_band_hz, (3600.0, 4150.0));
    assert_eq!(DcdConfig::for_width(56).quieting_band_hz, (6000.0, 7000.0));
    // Every width: 100 Hz above the highest tone's edge (centre + 50 Hz).
    for w in 24..=56 {
        let lo = DcdConfig::for_width(w).quieting_band_hz.0;
        assert_eq!(lo, cairn_phy::params::band_high_hz(w) + 100.0, "width {w}");
    }
    assert_eq!(DcdConfig::default(), DcdConfig::for_width(24));
}

#[test]
fn processing_cost() {
    let mut ns = OpenSquelchNoise::new(NoiseKind::White, Rng::new(1));
    let x: Vec<f64> = ns.generate(60 * FS).iter().map(|v| v * NOISE_RMS).collect();
    let mut dcd = Dcd::new(DcdConfig::default());
    let t = std::time::Instant::now();
    run(&mut dcd, &x, HOP);
    let per_s = t.elapsed().as_secs_f64() / 60.0;
    println!("Dcd::process: {:.2} ms of CPU per second of audio", per_s * 1e3);
}
