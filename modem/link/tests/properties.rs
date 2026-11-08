//! Property tests on the simulated air. Case counts default low enough for
//! a laptop; set `PROPTEST_CASES` for long runs.

mod common;

use std::time::Duration;

use cairn_link::{HostCommand, HostEvent, Pdu};
use common::*;
use proptest::prelude::*;

fn connect(sim: &mut Sim) {
    let peer = sim.addr(1);
    sim.host(0, t(0.0), HostCommand::Connect { peer, new_session: false });
}

/// Queue PDUs of the given sizes; returns the concatenated bytes.
fn queue(sim: &mut Sim, st: usize, sizes: &[usize], seed: u64) -> Vec<u8> {
    let mut all = Vec::new();
    for (i, &n) in sizes.iter().enumerate() {
        let body = payload(n, seed.wrapping_add(i as u64));
        all.extend_from_slice(&body);
        if st == 0 {
            sim.host(st, t(0.05 * i as f64), HostCommand::Send { id: i as u64, pdu: Pdu::message(body) });
        } else {
            sim.stations[st].on_connect.push((i as u64, Pdu::message(body)));
        }
    }
    all
}

fn pdu_count(sim: &Sim, st: usize) -> usize {
    sim.stations[st].delivered.len()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(24), ..ProptestConfig::default() })]

    /// Exactly-once, in-order delivery of arbitrary byte streams both ways
    /// under arbitrary (bounded) header, ACK and block loss, with the host
    /// resuming after a link loss.
    #[test]
    fn exactly_once_in_order_under_loss(
        seed in any::<u64>(),
        sizes_ab in proptest::collection::vec(1usize..4000, 0..16),
        sizes_ba in proptest::collection::vec(1usize..4000, 0..6),
        snr in 6.0f64..30.0,
        header_loss in 0.0f64..0.25,
        ack_loss in 0.0f64..0.25,
        block_loss in 0.0f64..0.25,
    ) {
        let mut sim = pair(seed, snr);
        sim.stations[0].auto_resume = Some(Duration::from_secs(2));
        for (a, b) in [(0, 1), (1, 0)] {
            let l = sim.link(a, b);
            l.header_loss = header_loss;
            l.ack_loss = ack_loss;
            l.block_loss = block_loss;
        }
        connect(&mut sim);
        let ab = queue(&mut sim, 0, &sizes_ab, seed);
        let ba = queue(&mut sim, 1, &sizes_ba, seed ^ 1);
        sim.run_while(t(6000.0), |s| s.delivered_bytes(1).len() < ab.len() || s.delivered_bytes(0).len() < ba.len());
        // Run on: nothing is delivered twice later, and anything left over is
        // reported once retained state expires.
        let end = sim.now + Duration::from_secs(700);
        sim.run_until(end);
        // §12.4: every PDU is delivered in order exactly once, or reported
        // undelivered. Without host retries the delivered PDUs are a prefix
        // of those queued, and every one after the prefix is reported.
        for (tx, rx, sizes, bytes) in [(0usize, 1usize, &sizes_ab, &ab), (1, 0, &sizes_ba, &ba)] {
            // B queues its PDUs when its session first comes up; if it never
            // did, they were never submitted.
            let submitted = sizes.len() - sim.stations[tx].on_connect.len();
            let sizes = &sizes[..submitted];
            let got = sim.delivered_bytes(rx);
            let n = pdu_count(&sim, rx);
            let prefix: usize = sizes[..n.min(sizes.len())].iter().sum();
            prop_assert!(n <= sizes.len(), "more PDUs delivered than queued");
            prop_assert_eq!(&got[..], &bytes[..prefix], "delivered PDUs are not the queued prefix");
            let reported: Vec<u64> = sim.stations[tx].events.iter().filter_map(|(_, e)| match e {
                HostEvent::Undelivered { ids, .. } => Some(ids.clone()),
                _ => None,
            }).flatten().collect();
            for id in n as u64..sizes.len() as u64 {
                prop_assert!(reported.contains(&id), "PDU {} neither delivered nor reported", id);
            }
            if n < sizes.len() {
                eprintln!("seed {seed}: {n} of {} PDUs delivered, the rest reported undelivered", sizes.len());
            }
        }
        assert_clean(&sim);
    }

    /// No station keys while a foreign carrier it can hear is up (turn-holder
    /// replies included, by the 50 ms raw-DCD check), and the transfer
    /// still completes.
    #[test]
    fn never_transmits_over_foreign_dcd(
        seed in any::<u64>(),
        bursts in proptest::collection::vec((0.0f64..240.0, 0.02f64..6.0), 1..16),
        snr in 12.0f64..28.0,
    ) {
        let mut sim = pair(seed, snr);
        sim.stations[0].auto_resume = Some(Duration::from_secs(2));
        connect(&mut sim);
        let ab = payload(40_000, seed);
        send_chunks(&mut sim, 0, t(0.1), &ab, 1500);
        let ba = payload(8_000, seed ^ 7);
        send_on_connect(&mut sim, 1, &ba, 800);
        for (s, d) in &bursts {
            sim.foreign(t(*s), Duration::from_secs_f64(*d));
        }
        sim.run_while(t(3000.0), |s| s.delivered_bytes(1).len() < ab.len() || s.delivered_bytes(0).len() < ba.len());
        prop_assert_eq!(sim.delivered_bytes(1), ab);
        prop_assert_eq!(sim.delivered_bytes(0), ba);
        assert_clean(&sim);
    }

    /// Sequences that start near the wrap point cross it cleanly.
    #[test]
    fn sequence_wraparound(
        seed in any::<u64>(),
        below_wrap in 0u16..400,
        kbytes in 20usize..90,
        block_loss in 0.0f64..0.2,
    ) {
        let start = 65535u16.wrapping_sub(below_wrap);
        let mut sim = pair_with(seed, 25.0, |c| c.initial_sequence = start);
        sim.stations[0].auto_resume = Some(Duration::from_secs(2));
        sim.link(0, 1).block_loss = block_loss;
        sim.link(1, 0).block_loss = block_loss;
        connect(&mut sim);
        let ab = payload(kbytes * 1000, seed);
        send_chunks(&mut sim, 0, t(0.1), &ab, 1200);
        let ba = payload(kbytes * 300, seed ^ 3);
        send_on_connect(&mut sim, 1, &ba, 1200);
        sim.run_while(t(3000.0), |s| s.delivered_bytes(1).len() < ab.len() || s.delivered_bytes(0).len() < ba.len());
        prop_assert_eq!(sim.delivered_bytes(1), ab);
        prop_assert_eq!(sim.delivered_bytes(0), ba);
        let info = sim.stations[0].st.session_info().unwrap();
        // The stream really crossed 65535 -> 0.
        prop_assert!(info.next_seq.0 < start || below_wrap as usize * 126 > kbytes * 1000);
        assert_clean(&sim);
    }

    /// A link loss followed by a resume delivers everything exactly once,
    /// and no block is received twice.
    #[test]
    fn resume_after_link_loss_delivers_exactly_once(
        seed in any::<u64>(),
        outage_start in 10.0f64..60.0,
        outage_len in 60.0f64..240.0,
        kbytes in 20usize..120,
        snr in 10.0f64..30.0,
    ) {
        let mut sim = pair(seed, snr);
        sim.stations[0].auto_resume = Some(Duration::from_secs(1));
        connect(&mut sim);
        let ab = payload(kbytes * 1000, seed);
        send_chunks(&mut sim, 0, t(0.1), &ab, 1000);
        let ba = payload(kbytes * 100, seed ^ 5);
        send_on_connect(&mut sim, 1, &ba, 1000);
        sim.outage(0, 1, t(outage_start), t(outage_start + outage_len));
        sim.run_while(t(4000.0), |s| s.delivered_bytes(1).len() < ab.len() || s.delivered_bytes(0).len() < ba.len());
        prop_assert_eq!(sim.delivered_bytes(1), ab);
        prop_assert_eq!(sim.delivered_bytes(0), ba);
        prop_assert_eq!(sim.duplicate_blocks(1), 0);
        prop_assert_eq!(sim.duplicate_blocks(0), 0);
        prop_assert!(!sim.has_event(0, |e| matches!(e, HostEvent::Undelivered { .. })), "undelivered reported");
        assert_clean(&sim);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(24).div_ceil(4), ..ProptestConfig::default() })]

    /// Two stations in a long idle session with keepalives never collide,
    /// and the session stays up.
    #[test]
    fn idle_keepalive_never_collides(seed in any::<u64>(), snr in 8.0f64..30.0, minutes in 20u32..60) {
        let mut sim = pair(seed, snr);
        connect(&mut sim);
        sim.run_until(t(minutes as f64 * 60.0));
        prop_assert_eq!(sim.collisions_between(0, 1), 0);
        prop_assert!(!sim.has_event(0, |e| matches!(e, HostEvent::LinkLost { .. })), "link lost");
        prop_assert!(!sim.has_event(1, |e| matches!(e, HostEvent::LinkLost { .. })), "link lost");
        let sounds = sim.frames_from(0).iter().filter(|a| a.frame.as_ref().is_some_and(|f| f.ftype == cairn_link::FrameType::Sound)).count();
        // About one keepalive per 33 s: 30 s idle, a yield window, carrier
        // sense and the exchange itself.
        prop_assert!(sounds as u32 >= minutes * 60 / 36, "{} keepalives in {} minutes", sounds, minutes);
        assert_clean(&sim);
    }
}
