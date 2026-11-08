//! ADR 0007 in the Rust modem: every frame whose data uses tones above 23
//! carries the probe; the probe reaches the sender's cap; a broadcast's width
//! comes from its probe; header-first decode of frames built with real
//! headers at several widths, masks and caps (SPEC 5.4, 8.2, 10.4).

use cairn_phy::channel::{Rng, awgn};
use cairn_phy::fields::{self, AckBlock, Address, ControlBlock, Header, IdBlock, frame_type};
use cairn_phy::rx::{extent_samples, width_alternatives};
use cairn_phy::{ConfigError, FrameConfig, LinkState, Receiver, RxOptions, Transmitter, TxFrame, params};

const LEAD: usize = 1500;

fn src() -> Address {
    Address::new("N0CALL", 7).unwrap()
}

fn dst() -> Address {
    Address::new("N1CALL", 2).unwrap()
}

fn id() -> ControlBlock {
    ControlBlock::Id(IdBlock { callsign: "N0CALL".into(), ssid: 7, grid: Some("FN31".into()) })
}

fn ack() -> ControlBlock {
    ControlBlock::Ack(AckBlock {
        base_sequence: 9,
        bitmap: 0xF,
        requested_mcs: 2,
        snr_q: fields::snr_code(12.25),
        ..Default::default()
    })
}

fn payloads(n: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
    (0..n).map(|_| rng.bits(params::DATA_PAYLOAD_BITS)).collect()
}

fn air(f: &TxFrame, snr: Option<f64>, rng: &mut Rng) -> Vec<f64> {
    let mut x = vec![0.0; LEAD];
    x.extend_from_slice(&f.samples);
    x.extend(vec![0.0; LEAD]);
    match snr {
        Some(s) => {
            let p = f.samples.iter().map(|v| v * v).sum::<f64>() / f.samples.len() as f64;
            awgn(&x, s, f.config.width, rng, Some(p))
        }
        None => x,
    }
}

#[test]
fn wide_frame_without_probe_is_refused() {
    assert_eq!(FrameConfig::new(30, 2, 2).unwrap().with_probe(false), Err(ConfigError::NeedsProbe));
    let cfg = FrameConfig::new(30, 2, 2).unwrap();
    assert!(cfg.has_probe && cfg.needs_probe());
    let mut h = Header::new(frame_type::DATA, src(), dst());
    (h.mcs, h.n_data, h.session) = (2, 2, true);
    assert_eq!(FrameConfig::from_header(&h, &LinkState::width(30), 60), Err(ConfigError::NeedsProbe));
    // Data on the base tones only, or no data: no probe needed.
    let masked = FrameConfig::new(56, 2, 2).unwrap().with_mask(Some((0..24).collect())).unwrap();
    assert!(!masked.has_probe && masked.n_tones() == 24);
    assert!(!FrameConfig::new(30, 0, 0).unwrap().with_controls(1).unwrap().has_probe);
    assert!(FrameConfig::new(30, 0, 0).unwrap().with_cap(29).is_err());
}

#[test]
fn probe_reaches_the_cap() {
    let mut rng = Rng::new(17);
    for (width, cap) in [(24, 30), (30, 56), (40, 48)] {
        let usable: Vec<usize> = (0..cap).filter(|&k| k != 9).collect();
        let link = LinkState { width, usable: Some(usable.clone()), cap: Some(cap), ..Default::default() };
        let mut h = Header::new(frame_type::DATA, src(), dst());
        (h.sequence, h.mcs, h.n_control, h.n_data, h.session, h.probe) = (11, 3, 1, 3, true, true);
        let f = Transmitter::new().build_frame(&h, &[ack()], &payloads(3, &mut rng), &link).unwrap();
        let cfg = &f.config;
        assert_eq!((cfg.cap, cfg.n_tones(), f.grid.cols), (cap, cap, cap));
        for s in cfg.first_probe()..cfg.first_control() {
            assert!((0..cap).all(|k| f.grid.at(s, k).norm() > 0.0), "probe on every tone up to the cap");
        }
        for s in cfg.first_data()..cfg.total_symbols() {
            assert!((width..cap).all(|k| f.grid.at(s, k).norm() == 0.0), "data within the width");
        }
        let y = air(&f, Some(25.0), &mut rng);
        let r = Receiver::new().receive_header_first(&y, &link, LEAD, 60, &RxOptions::default());
        assert_eq!(r.config.as_ref(), Some(cfg), "{width}/{cap}");
        assert!(r.block_ok.iter().all(|&b| b) && r.controls == vec![Some(ack())]);
        assert_eq!(r.snr_db.len(), cap);
        assert!(r.snr_db[width..cap].iter().all(|&s| s > 10.0), "tones above the width measured");
        // The simulator's geometry: same samples as the Python transmitter would give is
        // covered by the vectors at cap = width; here the receiver must agree with itself.
        let wrong = LinkState { cap: None, ..link.clone() };
        let rw = Receiver::new().receive_header_first(&y, &wrong, LEAD, 60, &RxOptions::default());
        assert_eq!(rw.config.unwrap().cap, width);
    }
}

#[test]
fn broadcast_width_from_probe_without_link_state() {
    let mut rng = Rng::new(3);
    for (width, mcs) in [(25, 0u8), (30, 1), (40, 2), (56, 3)] {
        let mut h = Header::new(frame_type::UI, src(), Address::Broadcast);
        (h.sequence, h.mcs, h.n_control, h.n_data) = (width as u16, mcs, 1, 3);
        let f = Transmitter::new().build_frame(&h, &[id()], &payloads(3, &mut rng), &LinkState::width(width)).unwrap();
        let sent = f.header.clone().unwrap();
        assert!(sent.probe && !sent.session);
        let y = air(&f, Some(20.0), &mut rng);
        let r = Receiver::new().detect_and_receive(&y, &LinkState::default(), &RxOptions::default()).unwrap();
        let ext = r.probe_extent.clone().expect("broadcast with a probe");
        assert_eq!((ext.width, r.config.as_ref().unwrap().width), (width, width));
        assert_eq!(r.header.as_ref(), Some(&sent));
        assert!(r.block_ok.iter().all(|&b| b) && r.controls == vec![Some(id())]);
        let (inside, outside) = ext.margin_db();
        assert!(inside > 3.0 && outside > 3.0, "{width}: margins {inside} {outside}");
        assert!(extent_samples(&sent, 60) <= f.samples.len());
    }
}

#[test]
fn extent_ignores_clip_regrowth_above_the_cap() {
    let mut rng = Rng::new(1);
    for width in [25, 37, 52, 55] {
        let mut h = Header::new(frame_type::UI, src(), Address::Broadcast);
        (h.mcs, h.n_data) = (6, 2);
        let f = Transmitter::new().build_frame(&h, &[], &payloads(2, &mut rng), &LinkState::width(width)).unwrap();
        let y = air(&f, None, &mut rng);
        let e = Receiver::new().probe_extent(&y, LEAD, 60, f.header.as_ref().unwrap(), &RxOptions::default());
        assert_eq!(e.width, width);
        assert!(e.margin_db().1 > 3.0, "{width}: {:?}", e.margin_db());
    }
}

#[test]
fn extent_across_snr_and_width() {
    let mut rng = Rng::new(5);
    for width in [25, 26, 28, 33, 40, 48, 55, 56] {
        for snr in [3.5, 6.0, 15.0] {
            let mut h = Header::new(frame_type::UI, src(), Address::Broadcast);
            (h.mcs, h.n_data) = (0, 3);
            let f = Transmitter::new().build_frame(&h, &[], &payloads(3, &mut rng), &LinkState::width(width)).unwrap();
            let y = air(&f, Some(snr), &mut rng);
            let e = Receiver::new().probe_extent(&y, LEAD, 60, f.header.as_ref().unwrap(), &RxOptions::default());
            assert_eq!(e.width, width, "{width} tones at {snr} dB: {:?}", e.margin_db());
        }
    }
    assert_eq!(width_alternatives(55), vec![56, 54, 53, 52]);
}

#[test]
fn broadcast_24_tones_has_no_probe() {
    let mut rng = Rng::new(8);
    let mut h = Header::new(frame_type::BEACON, src(), Address::Broadcast);
    h.n_control = 1;
    let f = Transmitter::new().build_frame(&h, &[id()], &[], &LinkState::default()).unwrap();
    assert!(!f.header.as_ref().unwrap().probe);
    let y = air(&f, Some(15.0), &mut rng);
    let r = Receiver::new().receive_header_first(&y, &LinkState::width(40), LEAD, 60, &RxOptions::default());
    assert!(r.probe_extent.is_none() && r.config.unwrap().width == 24 && r.controls == vec![Some(id())]);
}

#[test]
fn header_first_in_session_at_several_widths_and_masks() {
    let mut rng = Rng::new(21);
    let cases: [(usize, Option<Vec<usize>>); 4] = [
        (24, None),
        (30, Some((0..30).filter(|k| ![7, 26].contains(k)).collect())),
        (44, None),
        (56, Some((0..56).filter(|k| ![40, 41].contains(k)).collect())),
    ];
    for (width, usable) in cases {
        let link = LinkState { width, usable, ..Default::default() };
        let mut h = Header::new(frame_type::DATA, src(), dst());
        (h.sequence, h.mcs, h.n_control, h.n_data, h.session) = (0x1234, 2, 1, 4, true);
        let pl = payloads(4, &mut rng);
        let f = Transmitter::new().build_frame(&h, &[ack()], &pl, &link).unwrap();
        let y = air(&f, Some(18.0), &mut rng);
        let opts = RxOptions { threads: 3, ..Default::default() };
        let r = Receiver::new().receive_header_first(&y, &link, LEAD, 60, &opts);
        assert_eq!(r.header, f.header);
        assert_eq!(r.header.as_ref().unwrap().probe, width > 24);
        assert!(r.probe_extent.is_none());
        assert!(r.block_bits.iter().zip(&pl).all(|(b, p)| b[..1008] == p[..]), "{width}");
        assert_eq!(r.controls, vec![Some(ack())]);
    }
}

#[test]
fn threads_do_not_change_results() {
    let mut rng = Rng::new(2);
    let cfg = FrameConfig::new(40, 4, 20).unwrap();
    let pl = payloads(20, &mut rng);
    let f = Transmitter::new().build(&cfg, &rng.bits(128), &pl);
    let y = air(&f, Some(16.0), &mut rng);
    let a = Receiver::new().receive(&y, &cfg, LEAD, &RxOptions::default());
    let b = Receiver::new().receive(&y, &cfg, LEAD, &RxOptions { threads: 4, ..Default::default() });
    assert_eq!(a.block_bits, b.block_bits);
    assert_eq!(a.iterations, b.iterations);
    assert_eq!(a.block_ok, b.block_ok);
}

#[test]
fn link_state_from_cap() {
    let l = LinkState::from_cap(40, None);
    assert_eq!((l.width, l.probe_extent()), (40, 40));
}

#[test]
fn width_alternatives_reach_every_wider_width() {
    assert_eq!(width_alternatives(55), vec![56, 54, 53, 52]);
    let mut want = vec![39, 37, 40, 36, 41, 35];
    want.extend(42..=56);
    assert_eq!(width_alternatives(38), want);
    let mut want = vec![25, 26, 27];
    want.extend(28..=56);
    assert_eq!(width_alternatives(24), want);
}

#[test]
fn width_unconfirmed_when_no_block_decodes() {
    // The data symbols lost (the channel's noise alone after the probe): no
    // width decodes them, so the width is reported unknown (data_width None),
    // not taken as the probe's.
    let mut rng = Rng::new(9);
    let mut h = Header::new(frame_type::UI, src(), Address::Broadcast);
    (h.sequence, h.mcs, h.n_data) = (6, 0, 2);
    let f = Transmitter::new().build_frame(&h, &[], &payloads(2, &mut rng), &LinkState::width(36)).unwrap();
    let clean = air(&f, None, &mut rng);
    let mut y = air(&f, Some(20.0), &mut rng);
    let d0 = LEAD + f.config.first_data() * f.config.stride();
    for (v, c) in y[d0..].iter_mut().zip(&clean[d0..]) {
        *v -= c;
    }
    let r = Receiver::new().receive_header_first(&y, &LinkState::default(), LEAD, 60, &RxOptions::default());
    let ext = r.probe_extent.as_ref().unwrap();
    assert_eq!((ext.width, ext.data_width, r.config.as_ref().unwrap().width), (36, None, 36));
    assert!(!r.block_ok.iter().any(|&b| b));
    // A frame whose data decodes confirms its width.
    let y = air(&f, Some(20.0), &mut rng);
    let r = Receiver::new().receive_header_first(&y, &LinkState::default(), LEAD, 60, &RxOptions::default());
    assert_eq!(r.probe_extent.unwrap().data_width, Some(36));
}
