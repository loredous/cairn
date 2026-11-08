//! Scenario tests against the SPEC §14.1 link targets and the ADR 0010
//! session rules, on the simulated air of `common`.

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use cairn_link::linktest::ProbeKind;
use cairn_link::params::{ALL_TONES, RUNG_THRESHOLDS_DB};
use cairn_link::{ConnectFailure, ControlBlock, FrameType, HostCommand, HostEvent, Instant, Pdu};
use common::*;

fn connect(sim: &mut Sim, a: usize, b: usize, at: Instant) {
    let peer = sim.addr(b);
    sim.host(a, at, HostCommand::Connect { peer, new_session: false });
}

/// (start, mcs) of a station's DATA frames.
fn data_frames(sim: &Sim, st: usize) -> Vec<(Instant, u8)> {
    sim.frames_from(st)
        .iter()
        .filter_map(|a| a.frame.as_ref().filter(|f| f.ftype == FrameType::Data).map(|f| (a.start, f.mcs)))
        .collect()
}

fn frames_of(sim: &Sim, st: usize, ft: FrameType) -> Vec<(Instant, Instant)> {
    sim.frames_from(st)
        .iter()
        .filter(|a| a.frame.as_ref().is_some_and(|f| f.ftype == ft))
        .map(|a| (a.start, a.end))
        .collect()
}

/// Frames after `step` until the rung reaches its final value and holds.
fn settle(frames: &[(Instant, u8)], step: Instant) -> (usize, u8, usize) {
    let after: Vec<u8> = frames.iter().filter(|(s, _)| *s >= step).map(|(_, m)| *m).collect();
    let last = *after.last().expect("frames after the step");
    let i = after.iter().rposition(|&m| m != last).map_or(0, |i| i + 1);
    (i + 1, last, after.len())
}

#[test]
fn rung_settles_within_two_frames_after_6db_drop() {
    for (seed, from, to) in [(11u64, 25.0, 19.0), (12, 19.0, 13.0), (13, 14.0, 8.0)] {
        let mut sim = pair(seed, from);
        connect(&mut sim, 0, 1, t(0.0));
        send_chunks(&mut sim, 0, t(0.1), &payload(400_000, seed), 1000);
        sim.snr_step(t(40.0), 0, 1, to);
        sim.run_until(t(110.0));
        let (n, rung, total) = settle(&data_frames(&sim, 0), t(40.0));
        eprintln!("{from} -> {to} dB: settled at MCS {rung} after {n} frame(s), held {total} frames");
        assert!(n <= 2, "{from} -> {to} dB: settled after {n} frames");
        assert!(bler(to, RUNG_THRESHOLDS_DB[rung as usize]) <= 0.10, "MCS {rung} at {to} dB");
        assert_clean(&sim);
    }
}

#[test]
fn rung_climbs_within_four_frames_after_6db_rise() {
    // ADR 0013: §14.1 asks for the correct rung within 4 frames of a rise.
    // The step-up still needs 3 clean frames (§10.1), but then requests the
    // highest rung 2 dB below the measured SNR, so a 6 dB rise is climbed in
    // one step: 3 frames to see the run, the 4th at the new rung.
    for (seed, from, to) in [(14u64, 19.0, 25.0), (15, 8.0, 14.0), (16, 13.0, 19.0), (17, 6.0, 12.0)] {
        let mut sim = pair(seed, from);
        connect(&mut sim, 0, 1, t(0.0));
        send_chunks(&mut sim, 0, t(0.1), &payload(400_000, seed), 1000);
        sim.snr_step(t(40.0), 0, 1, to);
        sim.run_until(t(130.0));
        let frames = data_frames(&sim, 0);
        let before = frames.iter().rfind(|(s, _)| *s < t(40.0)).unwrap().1;
        let (n, rung, _) = settle(&frames, t(40.0));
        let climbed = rung.saturating_sub(before) as usize;
        eprintln!("{from} -> {to} dB: MCS {before} -> {rung} after {n} frames");
        assert!(rung > before);
        assert!(n <= 4, "{from} -> {to} dB: {n} frames for {climbed} rungs");
        assert!(bler(to, RUNG_THRESHOLDS_DB[rung as usize]) <= 0.10, "MCS {rung} at {to} dB");
        assert_clean(&sim);
    }
}

#[test]
fn no_oscillation_over_60s_at_constant_snr() {
    for (i, snr) in [6.0, 10.0, 14.0, 19.0, 22.0, 27.0].into_iter().enumerate() {
        let mut sim = pair(100 + i as u64, snr);
        connect(&mut sim, 0, 1, t(0.0));
        send_chunks(&mut sim, 0, t(0.1), &payload(300_000, i as u64), 1000);
        sim.run_until(t(75.0));
        let frames = data_frames(&sim, 0);
        let settled: Vec<u8> = frames.iter().filter(|(s, _)| *s >= t(10.0)).map(|(_, m)| *m).collect();
        let changes = settled.windows(2).filter(|w| w[0] != w[1]).count();
        eprintln!("{snr} dB: MCS {:?} over {} frames, {changes} changes", settled.first(), settled.len());
        assert_eq!(changes, 0, "{snr} dB: {settled:?}");
        assert_clean(&sim);
    }
}

#[test]
fn transfer_survives_60s_of_link_loss_without_resending_delivered_blocks() {
    // 60 s of silence: the ACK-timer escalation outlasts it, so the session
    // carries on without a resume.
    let mut sim = pair(21, 25.0);
    sim.stations[0].auto_resume = Some(Duration::from_secs(1));
    let data = payload(150_000, 21);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.outage(0, 1, t(20.0), t(80.0));
    sim.run_while(t(600.0), |s| s.delivered_bytes(1).len() < data.len());
    assert_eq!(sim.delivered_bytes(1), data, "exactly once, in order");
    assert_eq!(sim.duplicate_blocks(1), 0, "no block received twice");
    let lost = sim.has_event(0, |e| matches!(e, HostEvent::LinkLost { .. }));
    eprintln!("60 s outage: complete at {}, link declared lost: {lost}", sim.now);
    assert_clean(&sim);
}

#[test]
fn resume_after_120s_link_loss_resends_no_delivered_block() {
    // ADR 0013: §14.1's session-resume target is tested at a 120 s outage.
    let mut sim = pair(22, 25.0);
    sim.stations[0].auto_resume = Some(Duration::from_secs(1));
    let data = payload(150_000, 22);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    // 120 s: long enough for both ends to declare the link lost.
    sim.outage(0, 1, t(20.0), t(140.0));
    sim.run_while(t(900.0), |s| s.delivered_bytes(1).len() < data.len());
    assert_eq!(sim.delivered_bytes(1), data, "exactly once, in order");
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::LinkLost { .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::LinkLost { .. })));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::Connected { resumed: true, .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::Connected { resumed: true, .. })));
    assert_eq!(sim.duplicate_blocks(1), 0, "no block received twice");
    // B's resume CONNECT-ACK carries its next in-order sequence; nothing
    // A sends afterwards lies below it.
    let ack = sim
        .frames_from(1)
        .iter()
        .find_map(|a| {
            let f = a.frame.as_ref()?;
            (f.ftype == FrameType::ConnectAck && a.start > t(20.0)).then_some(())?;
            match f.controls.get(2) {
                Some(ControlBlock::Ack(k)) => Some((a.start, k.base)),
                _ => None,
            }
        })
        .expect("resume CONNECT-ACK with an ACK block");
    for a in sim.frames_from(0) {
        let Some(f) = &a.frame else { continue };
        if a.start > ack.0 && f.ftype == FrameType::Data {
            for s in f.data_seqs() {
                assert!(s.diff(ack.1) >= 0, "block {s} re-sent below {}", ack.1);
            }
        }
    }
    let done = sim.now;
    eprintln!(
        "resume: complete at {done}, connected again at {:?}",
        sim.event_time(0, |e| matches!(e, HostEvent::Connected { resumed: true, .. }))
    );
    assert_clean(&sim);
}

#[test]
fn link_test_completes_within_3s_of_air_time() {
    let mut sim = pair(31, 20.0);
    let b = sim.addr(1);
    sim.host(0, t(0.0), HostCommand::Probe { target: b, kind: ProbeKind::LinkTest, repeats: 0 });
    sim.run_until(t(10.0));
    let r = sim.stations[0]
        .events
        .iter()
        .find_map(|(_, e)| match e {
            HostEvent::LinkTest(r) => Some(r.clone()),
            _ => None,
        })
        .expect("link test result");
    let air: Duration = sim.air.iter().map(|a| a.end - a.start).sum();
    eprintln!("link test: {r:?}, air time {air:?}");
    assert_eq!(sim.air.len(), 2, "two transmissions");
    assert!(air < Duration::from_secs(3));
    assert_eq!(r.forward.mcs, 3);
    assert_eq!(r.reverse.mcs, 3);
    // Within 20% of what a following session achieves.
    connect(&mut sim, 0, 1, t(10.0));
    let data = payload(300_000, 3);
    send_chunks(&mut sim, 0, t(10.0), &data, 1000);
    sim.run_until(t(100.0));
    let d = &sim.stations[1].delivered;
    let first = data_frames(&sim, 0)[0].0;
    let bytes: usize = d.iter().map(|(_, _, p)| p.body.len() + 9).sum();
    let kbps = bytes as f64 * 8.0 / (d.last().unwrap().0 - first).as_secs_f64() / 1000.0;
    eprintln!("predicted {:.2} kbps, session {:.2} kbps", r.forward.net_kbps, kbps);
    assert!((kbps - r.forward.net_kbps).abs() / kbps <= 0.20);
    assert_clean(&sim);
}

#[test]
fn beacon_request_is_answered_with_a_beacon() {
    let mut sim = pair(32, 20.0);
    let b = sim.addr(1);
    sim.host(0, t(0.0), HostCommand::Probe { target: b, kind: ProbeKind::Beacon, repeats: 0 });
    sim.run_until(t(10.0));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::BeaconHeard { from, id: Some(_) } if *from == b)));
    assert!(!sim.has_event(0, |e| matches!(e, HostEvent::Unreachable { .. })));
    assert_clean(&sim);
}

#[test]
fn probe_unanswered_in_session_is_unreachable() {
    let mut sim = pair(33, 20.0);
    let c = sim.add(cfg("N0CCC", 33), Duration::from_millis(150));
    let b = sim.addr(1);
    connect(&mut sim, 0, 1, t(0.0));
    sim.host(c, t(5.0), HostCommand::Probe { target: b, kind: ProbeKind::LinkTest, repeats: 0 });
    sim.run_until(t(20.0));
    assert!(sim.has_event(c, |e| matches!(e, HostEvent::Unreachable { .. })));
    assert_clean(&sim);
}

#[test]
fn connect_refusal() {
    let mut sim = Sim::new(41);
    sim.add(cfg("N0AAA", 1), Duration::from_millis(150));
    let mut c = cfg("N0BBB", 2);
    c.accept_connects = false;
    sim.add(c, Duration::from_millis(150));
    connect(&mut sim, 0, 1, t(0.0));
    sim.run_until(t(30.0));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::ConnectFailed { reason: ConnectFailure::Refused, .. })));
    assert_eq!(frames_of(&sim, 0, FrameType::Connect).len(), 1, "no retry after a refusal");
    let d: Vec<_> = sim.frames_from(1).iter().filter_map(|a| a.frame.clone()).collect();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].ftype, FrameType::Disconnect);
    assert!(!d[0].flags.session);
    assert!(sim.air[1].turn_holder);
    assert_clean(&sim);
}

#[test]
fn connect_retries_exhaust() {
    let mut sim = pair(42, 25.0);
    sim.link(0, 1).audible = false;
    sim.link(1, 0).audible = false;
    connect(&mut sim, 0, 1, t(0.0));
    sim.run_until(t(80.0));
    let c = frames_of(&sim, 0, FrameType::Connect);
    assert_eq!(c.len(), 4, "the first CONNECT and three retries");
    for k in 1..4 {
        let gap = (c[k].0 - c[k - 1].1).as_secs_f64();
        let (lo, hi) = (3.0 + k as f64 * 1.0, 3.0 + k as f64 * 5.0 + 1.0);
        eprintln!("retry {k}: {gap:.2} s after PTT release");
        assert!(gap >= lo && gap <= hi, "retry {k} after {gap} s");
    }
    let fail = sim.event_time(0, |e| matches!(e, HostEvent::ConnectFailed { reason: ConnectFailure::NoAnswer, .. }));
    let wait = (fail.unwrap() - c[3].1).as_secs_f64();
    assert!((wait - 3.0).abs() < 1e-3, "{wait}");
    assert_clean(&sim);
}

#[test]
fn lost_connect_ack_and_lost_first_frame_recover() {
    for (drop_from, drop_type) in [(1usize, FrameType::ConnectAck), (0, FrameType::Data)] {
        let mut sim = pair(43, 25.0);
        let dropped = Rc::new(Cell::new(false));
        let d2 = dropped.clone();
        sim.filter = Some(Box::new(move |_, _r, a| {
            if !d2.get() && a.from == drop_from && a.frame.as_ref().is_some_and(|f| f.ftype == drop_type) {
                d2.set(true);
                return Fate::HeaderLost;
            }
            Fate::Normal
        }));
        let data = payload(10_000, 4);
        connect(&mut sim, 0, 1, t(0.0));
        send_chunks(&mut sim, 0, t(0.0), &data, 1000);
        sim.run_while(t(120.0), |s| s.delivered_bytes(1).len() < data.len());
        assert!(dropped.get());
        assert_eq!(sim.delivered_bytes(1), data);
        assert_eq!(sim.stations[1].events.iter().filter(|(_, e)| matches!(e, HostEvent::Connected { .. })).count(), 1);
        assert_clean(&sim);
    }
}

#[test]
fn mask_change_recovers_when_the_ack_carrying_it_is_lost() {
    let mut sim = Sim::new(51);
    for (call, seed) in [("N0AAA", 1), ("N0BBB", 2)] {
        let mut c = cfg(call, seed);
        c.track_masks = true;
        sim.add(c, Duration::from_millis(150));
    }
    sim.set_snr(0, 1, 25.0);
    let dropped = Rc::new(Cell::new(0usize));
    let d2 = dropped.clone();
    sim.filter = Some(Box::new(move |_, r, a| {
        // Lose the leading ACK block of the first frames from B that carry mask ID 1.
        if r == 0 && d2.get() < 2 {
            if let Some(ControlBlock::Ack(k)) = a.frame.as_ref().and_then(|f| f.controls.first()) {
                if k.mask_id == 1 {
                    d2.set(d2.get() + 1);
                    return Fate::DropControls(vec![0]);
                }
            }
        }
        Fate::Normal
    }));
    let data = payload(150_000, 5);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.impair(t(15.0), 0, 1, vec![(7, 20.0)]);
    sim.run_while(t(300.0), |s| s.delivered_bytes(1).len() < data.len());
    assert_eq!(dropped.get(), 2, "two ACKs carrying the change were lost");
    assert_eq!(sim.delivered_bytes(1), data);
    let want = (ALL_TONES & !(1 << 7), 1);
    assert_eq!(sim.stations[0].st.session_info().unwrap().tx_mask, want);
    assert_eq!(sim.stations[1].st.session_info().unwrap().rx_mask, want);
    // A frames named mask 0 until an ACK with ID 1 decoded, then mask 1.
    let ids: Vec<u8> = sim
        .frames_from(0)
        .iter()
        .filter_map(|a| a.frame.as_ref())
        .filter(|f| f.flags.session)
        .map(|f| f.mask_id)
        .collect();
    let first1 = ids.iter().position(|&m| m == 1).expect("mask 1 applied");
    assert!(ids[first1..].iter().all(|&m| m == 1));
    assert_clean(&sim);
}

#[test]
fn keepalive_cadence_and_link_loss_timing() {
    let mut sim = pair(61, 25.0);
    connect(&mut sim, 0, 1, t(0.0));
    sim.outage(0, 1, t(200.0), t(10_000.0));
    sim.run_until(t(400.0));
    let a_sounds = frames_of(&sim, 0, FrameType::Sound);
    // Every keepalive before the outage starts 30 s (plus carrier sense)
    // after the end of the previous frame either station sent.
    let mut all: Vec<(Instant, Instant)> = sim.air.iter().map(|a| (a.start, a.end)).collect();
    all.sort();
    let mut n = 0;
    for (s, _) in a_sounds.iter().filter(|(s, _)| *s < t(200.0)) {
        let prev_end = all.iter().filter(|(_, e)| e < s).map(|(_, e)| *e).max().unwrap();
        let gap = (*s - prev_end).as_secs_f64();
        assert!((30.0..32.0).contains(&gap), "keepalive {gap} s after the last frame");
        n += 1;
    }
    assert!(n >= 5, "{n} keepalives");
    assert!(frames_of(&sim, 1, FrameType::Sound).iter().all(|(s, _)| *s >= t(200.0)), "B only as a fallback");
    // Link loss: the first unanswered keepalive, then 8 timer periods.
    let lost = sim.event_time(0, |e| matches!(e, HostEvent::LinkLost { .. })).unwrap();
    // The unanswered keepalive is A's last transmission under carrier sense.
    let ka = sim
        .frames_from(0)
        .iter()
        .filter(|a| !a.turn_holder && a.start < lost)
        .map(|a| (a.start, a.end))
        .next_back()
        .unwrap();
    let ka = &ka;
    let took = (lost - ka.1).as_secs_f64();
    let (lo, hi) = (8.0 * 6.6 + 7.0 * 0.49, 8.0 * 6.85 + 7.0 * 0.5 + 2.2);
    eprintln!("A: keepalive at {}, link lost {took:.1} s later", ka.0);
    assert!(took >= lo && took <= hi, "A lost the link {took} s after its keepalive");
    // B, the original responder, falls back to its own keepalive at 35-36 s.
    let b_ka = frames_of(&sim, 1, FrameType::Sound)[0];
    let last_a = sim.frames_from(0).iter().filter(|a| a.start < t(200.0)).map(|a| a.end).max().unwrap();
    let gap = (b_ka.0 - last_a).as_secs_f64();
    assert!((35.0..38.5).contains(&gap), "B keepalive {gap} s after A's last frame");
    let b_lost = sim.event_time(1, |e| matches!(e, HostEvent::LinkLost { .. })).unwrap();
    let took = (b_lost - b_ka.1).as_secs_f64();
    eprintln!("B: keepalive {gap:.1} s after A's last frame, link lost {took:.1} s later");
    assert!((8.0 * 7.1 + 7.0 * 0.49..=8.0 * 7.35 + 7.0 * 0.5 + 2.2).contains(&took), "{took}");
    assert_clean(&sim);
}

#[test]
fn yield_window_lets_a_third_station_in() {
    let mut sim = pair(71, 25.0);
    let c = sim.add(cfg("N0CCC", 71), Duration::from_millis(150));
    let data = payload(400_000, 6);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.host(c, t(10.0), HostCommand::Broadcast { id: 1, pdu: Pdu::message(b"CQ CQ de N0CCC".to_vec()), repeats: 0 });
    sim.run_until(t(120.0));
    let ui = frames_of(&sim, c, FrameType::Ui);
    assert_eq!(ui.len(), 1);
    eprintln!("third station got in at {}", ui[0].0);
    assert!(ui[0].0 < t(75.0));
    assert_eq!(sim.collisions_between(c, 0) + sim.collisions_between(c, 1), 0);
    assert!(sim.stations[1].broadcasts.iter().any(|(_, _, p)| p.body == b"CQ CQ de N0CCC"));
    // The session carried on afterwards.
    assert!(data_frames(&sim, 0).iter().any(|(s, _)| *s > ui[0].1));
    assert_clean(&sim);
}

#[test]
fn foreign_carrier_is_never_talked_over() {
    let mut sim = pair(72, 22.0);
    let data = payload(60_000, 7);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    send_chunks(&mut sim, 1, t(0.1), &payload(20_000, 8), 900);
    for (s, d) in [(6.3, 3.0), (17.0, 0.4), (25.5, 5.0), (40.0, 9.0), (61.2, 1.2)] {
        sim.foreign(t(s), Duration::from_secs_f64(d));
    }
    sim.run_while(t(400.0), |s| s.delivered_bytes(1).len() < data.len());
    assert_eq!(sim.delivered_bytes(1), data);
    assert_clean(&sim);
}

#[test]
fn disconnect_reports_undelivered() {
    let mut sim = pair(81, 25.0);
    let data = payload(100_000, 9);
    connect(&mut sim, 0, 1, t(0.0));
    let n = send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.host(0, t(12.0), HostCommand::Disconnect { graceful: false });
    sim.run_until(t(60.0));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::Disconnected { .. })));
    let undelivered: Vec<u64> = sim.stations[0]
        .events
        .iter()
        .find_map(|(_, e)| match e {
            HostEvent::Undelivered { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .unwrap();
    let delivered = sim.stations[1].delivered.len();
    assert!(delivered > 0 && delivered < n);
    let expect: Vec<u64> = (delivered as u64..n as u64).collect();
    assert_eq!(undelivered, expect, "delivered and undelivered partition the PDUs");
    assert_clean(&sim);
}

#[test]
fn broadcast_fragments_and_several_pdus_per_frame() {
    let mut sim = pair(91, 20.0);
    let big = payload(2_000, 10);
    sim.host(0, t(0.0), HostCommand::Broadcast { id: 1, pdu: Pdu::message(big.clone()), repeats: 1 });
    for i in 0..3u64 {
        sim.host(0, t(0.0), HostCommand::Broadcast { id: 10 + i, pdu: Pdu::message(vec![i as u8; 20]), repeats: 0 });
    }
    sim.run_until(t(120.0));
    let got: Vec<Vec<u8>> = sim.stations[1].broadcasts.iter().map(|(_, _, p)| p.body.clone()).collect();
    assert_eq!(got[0], big, "reassembled once despite the repeats");
    assert_eq!(&got[1..], &[vec![0u8; 20], vec![1u8; 20], vec![2u8; 20]]);
    let ui = frames_of(&sim, 0, FrameType::Ui).len();
    let fragments = 2_000usize.div_ceil(3 * 126 - 9);
    assert_eq!(ui, 2 * fragments + 1, "each fragment twice, the three small PDUs in one frame");
    assert_clean(&sim);
}

#[test]
fn identification_cadence_in_a_long_session() {
    let mut sim = pair(95, 25.0);
    connect(&mut sim, 0, 1, t(0.0));
    sim.run_until(t(40.0 * 60.0));
    for st in 0..2 {
        let ids: Vec<Instant> = sim
            .frames_from(st)
            .iter()
            .filter(|a| a.frame.as_ref().is_some_and(|f| f.controls.iter().any(|c| matches!(c, ControlBlock::Id(_)))))
            .map(|a| a.start)
            .collect();
        assert!(ids.len() >= 4);
        for w in ids.windows(2) {
            let gap = (w[1] - w[0]).as_secs_f64();
            assert!(gap <= 600.0, "station {st}: ID blocks {gap} s apart");
        }
    }
    assert_eq!(sim.collisions_between(0, 1), 0);
    assert_clean(&sim);
}

#[test]
fn repeater_mode_session() {
    let mut sim = Sim::new(96);
    for (call, seed) in [("N0AAA", 1u64), ("N0BBB", 2)] {
        let mut c = cfg(call, seed);
        c.repeater = true;
        sim.add_with_model(c, cairn_link::AppendixA::repeater());
    }
    sim.set_snr(0, 1, 22.0);
    let data = payload(40_000, 11);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.run_while(t(300.0), |s| s.delivered_bytes(1).len() < data.len());
    assert_eq!(sim.delivered_bytes(1), data);
    assert!(sim.air.iter().all(|a| a.frame.as_ref().is_some_and(|f| f.tx.long_prefix)));
    // Every frame stays within 4 s of PTT-on time, access delay included.
    assert!(sim.air.iter().all(|a| a.end - a.start <= Duration::from_secs(4)));
    assert_clean(&sim);
}

#[test]
fn legacy_activity_caps_frames_at_two_seconds() {
    let mut sim = pair(97, 25.0);
    let data = payload(200_000, 12);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.event(0, t(20.0), cairn_link::Event::LegacyActivity);
    sim.run_until(t(60.0));
    let long = |a: &&AirTx| a.end - a.start > Duration::from_secs(2);
    let frames = sim.frames_from(0);
    assert!(frames.iter().filter(|a| a.start < t(20.0)).any(long));
    assert!(!frames.iter().filter(|a| a.start >= t(20.0)).any(long), "frames capped at 2 s (§11.3)");
    assert_clean(&sim);
}

#[test]
fn idle_disconnect_counts_from_the_last_data() {
    let mut sim = pair_with(98, 25.0, |c| c.idle_disconnect = Some(Duration::from_secs(90)));
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &payload(3000, 13), 1000);
    sim.run_until(t(300.0));
    let last_data = frames_of(&sim, 0, FrameType::Data).last().unwrap().1;
    let disc = frames_of(&sim, 0, FrameType::Disconnect)[0].0;
    let gap = (disc - last_data).as_secs_f64();
    eprintln!(
        "idle DISCONNECT {gap:.1} s after the last data frame, keepalives in between: {}",
        frames_of(&sim, 0, FrameType::Sound).len()
    );
    assert!((90.0..95.0).contains(&gap), "{gap}");
    assert!(!frames_of(&sim, 0, FrameType::Sound).is_empty(), "keepalives ran and did not reset the idle time");
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert_clean(&sim);
}

#[test]
fn both_stations_resuming_converge() {
    let mut sim = pair(99, 20.0);
    sim.stations[0].auto_resume = Some(Duration::from_secs(1));
    sim.stations[1].auto_resume = Some(Duration::from_secs(1));
    let ab = payload(60_000, 14);
    let ba = payload(30_000, 15);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &ab, 1000);
    send_on_connect(&mut sim, 1, &ba, 1000);
    sim.outage(0, 1, t(15.0), t(150.0));
    sim.run_while(t(1500.0), |s| s.delivered_bytes(1).len() < ab.len() || s.delivered_bytes(0).len() < ba.len());
    assert_eq!(sim.delivered_bytes(1), ab);
    assert_eq!(sim.delivered_bytes(0), ba);
    assert_eq!(sim.duplicate_blocks(1) + sim.duplicate_blocks(0), 0);
    assert_clean(&sim);
}

#[test]
fn connect_from_a_third_station_during_a_session_is_refused() {
    let mut sim = pair(100, 25.0);
    let c = sim.add(cfg("N0CCC", 100), Duration::from_millis(150));
    let data = payload(20_000, 16);
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &data, 1000);
    sim.run_until(t(25.0));
    connect(&mut sim, c, 1, t(25.0));
    sim.run_until(t(120.0));
    assert!(sim.has_event(c, |e| matches!(e, HostEvent::ConnectFailed { reason: ConnectFailure::Refused, .. })));
    assert_eq!(sim.delivered_bytes(1), data);
    assert_clean(&sim);
}

#[test]
fn lost_disconnect_reply_is_answered_again_while_lingering() {
    let mut sim = pair(101, 25.0);
    let dropped = Rc::new(Cell::new(false));
    let d2 = dropped.clone();
    sim.filter = Some(Box::new(move |_, r, a| {
        if r == 0 && !d2.get() && a.frame.as_ref().is_some_and(|f| f.ftype == FrameType::Disconnect) {
            d2.set(true);
            return Fate::HeaderLost;
        }
        Fate::Normal
    }));
    connect(&mut sim, 0, 1, t(0.0));
    sim.host(0, t(10.0), HostCommand::Disconnect { graceful: true });
    sim.run_until(t(60.0));
    assert!(dropped.get());
    assert_eq!(frames_of(&sim, 0, FrameType::Disconnect).len(), 2, "one resend");
    assert_eq!(frames_of(&sim, 1, FrameType::Disconnect).len(), 2, "the reply, then the lingering answer");
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(sim.stations[0].st.session_info().is_none() && sim.stations[1].st.session_info().is_none());
    assert_clean(&sim);
}
