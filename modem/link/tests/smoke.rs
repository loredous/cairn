//! End-to-end smoke tests: connect, transfer both ways, disconnect.

mod common;

use cairn_link::{HostCommand, HostEvent};
use common::*;

#[test]
fn connect_transfer_disconnect() {
    let mut sim = pair(1, 25.0);
    let b = sim.addr(1);
    let data_ab = payload(20_000, 7);
    let data_ba = payload(5_000, 8);
    sim.host(0, t(0.0), HostCommand::Connect { peer: b, new_session: false });
    send_chunks(&mut sim, 0, t(0.1), &data_ab, 1000);
    send_chunks(&mut sim, 1, t(2.0), &data_ba, 700);
    sim.run_while(t(120.0), |s| {
        s.delivered_bytes(1).len() < data_ab.len() || s.delivered_bytes(0).len() < data_ba.len()
    });
    assert_eq!(sim.delivered_bytes(1), data_ab);
    assert_eq!(sim.delivered_bytes(0), data_ba);
    let done = sim.now;
    sim.host(0, done, HostCommand::Disconnect { graceful: true });
    sim.run_until(done + std::time::Duration::from_secs(20));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::Disconnected { .. })));
    assert!(!sim.has_event(0, |e| matches!(e, HostEvent::Undelivered { .. })));
    assert!(sim.invalid_frames.is_empty(), "{:?}", sim.invalid_frames);
    assert!(sim.csma_violations.is_empty(), "{:?}", sim.csma_violations);
    assert!(sim.turn_holder_overlaps.is_empty(), "{:?}", sim.turn_holder_overlaps);
    assert_eq!(sim.mask_mismatches, 0);
}
