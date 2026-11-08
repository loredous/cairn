//! Two complete modem runtimes (real PHY, link layer and carrier detect) on
//! a simulated audio channel (`cairn_modem::twosim`), on a virtual clock.
//! `SIM_TRACE=1` prints every frame. The figures these tests print are the
//! ones in `modem/README.md`.

use std::time::Duration;

use cairn_link::linktest::ProbeKind;
use cairn_link::pdu::pdu_type;
use cairn_link::{Config, ConnectFailure, FrameType, HostCommand, HostEvent, Instant, Pdu};
use cairn_modem::control::HostState;
use cairn_modem::kiss::ax25_pdu;
use cairn_modem::runtime::{DecodeTime, ModemConfig};
use cairn_modem::twosim::{Sim, Source, percentile_ms};
use cairn_phy::dcd::testsig::{bell202, hdlc_bits};
use cairn_phy::params::net_bps;

fn t(s: f64) -> Instant {
    Instant::from_secs_f64(s)
}

fn station(call: &str, seed: u64) -> ModemConfig {
    let mut c = Config::for_callsign(call, 0);
    c.seed = seed;
    let mut m = ModemConfig::new(c);
    m.rx.threads = 1;
    m
}

fn pair(snr: f64, seed: u64) -> Sim {
    pair_ppm(snr, seed, 0.0, 0.0)
}

fn pair_ppm(snr: f64, seed: u64, ppm_a: f64, ppm_b: f64) -> Sim {
    let mut sim = Sim::new();
    sim.add(station("N0AAA", seed), ppm_a, snr, seed);
    sim.add(station("N0BBB", seed + 1), ppm_b, snr, seed + 1);
    sim
}

fn payload(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Queue `data` at station `st` as FILE-CHUNK PDUs, as `send @file` does.
fn send_file(sim: &mut Sim, st: usize, at: Instant, data: &[u8]) {
    let mut h = HostState::new();
    let (cmds, _, _) = h.file_commands(data);
    for c in cmds {
        sim.host(st, at, c);
    }
}

fn connect(sim: &mut Sim, a: usize, b: usize, at: Instant) {
    let peer = sim.addr(b);
    sim.host(a, at, HostCommand::Connect { peer, new_session: false });
}

/// (key time, MCS, data blocks) of a station's DATA frames.
fn data_frames(sim: &Sim, st: usize) -> Vec<(Instant, u8, usize)> {
    sim.frames_from(st)
        .iter()
        .filter_map(|a| {
            a.frame.as_ref().filter(|f| f.ftype == FrameType::Data).map(|f| (a.key_time(), f.mcs, f.data.len()))
        })
        .collect()
}

/// Net payload rate of a transfer, kbps: bytes over the time from the
/// first DATA frame's keying to the last byte's delivery.
fn net_kbps(sim: &Sim, from: usize, to: usize) -> f64 {
    let first = data_frames(sim, from).first().map(|x| x.0).expect("data frames");
    let last = sim.stations[to].delivered.last().map(|x| x.0).expect("delivered");
    let bytes = sim.stations[to].delivered_bytes().len();
    bytes as f64 * 8.0 / (last - first).as_secs_f64() / 1000.0
}

fn a2_kbps(mcs: u8) -> f64 {
    net_bps(mcs, 24, if mcs <= 1 { 2.0 } else { 4.0 }) / 1000.0
}

fn mode_mcs(frames: &[(Instant, u8, usize)]) -> u8 {
    let mut n = [0usize; 7];
    for f in frames {
        n[f.1 as usize] += 1;
    }
    (0..7).max_by_key(|&m| n[m]).unwrap() as u8
}

fn assert_no_overlaps(sim: &Sim) {
    let o = sim.overlaps();
    for &(i, j) in &o {
        let (a, b) = (&sim.air[i], &sim.air[j]);
        eprintln!(
            "overlap: {:?} keyed at {} while {:?} on air {}..{}",
            a.from,
            a.key_time(),
            b.from,
            b.key_time(),
            b.end_time()
        );
    }
    assert!(o.is_empty(), "a station keyed over a carrier it could hear");
}

#[test]
fn connect_transfer_50k_each_way_and_disconnect() {
    // Clock offsets of +60 and -40 ppm (100 ppm apart, SPEC 14.1's tolerance).
    let mut sim = pair_ppm(25.0, 10, 60.0, -40.0);
    let ab = payload(50_000, 1);
    let ba = payload(50_000, 2);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &ab);
    send_file(&mut sim, 1, t(3.0), &ba);
    let done = |s: &Sim| {
        s.stations[1].delivered_bytes().len() >= ab.len() && s.stations[0].delivered_bytes().len() >= ba.len()
    };
    sim.run_while(t(400.0), |s| !done(s));
    let end = sim.now();
    assert_eq!(sim.stations[1].delivered_bytes(), ab, "A to B: every byte, once, in order");
    assert_eq!(sim.stations[0].delivered_bytes(), ba, "B to A: every byte, once, in order");
    sim.host(0, end + Duration::from_secs(1), HostCommand::Disconnect { graceful: true });
    sim.run_until(end + Duration::from_secs(30));
    assert!(sim.stations[0].has(|e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(sim.stations[1].has(|e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(!sim.stations[0].has(|e| matches!(e, HostEvent::Undelivered { .. })));
    let (fa, fb) = (data_frames(&sim, 0), data_frames(&sim, 1));
    let ta = sim.turnarounds();
    eprintln!(
        "50 kB each way at 25 dB, +60/-40 ppm: done at {end}; A->B {:.2} kbps, B->A {:.2} kbps (sum {:.2}); \
DATA frames A {} at MCS {}, B {} at MCS {}; dup blocks {} / {}; turnaround p50 {:.1} p99 {:.1} max {:.1} ms ({} replies)",
        net_kbps(&sim, 0, 1),
        net_kbps(&sim, 1, 0),
        net_kbps(&sim, 0, 1) + net_kbps(&sim, 1, 0),
        fa.len(),
        mode_mcs(&fa),
        fb.len(),
        mode_mcs(&fb),
        sim.stations[0].modem.stats.dup_blocks,
        sim.stations[1].modem.stats.dup_blocks,
        percentile_ms(&ta, 50.0),
        percentile_ms(&ta, 99.0),
        percentile_ms(&ta, 100.0),
        ta.len()
    );
    assert_eq!(sim.stations[0].modem.stats.dup_blocks + sim.stations[1].modem.stats.dup_blocks, 0);
    assert!(percentile_ms(&ta, 100.0) <= 100.0, "turn-holder replies within 100 ms of the peer's last sample");
    assert_no_overlaps(&sim);
}

#[test]
fn one_way_throughput_against_appendix_a() {
    // 50 kB one way at several SNRs; the settled rung's Appendix A.2 rate
    // assumes 4 s frames (2 s at MCS 0-1), TXDelay 150 ms, 0.51 s peer cycle.
    let mut rows = Vec::new();
    for (i, snr) in [8.0, 14.0, 20.0, 25.0, 32.0].into_iter().enumerate() {
        let mut sim = pair(snr, 20 + i as u64);
        let data = payload(if snr < 10.0 { 15_000 } else { 50_000 }, 3);
        connect(&mut sim, 0, 1, t(0.5));
        send_file(&mut sim, 0, t(0.5), &data);
        sim.run_while(t(600.0), |s| s.stations[1].delivered_bytes().len() < data.len());
        assert_eq!(sim.stations[1].delivered_bytes(), data, "{snr} dB");
        let f = data_frames(&sim, 0);
        let mcs = mode_mcs(&f);
        let got = net_kbps(&sim, 0, 1);
        let a2 = a2_kbps(mcs);
        let blocks: usize = f.iter().map(|x| x.2).sum();
        let lost: u64 = sim.stations[1].modem.stats.blocks - sim.stations[1].modem.stats.blocks_ok;
        eprintln!(
            "{snr:4.1} dB: MCS {mcs} ({} frames, {blocks} blocks sent, {lost} lost), {got:.2} kbps net, A.2 {a2:.2} kbps ({:.0}%)",
            f.len(),
            100.0 * got / a2
        );
        rows.push((snr, mcs, got, a2));
        assert_no_overlaps(&sim);
    }
    // The minimum usable link: >= 1.2 kbps at 8 dB (SPEC 14.1), allowing
    // for the transfer's start-up on a short run.
    assert!(rows[0].2 >= 0.9, "{rows:?}");
    // 24 tones at 25 dB: SPEC 14.1 asks for 5.0 kbps.
    assert!(rows[3].2 >= 5.0, "{rows:?}");
    for r in &rows[1..] {
        assert!(r.2 >= 0.75 * r.3, "{r:?}");
    }
}

#[test]
fn rung_follows_snr_steps() {
    let mut sim = pair(25.0, 30);
    let data = payload(200_000, 4);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    sim.snr_step(t(40.0), 0, 1, 19.0);
    sim.snr_step(t(80.0), 0, 1, 25.0);
    sim.run_until(t(125.0));
    let f = data_frames(&sim, 0);
    let rungs = |a: f64, b: f64| -> Vec<u8> { f.iter().filter(|x| x.0 >= t(a) && x.0 < t(b)).map(|x| x.1).collect() };
    let (before, down, up) = (rungs(20.0, 40.0), rungs(40.0, 80.0), rungs(80.0, 125.0));
    eprintln!("25 dB {before:?}\n19 dB {down:?}\n25 dB {up:?}");
    let hi = *before.last().unwrap();
    let lo = *down.last().unwrap();
    assert!(lo < hi, "stepped down");
    // Settled within 2 frames down and 4 up (ADR 0013 targets).
    let settle = |v: &[u8]| v.iter().rposition(|&m| m != *v.last().unwrap()).map_or(1, |i| i + 2);
    assert!(settle(&down) <= 2, "{down:?}");
    // Back up to the rung three clean frames 2 dB clear of its threshold
    // support (MCS 5 came from the 3 dB reply rule at setup; the run's
    // lowest measured SNR may hold the climb one rung below it).
    assert!(*up.last().unwrap() > lo && *up.last().unwrap() + 1 >= hi, "back up");
    assert!(settle(&up) <= 4, "{up:?}");
    assert_no_overlaps(&sim);
}

#[test]
fn bell202_burst_during_a_session_is_deferred_to() {
    let mut sim = pair(25.0, 40);
    let data = payload(40_000, 5);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    // A packet station keys after 20 s, in the first gap between the
    // session's transmissions (10 ms after a release): 300 ms of flags (its
    // TXDelay), a 60-byte frame, heard by both stations at 20 dB.
    let frame: Vec<u8> = (0..60u8).collect();
    let audio = bell202(&hdlc_bits(&frame, 45, 3), 0.3, 0.0);
    let burst_len = Duration::from_secs_f64(audio.len() as f64 / 48_000.0);
    sim.run_until(t(20.0));
    sim.run_while(t(40.0), |s| !s.stations.iter().any(|st| st.modem.keyed()));
    sim.run_while(t(40.0), |s| s.stations.iter().any(|st| st.modem.keyed()));
    let at = sim.now() + Duration::from_millis(10);
    sim.foreign(at, Duration::from_millis(30), audio, Duration::from_millis(20), 20.0);
    sim.run_while(t(300.0), |s| s.stations[1].delivered_bytes().len() < data.len());
    assert_eq!(sim.stations[1].delivered_bytes(), data);
    let afsk: Vec<u64> = sim.stations.iter().map(|s| s.modem.stats.dcd_rises[2]).collect();
    let burst = sim.air.iter().find(|a| a.from == Source::Foreign).unwrap();
    let (b0, b1) = (burst.key_time(), t_end(burst.release));
    // When each station first heard the burst on any detector: at its start
    // if a detector was already up then.
    let detected: Vec<Instant> = sim
        .stations
        .iter()
        .map(|st| {
            let up_at_start = st.dcd.iter().rfind(|(at, _)| *at <= b0).is_some_and(|(_, s)| s.any());
            if up_at_start { b0 } else { st.dcd.iter().find(|(at, s)| *at > b0 && s.any()).map_or(b1, |(at, _)| *at) }
        })
        .collect();
    // No station starts a transmission once it has detected the packet
    // station, until 100 ms after it unkeys (SPEC 14.1). A start before any
    // detector could respond is a collision carrier sense cannot prevent: the
    // burst begins in the 50 ms turn-holder window (ADR 0010) and quieting
    // needs about 50 ms to see a new carrier, AFSK 80-90 ms. Where the burst
    // falls in that window depends on the frame schedule, so such a start is
    // allowed, but only within 60 ms of the burst's start.
    let starts: Vec<(Source, Instant)> = sim
        .air
        .iter()
        .filter(|a| a.from != Source::Foreign && a.key_time() >= b0 && a.key_time() < b1 + Duration::from_millis(100))
        .map(|a| (a.from, a.key_time()))
        .collect();
    let undetected = |src: &Source, at: &Instant| match src {
        Source::Station(i) => *at < detected[*i] && *at <= b0 + Duration::from_millis(60),
        Source::Foreign => false,
    };
    let violations: Vec<&(Source, Instant)> = starts.iter().filter(|(src, at)| !undetected(src, at)).collect();
    let first_after =
        sim.air.iter().filter(|a| a.from != Source::Foreign && a.key_time() >= b1).map(|a| a.key_time()).min();
    eprintln!(
        "Bell 202 burst {b0}..{b1} ({:.0} ms of audio): AFSK detected by A/B {afsk:?}, first detection by A/B {detected:?}; \
transmissions started in it: {starts:?}; first after it at {:?} ({:.0} ms after release)",
        burst_len.as_secs_f64() * 1e3,
        first_after,
        first_after.map_or(f64::NAN, |f| (f - b1).as_secs_f64() * 1e3)
    );
    assert!(afsk.iter().all(|&n| n >= 1), "both stations' AFSK detectors asserted");
    assert!(violations.is_empty(), "started after detecting the burst: {violations:?}");
    // Legacy activity: frames capped at 2 s for 10 minutes (SPEC 11.3).
    let long_after =
        sim.frames_from(0).iter().filter(|a| a.key_time() > b1).map(|a| a.release - a.key).max().unwrap_or(0);
    assert!(long_after as f64 / 48_000.0 <= 2.0 + 1e-3, "frames capped at 2 s after AFSK: {long_after}");
}

fn t_end(n: usize) -> Instant {
    cairn_modem::twosim::t_of(n)
}

#[test]
fn link_lost_for_120s_then_resumed() {
    let mut sim = pair(25.0, 50);
    sim.stations[0].auto_resume = Some(Duration::from_secs(1));
    let data = payload(60_000, 6);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    sim.outage(0, 1, t(15.0), t(135.0));
    sim.run_while(t(700.0), |s| s.stations[1].delivered_bytes().len() < data.len());
    assert_eq!(sim.stations[1].delivered_bytes(), data, "exactly once, in order");
    let lost: Vec<Option<Instant>> =
        sim.stations.iter().map(|s| s.when(|e| matches!(e, HostEvent::LinkLost { .. }))).collect();
    let resumed: Vec<Option<Instant>> =
        sim.stations.iter().map(|s| s.when(|e| matches!(e, HostEvent::Connected { resumed: true, .. }))).collect();
    eprintln!(
        "120 s outage from 15 s: link lost at {lost:?}, resumed at {resumed:?}, complete at {}; dup blocks at B {}",
        sim.now(),
        sim.stations[1].modem.stats.dup_blocks
    );
    assert!(lost.iter().all(|l| l.is_some()), "both ends declared the link lost");
    assert!(resumed.iter().all(|r| r.is_some()), "both ends resumed");
    assert_eq!(sim.stations[1].modem.stats.dup_blocks, 0, "no delivered block resent");
    assert!(!sim.stations[0].has(|e| matches!(e, HostEvent::Undelivered { .. })));
}

#[test]
fn link_test_predicts_the_session() {
    for (seed, snr) in [(60u64, 14.0), (61, 25.0)] {
        let mut sim = pair(snr, seed);
        let peer = sim.addr(1);
        sim.host(0, t(0.5), HostCommand::Probe { target: peer, kind: ProbeKind::LinkTest, repeats: 2 });
        sim.run_while(t(20.0), |s| !s.stations[0].has(|e| matches!(e, HostEvent::LinkTest(_))));
        let r = sim.stations[0]
            .events
            .iter()
            .find_map(|(_, e)| match e {
                HostEvent::LinkTest(r) => Some(r.clone()),
                _ => None,
            })
            .expect("link test result");
        let air: f64 = sim.air.iter().map(|a| (a.release - a.key) as f64 / 48_000.0).sum();
        // Then a one-way session of the same direction.
        let data = payload(60_000, 7);
        let start = sim.now() + Duration::from_secs(1);
        connect(&mut sim, 0, 1, start);
        send_file(&mut sim, 0, start, &data);
        sim.run_while(start + Duration::from_secs(400), |s| s.stations[1].delivered_bytes().len() < data.len());
        let got = net_kbps(&sim, 0, 1);
        eprintln!(
            "{snr} dB link test: {:.2} s of air; forward SNR {:.1} dB MCS {} {:.2} kbps, reverse SNR {:.1} dB MCS {} {:.2} kbps; \
session {got:.2} kbps ({:+.0}%)",
            air,
            r.forward.snr_db,
            r.forward.mcs,
            r.forward.net_kbps,
            r.reverse.snr_db,
            r.reverse.mcs,
            r.reverse.net_kbps,
            100.0 * (got / r.forward.net_kbps - 1.0)
        );
        assert!(air <= 3.0, "within 3 s of air time");
        assert!((r.forward.snr_db - snr).abs() < 1.5, "{}", r.forward.snr_db);
        assert!((got / r.forward.net_kbps - 1.0).abs() <= 0.20, "within 20% of the session");
    }
}

#[test]
fn kiss_ax25_frames_cross_as_ui_broadcasts() {
    let mut sim = pair(20.0, 70);
    // Two AX.25 frames from A's KISS host (fictitious addresses), one large
    // enough to be fragmented across UI frames.
    let small: Vec<u8> = b"\x9c\x60\x86\x82\x98\x98\xe0\x9c\x60\x86\x82\x98\x98\x61\x03\xf0hello".to_vec();
    let big: Vec<u8> = small.iter().copied().chain(payload(900, 8)).collect();
    sim.host(0, t(0.5), HostCommand::Broadcast { id: 1, pdu: ax25_pdu(small.clone()), repeats: 0 });
    sim.host(0, t(0.5), HostCommand::Broadcast { id: 2, pdu: ax25_pdu(big.clone()), repeats: 0 });
    sim.run_while(t(60.0), |s| s.stations[1].broadcasts.len() < 2);
    let got: Vec<Vec<u8>> = sim.stations[1]
        .broadcasts
        .iter()
        .filter(|(_, _, ft, p)| *ft == FrameType::Ui && p.pdu_type == pdu_type::AX25)
        .map(|(_, _, _, p)| p.body.clone())
        .collect();
    let ui: Vec<_> =
        sim.frames_from(0).iter().filter_map(|a| a.frame.as_ref()).map(|f| (f.dst, f.data.len())).collect();
    eprintln!("KISS: {} AX.25 frames back at B from UI frames {ui:?}", got.len());
    assert_eq!(got, vec![small, big], "each AX.25 frame back unchanged, in order");
    assert!(ui.iter().all(|(d, _)| d.is_broadcast()), "UI to the broadcast address (ADR 0010 kiss-ui-frames)");
    assert!(sim.stations[0].has(|e| matches!(e, HostEvent::BroadcastSent { .. })));
}

#[test]
fn idle_open_squelch_gives_no_false_carrier_detect() {
    // Two minutes of open-squelch noise at both stations.
    let mut sim = pair(25.0, 80);
    sim.run_until(t(120.0));
    for s in &sim.stations {
        assert_eq!(s.modem.stats.dcd_rises, [0, 0, 0, 0], "no carrier detect on noise");
        assert_eq!(s.modem.stats.detections, 0);
        assert!(s.modem.dcd().floor_dbfs().is_some(), "quieting armed on open-squelch noise");
    }
}

#[test]
fn turnaround_with_measured_decode_time() {
    // Decoded frames reach the station as long after their last sample as
    // the decode really took (wall clock), plus the 10 ms chunk.
    let mut sim = Sim::new();
    for (call, seed) in [("N0AAA", 90u64), ("N0BBB", 91)] {
        let mut c = station(call, seed);
        c.decode_time = DecodeTime::Measured;
        c.rx.threads = 4;
        sim.add(c, 0.0, 25.0, seed);
    }
    let data = payload(30_000, 9);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    send_file(&mut sim, 1, t(3.0), &payload(10_000, 10));
    sim.run_while(t(200.0), |s| s.stations[1].delivered_bytes().len() < data.len());
    let ta = sim.turnarounds();
    let dec: Vec<f64> = sim.stations.iter().flat_map(|s| s.modem.stats.decode_ms.clone()).collect();
    let mut d = dec.clone();
    d.sort_by(f64::total_cmp);
    eprintln!(
        "turnaround (peer's last sample to PTT) over {} replies: p50 {:.1} p99 {:.1} max {:.1} ms; decode p50 {:.1} max {:.1} ms",
        ta.len(),
        percentile_ms(&ta, 50.0),
        percentile_ms(&ta, 99.0),
        percentile_ms(&ta, 100.0),
        d[d.len() / 2],
        d[d.len() - 1]
    );
    assert!(percentile_ms(&ta, 100.0) <= 100.0);
}

#[test]
fn beacon_and_ui_text() {
    let mut sim = pair(20.0, 95);
    sim.host(0, t(0.5), HostCommand::Beacon { id: 1, pdu: Some(Pdu::message(b"N0AAA test".to_vec())), repeats: 0 });
    sim.run_while(t(20.0), |s| !s.stations[1].has(|e| matches!(e, HostEvent::BeaconHeard { .. })));
    assert!(sim.stations[1].has(|e| matches!(e, HostEvent::BeaconHeard { id: Some(_), .. })));
    assert_eq!(sim.stations[1].broadcasts[0].3.body, b"N0AAA test");
}

#[test]
#[ignore = "10 minutes of simulated audio per station; run with --ignored"]
fn ten_minutes_idle_open_squelch() {
    // SPEC 14.1: at most 1 false carrier detect per 10 minutes, squelch open.
    let mut total = [0u64; 4];
    for seed in [200u64, 202, 204] {
        let mut sim = pair(25.0, seed);
        sim.run_until(t(600.0));
        for s in &sim.stations {
            for (k, v) in s.modem.stats.dcd_rises.iter().enumerate() {
                total[k] += v;
            }
        }
    }
    eprintln!("60 station-minutes of open-squelch noise: DCD rises quieting/preamble/AFSK/COS {total:?}");
    assert!(total.iter().sum::<u64>() <= 6, "{total:?}");
}

// --- Wide sessions with tone masks on a radio-like channel ----------------------------

/// A station with a width cap of `cap` tones (the wide capability above 24)
/// and tone masks tracked or not, as `run --cap N [--no-mask]` sets it.
fn wide_station(call: &str, seed: u64, cap: u8, masks: bool) -> ModemConfig {
    let mut m = station(call, seed);
    m.station.width_cap = cap - 1;
    if cap > 24 {
        m.station.capabilities |= cairn_link::frame::caps::WIDE;
    }
    m.station.track_masks = masks;
    m.dcd = cairn_phy::dcd::DcdConfig::for_width(cap as usize);
    m
}

fn wide_pair(snr: f64, seed: u64, cap: u8, masks: bool) -> Sim {
    let mut sim = Sim::new();
    sim.add(wide_station("N0AAA", seed, cap, masks), 20.0, snr, seed);
    sim.add(wide_station("N0BBB", seed + 1, cap, masks), -20.0, snr, seed + 1);
    sim
}

/// (key time s, MCS, mask ID, mask) of a station's DATA frames.
fn masked_frames(sim: &Sim, st: usize) -> Vec<(f64, u8, u8, u64)> {
    sim.frames_from(st)
        .iter()
        .filter_map(|a| {
            a.frame
                .as_ref()
                .filter(|f| f.ftype == FrameType::Data)
                .map(|f| (a.key_time().as_secs_f64(), f.mcs, f.mask_id, f.tx.mask))
        })
        .collect()
}

/// What a one-way transfer did.
#[derive(Debug)]
struct WideRun {
    kbps: f64,
    secs: f64,
    mode_mcs: u8,
    max_mcs: u8,
    frames: Vec<(f64, u8, u8, u64)>,
    lost: u64,
    /// The sender's applied mask at the end.
    tx_mask: (u64, u8),
    width: u8,
    /// Frames the receiver sent with a probe, of all it sent.
    rx_probes: (usize, usize),
}

fn radio_like_transfer(masks: bool, snr: f64, bytes: usize, seed: u64) -> WideRun {
    let mut sim = wide_pair(snr, seed, 32, masks);
    sim.radio_like_link(0, 1);
    let data = payload(bytes, seed);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    sim.run_while(t(900.0), |s| s.stations[1].delivered_bytes().len() < data.len());
    assert_eq!(sim.stations[1].delivered_bytes(), data, "masks {masks}: every byte, once, in order");
    assert_no_overlaps(&sim);
    let df = data_frames(&sim, 0);
    let first = df.first().map(|x| x.0).expect("data frames");
    let last = sim.stations[1].delivered.last().map(|x| x.0).expect("delivered");
    let info = sim.stations[0].modem.station().session_info().expect("session");
    let st = &sim.stations[1].modem.stats;
    let from_b = sim.frames_from(1);
    WideRun {
        kbps: net_kbps(&sim, 0, 1),
        secs: (last - first).as_secs_f64(),
        mode_mcs: mode_mcs(&df),
        max_mcs: df.iter().map(|f| f.1).max().unwrap_or(0),
        frames: masked_frames(&sim, 0),
        lost: st.blocks - st.blocks_ok,
        tx_mask: info.tx_mask,
        width: info.tx_width,
        rx_probes: (from_b.iter().filter(|a| a.frame.as_ref().is_some_and(|f| f.flags.probe)).count(), from_b.len()),
    }
}

const LOW_TONES: u64 = 0b111;

#[test]
fn radio_like_channel_masks_the_low_tones_at_32_tones() {
    // The radios of the first on-air session (2026-11-07): tones 0-2 far
    // down and noisy, the rest good, the response rolling off above 4.2 kHz
    // (`twosim::radio_like`). Both stations run `--cap 32`; masked, the
    // session starts with tones 0-2 dropped (the CONNECT-ACK's mask, from
    // the CONNECT's probe) and climbs to MCS 4; unmasked the noisy tones pull
    // the measured SNR down and the session stalls at MCS 2.
    let (snr, bytes) = (24.0, 60_000);
    let m = radio_like_transfer(true, snr, bytes, 70);
    let u = radio_like_transfer(false, snr, bytes, 70);
    for (name, r) in [("masked", &m), ("unmasked", &u)] {
        eprintln!(
            "radio-like channel, link SNR {snr} dB, 32 tones, {name}: {:.2} kbps net ({bytes} B in {:.0} s), MCS mode {} max {}, \
{} blocks lost; final tx mask ID {} ({}); receiver's frames with a probe {}/{}",
            r.kbps,
            r.secs,
            r.mode_mcs,
            r.max_mcs,
            r.lost,
            r.tx_mask.1,
            cairn_modem::runtime::describe_mask(r.tx_mask.0, r.width),
            r.rx_probes.0,
            r.rx_probes.1
        );
    }
    eprintln!(
        "masked DATA frames (s, MCS, mask ID): {:?}",
        m.frames.iter().map(|f| (f.0.round(), f.1, f.2)).collect::<Vec<_>>()
    );
    assert_eq!(m.width, 32);
    // Every DATA frame, the first included, already has tones 0-2 masked.
    assert!(m.frames.iter().all(|f| f.3 & LOW_TONES == 0), "{:?}", m.frames);
    assert_eq!(m.tx_mask.0 & LOW_TONES, 0);
    assert!(m.tx_mask.0.count_ones() as usize >= 56 - 3 - 1, "at most one more tone dropped");
    assert!(m.mode_mcs >= 4, "masked: MCS {}", m.mode_mcs);
    assert!(u.max_mcs < m.mode_mcs, "unmasked stalls lower: max MCS {}", u.max_mcs);
    assert!(u.frames.iter().all(|f| f.3 & LOW_TONES == LOW_TONES));
    assert!(m.kbps > 1.3 * u.kbps, "{:.2} vs {:.2} kbps", m.kbps, u.kbps);
}

#[test]
fn masks_follow_a_channel_change() {
    // Flat 32-tone link at 26 dB; at 25 s tones 0-2 fade by 25 dB (A to B),
    // at 70 s they come back. SPEC 10.3: dropped after two frames more than
    // 6 dB below the active-tone average, restored after a probe shows them
    // recovered (every 32-tone DATA frame probes, ADR 0007); every change
    // takes the next mask ID and the sender applies it (ADR 0011).
    let mut sim = wide_pair(26.0, 80, 32, true);
    let fade =
        cairn_modem::twosim::fir_from_response(&[(0.0, -25.0), (550.0, -25.0), (620.0, 0.0), (24_000.0, 0.0)], 1023);
    sim.shape_step(t(25.0), 0, 1, Some(fade));
    sim.shape_step(t(70.0), 0, 1, None);
    let data = payload(400_000, 8);
    connect(&mut sim, 0, 1, t(0.5));
    send_file(&mut sim, 0, t(0.5), &data);
    sim.run_until(t(110.0));
    let f = masked_frames(&sim, 0);
    let phase = |a: f64, b: f64| -> Vec<(u8, u8, u64)> {
        f.iter().filter(|x| x.0 >= a && x.0 < b).map(|x| (x.1, x.2, x.3 & LOW_TONES)).collect()
    };
    let (before, faded, after) = (phase(0.0, 25.0), phase(25.0, 70.0), phase(70.0, 110.0));
    eprintln!("before (MCS, ID, low-tone bits) {before:?}\nfaded {faded:?}\nafter {after:?}");
    assert!(before.iter().all(|x| x.2 == LOW_TONES), "unmasked before the fade");
    // Masked within a few frames of the fade (two measurements, the report,
    // the sender's next frame), then for the rest of it.
    let k = faded.iter().position(|x| x.2 == 0).expect("masked during the fade");
    assert!(k <= 4, "{faded:?}");
    assert!(faded[k..].iter().all(|x| x.2 == 0), "{faded:?}");
    let k = after.iter().position(|x| x.2 == LOW_TONES).expect("restored after the fade");
    assert!(k <= 3, "{after:?}");
    assert!(after[k..].iter().all(|x| x.2 == LOW_TONES), "{after:?}");
    // The mask IDs advance by one per change (mod 4).
    let ids: Vec<u8> = f.iter().map(|x| x.2).collect();
    assert!(ids.windows(2).all(|w| w[1] == w[0] || w[1] == (w[0] + 1) % 4), "{ids:?}");
    assert_no_overlaps(&sim);
}

#[test]
fn radio_tail_is_no_carrier() {
    // The AIOC radios kept their carrier 0.64-0.73 s after PTT release
    // (2026-11-07). Both stations here keep it 0.65 s: their own receivers
    // hear silence meanwhile, which used to read as a carrier (one quieting
    // rise per own transmission, 17 in 3.5 min); the peer sees the carrier
    // and waits it out before replying.
    let mut out = Vec::new();
    for legacy in [false, true] {
        let mut sim = wide_pair(25.0, 90, 32, true);
        for s in &mut sim.stations {
            s.radio_tail = Duration::from_millis(650);
            if legacy {
                let mut cfg = s.modem.config().clone();
                cfg.dcd = cfg.dcd.legacy();
                s.modem = cairn_modem::runtime::Modem::new(cfg);
            }
        }
        let data = payload(30_000, 9);
        connect(&mut sim, 0, 1, t(0.5));
        send_file(&mut sim, 0, t(0.5), &data);
        sim.run_while(t(300.0), |s| s.stations[1].delivered_bytes().len() < data.len());
        assert_eq!(sim.stations[1].delivered_bytes(), data, "legacy {legacy}");
        assert_no_overlaps(&sim);
        let end = sim.now();
        let rises: Vec<u64> = sim.stations.iter().map(|s| s.modem.stats.dcd_rises[0]).collect();
        let heard: Vec<usize> = (0..2).map(|i| sim.frames_from(1 - i).len()).collect();
        let rec: Vec<f64> = sim.stations[0].modem.stats.rx_recovery_ms.clone();
        let ta = sim.turnarounds();
        eprintln!(
            "radio tail 650 ms{}: done at {end}; quieting rises A/B {rises:?} for {heard:?} peer transmissions; \
A's receiver back after own PTT {:.0}-{:.0} ms ({}); peer replies {:.0}-{:.0} ms after the last sample",
            if legacy { " (legacy carrier detect)" } else { "" },
            rec.iter().cloned().fold(f64::INFINITY, f64::min),
            rec.iter().cloned().fold(0.0, f64::max),
            rec.len(),
            percentile_ms(&ta, 0.0),
            percentile_ms(&ta, 100.0)
        );
        out.push((rises, heard, rec));
    }
    let (rises, heard, rec) = &out[0];
    // One quieting rise per peer transmission at most: none from the tails.
    for i in 0..2 {
        assert!(rises[i] as usize <= heard[i], "{rises:?} {heard:?}");
    }
    assert!(rec.iter().all(|&r| (620.0..700.0).contains(&r)), "{rec:?}");
    let (old, _, _) = &out[1];
    assert!(old[0] + old[1] > rises[0] + rises[1] + 10, "legacy {old:?} against {rises:?}");
}

#[test]
fn rx_dir_files_arrive_byte_identical() {
    // `run --rx-dir`: every FILE-CHUNK and MESSAGE PDU delivered goes to the
    // receive directory; files are reassembled by object ID and offset.
    let mut sim = pair(25.0, 95);
    let files = [payload(9_000, 1), payload(2 * 2048, 2), payload(1, 3)];
    connect(&mut sim, 0, 1, t(0.5));
    let mut host = HostState::new();
    for f in &files {
        let (cmds, _, _) = host.file_commands(f);
        for c in cmds {
            sim.host(0, t(0.5), c);
        }
    }
    sim.host(0, t(0.5), HostCommand::Send { id: 99, pdu: Pdu::message(b"73 de N0AAA".to_vec()) });
    let total: usize = files.iter().map(|f| f.len()).sum::<usize>() + 11;
    sim.run_while(t(200.0), |s| s.stations[1].delivered_bytes().len() < total);
    let dir = std::env::temp_dir().join(format!("cairn-two-station-rxdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rx = cairn_modem::rxdir::RxDir::new(&dir).unwrap();
    let mut lines = Vec::new();
    for (_, from, pdu) in &sim.stations[1].delivered {
        lines.extend(rx.on_pdu(*from, pdu, false).unwrap());
    }
    for l in &lines {
        eprintln!("{l}");
    }
    assert_eq!(lines.len(), 4, "{lines:?}");
    for (k, f) in files.iter().enumerate() {
        let path = dir.join(format!("N0AAA_obj{}.bin", k + 1));
        assert_eq!(&std::fs::read(&path).unwrap(), f, "{}", path.display());
        assert!(lines[k].ends_with(&cairn_modem::rxdir::sha256_hex(f)), "{}", lines[k]);
    }
    assert_eq!(std::fs::read(dir.join("N0AAA_msg1.txt")).unwrap(), b"73 de N0AAA");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unattended_station_ids_its_ui_frames_and_refuses_unlisted_callers() {
    // Unattended operation through the real PHY: B's first UI broadcast
    // carries the ID block (ADR 0010 cadence applied to broadcasts) and A
    // decodes it; B accepts sessions only from N0CCC-*, so A's CONNECT gets
    // the refusal.
    let mut sim = Sim::new();
    sim.add(station("N0AAA", 80), 0.0, 20.0, 80);
    let mut b = station("N0BBB", 81);
    let allow = cairn_modem::unattended::parse_patterns(&["N0CCC-*".into()]).unwrap();
    cairn_modem::unattended::apply_lists(&mut b.station, &allow, &[]);
    b.station.id_broadcasts = true;
    sim.add(b, 0.0, 20.0, 81);
    sim.host(1, t(0.5), HostCommand::Broadcast { id: 1, pdu: Pdu::message(b"QST de N0BBB".to_vec()), repeats: 0 });
    sim.run_while(t(20.0), |s| s.stations[0].broadcasts.is_empty());
    assert_eq!(sim.stations[0].broadcasts[0].3.body, b"QST de N0BBB");
    let ui = sim.stations[0].heard.iter().find(|f| f.header.ftype == FrameType::Ui).expect("the UI frame");
    assert_eq!((ui.controls, ui.controls_ok), (1, 1), "the ID block, decoded");
    let at = sim.now() + Duration::from_secs(2);
    connect(&mut sim, 0, 1, at);
    sim.run_while(at + Duration::from_secs(30), |s| {
        !s.stations[0].has(|e| matches!(e, HostEvent::ConnectFailed { .. }))
    });
    assert!(sim.stations[0].has(|e| matches!(e, HostEvent::ConnectFailed { reason: ConnectFailure::Refused, .. })));
    assert!(sim.stations[1].has(|e| matches!(e, HostEvent::Refused { .. })));
}
