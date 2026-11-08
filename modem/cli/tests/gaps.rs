//! Capture gaps (lost audio, as in the first on-air test's 26.6 ms xrun
//! inside a frame): a mock capture device drops buffers at chosen points;
//! the capture path puts exactly as many zeros in their place, so the frames
//! before and after still decode, at their true sample positions; the frame
//! across a gap is flagged `audio_gap`; a header lost in a gap is a header
//! failure (flagged) toward the link layer, nothing more.

use cairn_audio::{InputStream, MockCapture};
use cairn_link::{Config, Instant};
use cairn_modem::headers::{HeaderSpec, build_header_frame};
use cairn_modem::listen::{self, HeaderFirst, InputSource, ListenOptions, Output as LOut, header_samples};
use cairn_modem::runtime::{Modem, ModemConfig, Output};
use cairn_phy::channel::Rng;
use cairn_phy::detector::DetectorConfig;
use cairn_phy::fields::{Address, frame_type};
use cairn_phy::params::{CP_SHORT, N_USEFUL, PREAMBLE_SYMBOLS};
use cairn_phy::{LinkState, RxOptions, Transmitter};

const FS: usize = 48_000;
/// Frame 4 loses its audio from just after the preamble to the end of the header.
const HEADER_FROM: usize = PREAMBLE_SYMBOLS * (N_USEFUL + CP_SHORT) + CP_SHORT;

/// Five UI broadcasts in noise; returns the audio and each frame's (start, length).
fn air() -> (Vec<f32>, Vec<(usize, usize)>) {
    let me = Address::new("N0GAP", 1).unwrap();
    let mut tx = Transmitter::new();
    let mut rng = Rng::new(17);
    let mut x: Vec<f64> = vec![0.0; FS / 2];
    let mut at = Vec::new();
    for k in 0..5u16 {
        let spec = HeaderSpec::broadcast(me.clone(), frame_type::UI, k, 24, 2, 3);
        let (f, _) = build_header_frame(&mut tx, &spec, 100 + k as u64).unwrap();
        let y = f.output();
        at.push((x.len(), y.len()));
        x.extend(y.iter().map(|v| v * 0.5));
        x.extend(std::iter::repeat_n(0.0, FS * 2 / 5));
    }
    x.extend(std::iter::repeat_n(0.0, FS / 2));
    for v in &mut x {
        *v += 1e-3 * rng.normal();
    }
    (x.iter().map(|&v| v as f32).collect(), at)
}

/// The buffers lost: 26.6 ms inside frame 1's data, 50 ms between frames 2
/// and 3, and frame 4's whole header (after its preamble).
fn drops(at: &[(usize, usize)]) -> Vec<(u64, u64)> {
    let (s1, l1) = at[1];
    let (s2, l2) = at[2];
    let (s4, _) = at[4];
    let h = header_samples(CP_SHORT);
    vec![
        ((s1 + l1 * 7 / 10) as u64, 1_277),
        ((s2 + l2 + FS / 10) as u64, 2_400),
        ((s4 + HEADER_FROM) as u64, (h - 64 - HEADER_FROM) as u64),
    ]
}

fn capture(x: Vec<f32>, drops: Vec<(u64, u64)>) -> InputStream {
    let mut m = MockCapture::new(x);
    m.drops = drops;
    m.ring_seconds = 1.0;
    InputStream::mock(m).unwrap()
}

#[test]
fn listener_keeps_timing_across_gaps_and_flags_the_gapped_frame() {
    let (x, at) = air();
    let d = drops(&at);
    let mut src = InputSource::new(capture(x.clone(), d.clone()), None);
    let link = LinkState { width: 24, ..Default::default() };
    let opts = ListenOptions {
        detector: DetectorConfig::default(),
        rx: RxOptions { threads: 2, ..Default::default() },
        stats_every: 0,
        boost: false,
        rt: cairn_audio::RtConfig { enabled: false, ..Default::default() },
    };
    let mut frames = Vec::new();
    let mut unresolved = Vec::new();
    let totals =
        listen::run(&mut src, Box::new(HeaderFirst::new(link, RxOptions::default())), opts, &mut |o| match o {
            LOut::Frame(f) => frames.push(*f),
            LOut::Unresolved(det, why, gap) => unresolved.push((det, why, gap)),
            _ => {}
        });
    assert_eq!(totals.samples, x.len(), "the stream has every sample period");
    let st = src.input.stats();
    let lost = d.iter().map(|g| g.1).sum::<u64>();
    assert_eq!((st.gaps, st.filled_samples), (3, lost));

    for f in &frames {
        println!("{:7} {}", f.det.start, f.line());
    }
    for (det, why, gap) in &unresolved {
        println!("{:7} not decoded: {why} {gap:?}", det.start);
    }
    // Frames 0, 1, 2, 3 decode; their starts are where they were sent, the
    // ones after the gaps included (without the zeros they would be early by
    // 1277 and 3677 samples).
    assert_eq!(frames.len(), 4, "frames 0-3 decode, frame 4's header was lost");
    for (k, f) in frames.iter().enumerate() {
        let (s, _) = at[k];
        assert!(f.det.start.abs_diff(s) <= 4, "frame {k} at {} (sent at {s})", f.det.start);
        assert!(f.rx.header_ok);
        assert_eq!(f.rx.header.as_ref().unwrap().sequence, k as u16);
    }
    for k in [0, 2, 3] {
        assert!(frames[k].complete(), "frame {k} complete");
        assert!(frames[k].gap.is_none(), "frame {k} not flagged");
    }
    // Frame 1 carries the gap: where it is and how long.
    let g = frames[1].gap.expect("frame 1 flagged audio_gap");
    assert_eq!(g.samples, 1_277);
    assert_eq!(g.offset, d[0].0 as usize - at[1].0);
    assert!((g.ms() - 26.6).abs() < 0.05);
    assert!(frames[1].line().contains("AUDIO GAP 26.6 ms"), "{}", frames[1].line());
    // Frame 4: its header fell in a gap; flagged too.
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    let (det, _, gap) = &unresolved[0];
    assert!(det.start.abs_diff(at[4].0) <= 4);
    assert_eq!(gap.expect("flagged").samples as u64, d[2].1);
    assert_eq!(totals.gapped, 2);
}

#[test]
fn modem_flags_gapped_frames_and_header_failures() {
    let (x, at) = air();
    let d = drops(&at);
    let mut input = capture(x.clone(), d.clone());
    let mut c = Config::for_callsign("N0RX", 0);
    c.seed = 3;
    let mut cfg = ModemConfig::new(c);
    cfg.rx.threads = 1;
    cfg.ring_seconds = 0.0;
    let mut modem = Modem::new(cfg);
    let mut buf = vec![0f32; 480];
    let mut chunk: Vec<f64> = Vec::with_capacity(480);
    let mut pos = 0usize;
    let mut outs = Vec::new();
    let t0 = std::time::Instant::now();
    while !input.ended() && t0.elapsed().as_secs() < 60 {
        let n = input.read(&mut buf[..480 - chunk.len()]).unwrap();
        chunk.extend(buf[..n].iter().map(|&v| v as f64));
        for g in input.take_gaps() {
            modem.note_gap(g.at as usize, g.samples as usize);
        }
        if chunk.len() == 480 {
            let t = Instant((pos as u64 * 1_000_000) / FS as u64);
            modem.push_audio(&chunk, t, t);
            pos += 480;
            chunk.clear();
            outs.extend(modem.drain());
        } else if n == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert_eq!(pos, x.len() / 480 * 480);
    let heard: Vec<_> = outs
        .iter()
        .filter_map(|o| match o {
            Output::Heard(f) => Some(f),
            _ => None,
        })
        .collect();
    let failed: Vec<_> = outs
        .iter()
        .filter_map(|o| match o {
            Output::HeaderFailed { at, audio_gap } => Some((*at, *audio_gap)),
            _ => None,
        })
        .collect();
    assert_eq!(heard.len(), 4);
    for (k, f) in heard.iter().enumerate() {
        assert_eq!(f.header.seq.0, k as u16);
        // Times on the modem's clock follow the sample positions.
        let end_s = (at[k].0 + at[k].1) as f64 / FS as f64;
        assert!((f.ended_at.as_secs_f64() - end_s).abs() < 0.002, "frame {k} ends at {} ({end_s})", f.ended_at);
        assert_eq!(f.audio_gap.is_some(), k == 1, "only frame 1 is flagged");
    }
    assert!(heard[1].line().contains("AUDIO GAP 26.6 ms"), "{}", heard[1].line());
    assert_eq!(failed.len(), 1, "frame 4's header: a header failure");
    assert_eq!(failed[0].1.expect("flagged").samples as u64, d[2].1);
    assert!((failed[0].0.as_secs_f64() - at[4].0 as f64 / FS as f64).abs() < 0.002);
    let s = &modem.stats;
    assert_eq!((s.audio_gaps, s.audio_gap_samples, s.gapped_frames), (3, d.iter().map(|g| g.1).sum(), 2));
    assert_eq!(s.header_failures, 1);
}
