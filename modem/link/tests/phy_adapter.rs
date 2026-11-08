//! The adapter between the link layer's frames and `cairn_phy::fields`:
//! field round trips through the PHY's bit packing, `check_controls` on
//! every frame the stations send, and frames carried over the real
//! waveform and decoded back.
#![cfg(feature = "phy")]

mod common;

use cairn_link::adapter::{FieldCodec, PhyCodec, control_from_phy, control_to_phy, header_from_phy, header_to_phy};
use cairn_link::params::ALL_TONES;
use cairn_link::{
    AckBlock, Address, BlockMap, ConnectBlock, ControlBlock, Flags, FrameType, HostCommand, IdBlock, Instant,
    LinkFrame, Measurement, RxFrame, Seq, TxParams,
};
use cairn_phy::fields as pf;
use cairn_phy::params::CP_SHORT;
use cairn_phy::rx::{Receiver, RxOptions};
use cairn_phy::tx::Transmitter;
use common::*;

fn a(call: &str) -> Address {
    Address::callsign(call, 3).unwrap()
}

fn frame(ftype: FrameType, flags: Flags, seq: u16, controls: Vec<ControlBlock>, ndata: usize, mcs: u8) -> LinkFrame {
    LinkFrame {
        ftype,
        src: a("N0AAA"),
        dst: if matches!(ftype, FrameType::Beacon | FrameType::Ui) { Address::BROADCAST } else { a("N0BBB") },
        flags,
        seq: Seq(seq),
        mcs,
        mask_id: 0,
        controls,
        data: (0..ndata).map(|i| [i as u8; 126]).collect(),
        tx: TxParams::default(),
    }
}

fn ack(base: u16, bitmap: u128, mask_id: u8) -> ControlBlock {
    ControlBlock::Ack(AckBlock {
        mask_present: true,
        reprobe: true,
        long_prefix: false,
        base: Seq(base),
        bitmap,
        requested_mcs: 4,
        snr: 81,
        block_errors: 2,
        mask: ALL_TONES & !(1 << 9),
        distortion: 3,
        adc_clipped: true,
        mask_id,
    })
}

fn id(grid: Option<&str>) -> ControlBlock {
    let g = grid.map(|g| {
        let mut b = [b' '; 6];
        b[..g.len()].copy_from_slice(g.as_bytes());
        b
    });
    ControlBlock::Id(IdBlock::new("N0AAA/7", 3, g).unwrap())
}

fn b3(resume: bool, mcs: u8) -> ControlBlock {
    ControlBlock::Connect(ConnectBlock {
        more_dicts: false,
        resume,
        session_id: 0xBEEF,
        capabilities: 0,
        width_cap: 23,
        requested_mcs: mcs,
        max_frame_time: 40,
        mask: ALL_TONES,
        dict_ids: [0x1234_5678, 0, 0, 0],
    })
}

fn samples() -> Vec<LinkFrame> {
    let s = Flags { session: true, ..Default::default() };
    let sp = Flags { session: true, probe: true, ..Default::default() };
    let mut data = frame(
        FrameType::Data,
        s,
        65533,
        vec![ack(10, 0b1010, 1), ControlBlock::BlockMap(BlockMap { base: Seq(65533), bitmap: 0b1101 }), id(None)],
        3,
        3,
    );
    data.mask_id = 2;
    // A CAL-REQUEST block from the PHY, carried opaquely.
    let cal = control_from_phy(&pf::ControlBlock::CalRequest(pf::CalRequestBlock {
        tone_freq: 100,
        step_count: 8,
        step_length: 40,
        gap_length: 10,
        start_level: 96,
        step_size: 3,
    }))
    .unwrap();
    vec![
        data,
        frame(FrameType::Ack, s, 7, vec![ack(0, 0, 0), id(Some("FN31"))], 0, 0),
        frame(FrameType::Ack, s, 7, vec![ack(0, 0, 0), cal], 0, 0),
        frame(FrameType::Sound, sp, 7, vec![ack(3, 0, 3)], 0, 0),
        frame(FrameType::Connect, Flags { probe: true, ..Default::default() }, 0, vec![b3(false, 0), id(None)], 0, 0),
        frame(FrameType::ConnectAck, sp, 12, vec![b3(true, 5), id(Some("FN31PR")), ack(40, 0b110, 0)], 0, 0),
        frame(FrameType::Disconnect, Flags::default(), 0, vec![ControlBlock::Ack(AckBlock::zero()), id(None)], 0, 0),
        frame(FrameType::Probe, Flags::default(), 4242, vec![], 0, 0),
        frame(
            FrameType::Ui,
            Flags { first_fragment: false, more_fragments: true, repeat: true, ..Default::default() },
            65535,
            vec![],
            2,
            1,
        ),
        frame(FrameType::Beacon, Flags { first_fragment: true, ..Default::default() }, 9, vec![id(Some("FN31"))], 0, 0),
    ]
}

#[test]
fn fields_round_trip_through_phy_bits() {
    for f in samples() {
        f.validate().unwrap_or_else(|e| panic!("{e:?}: {f:?}"));
        let h = header_to_phy(&f).unwrap();
        let back = pf::Header::unpack(&h.bits().unwrap()).unwrap();
        assert_eq!(back, h);
        let rh = header_from_phy(&back);
        let want = RxFrame::perfect(&f, Measurement::default(), Instant::ZERO).header;
        assert_eq!(rh, want, "{:?}", f.ftype);
        let controls: Vec<pf::ControlBlock> = f.controls.iter().map(|c| control_to_phy(c).unwrap()).collect();
        pf::check_controls(&h, &controls).unwrap_or_else(|e| panic!("{:?}: {e}", f.ftype));
        for (c, p) in f.controls.iter().zip(&controls) {
            let unpacked = pf::ControlBlock::unpack(&p.bits().unwrap()).unwrap();
            assert_eq!(&unpacked, p);
            assert_eq!(&control_from_phy(&unpacked).unwrap(), c);
        }
        // Block sequences agree.
        let seqs: Vec<u16> = f.data_seqs().iter().map(|s| s.0).collect();
        let opt: Vec<Option<pf::ControlBlock>> = controls.iter().cloned().map(Some).collect();
        if !f.data.is_empty() {
            assert_eq!(pf::block_sequences(&h, &opt), Some(seqs));
        }
    }
}

#[test]
fn reserved_types_come_back_opaque_and_are_refused_on_send() {
    let msg: Vec<u8> = (0..240).map(|i| if i < 4 { [1, 0, 0, 0][i] } else { (i % 3 == 0) as u8 }).collect();
    let p = pf::ControlBlock::unpack(&cairn_phy::crc::append_crc(&msg)).unwrap();
    let c = control_from_phy(&p).unwrap();
    assert_eq!(c.ctype(), 8);
    assert!(c.is_reserved());
    assert_eq!(control_to_phy(&c).unwrap(), p);
    let f = frame(FrameType::Ack, Flags { session: true, ..Default::default() }, 0, vec![ack(0, 0, 0), c], 0, 0);
    assert!(f.validate().is_err());
}

#[test]
fn every_frame_the_stations_send_passes_the_phy_checks() {
    let mut sim = pair(5, 18.0);
    let peer = sim.addr(1);
    sim.host(0, t(0.0), HostCommand::Connect { peer, new_session: false });
    send_chunks(&mut sim, 0, t(0.1), &payload(30_000, 1), 900);
    send_chunks(&mut sim, 1, t(3.0), &payload(9_000, 2), 900);
    sim.link(0, 1).block_loss = 0.15;
    sim.host(0, t(200.0), HostCommand::Disconnect { graceful: true });
    sim.run_until(t(260.0));
    let codec = PhyCodec;
    let mut n = 0;
    for a in &sim.air {
        let f = a.frame.as_ref().unwrap();
        let p = codec.to_phy(f).unwrap_or_else(|e| panic!("{e:?}: {f:?}"));
        pf::check_block_map(&p.header, &p.controls).unwrap();
        n += 1;
    }
    assert!(n > 40, "{n} frames");
}

#[test]
fn frames_cross_the_real_waveform() {
    // A short session's frames, built by cairn-phy, decoded and converted back.
    let mut sim = pair(6, 25.0);
    let peer = sim.addr(1);
    sim.host(0, t(0.0), HostCommand::Connect { peer, new_session: false });
    send_chunks(&mut sim, 0, t(0.1), &payload(3_000, 3), 1000);
    sim.run_until(t(15.0));
    let codec = PhyCodec;
    let mut tx = Transmitter::new();
    let mut checked = Vec::new();
    for a in sim.air.iter().take(6) {
        let f = a.frame.as_ref().unwrap();
        let p = codec.to_phy(f).unwrap();
        let built = tx.build_frame(&p.header, &p.controls, &p.data, &p.link).unwrap();
        let mut x = vec![0.0; 600];
        x.extend_from_slice(&built.samples);
        x.extend(vec![0.0; 600]);
        let link = p.link.clone();
        let source = move |_: &pf::Header| Some(link.clone());
        let r = Receiver::new().receive_header_first(&x, &source, 600, CP_SHORT, &RxOptions::default());
        assert!(r.header_ok, "{:?}", f.ftype);
        let got = codec.to_link(&r, Measurement::default(), Instant::ZERO).unwrap();
        let want = RxFrame::perfect(f, Measurement::default(), Instant::ZERO);
        assert_eq!(got.header, want.header, "{:?}", f.ftype);
        assert_eq!(got.controls, want.controls, "{:?}", f.ftype);
        assert_eq!(got.data, want.data, "{:?}", f.ftype);
        checked.push(f.ftype);
    }
    assert!(checked.contains(&FrameType::Connect) && checked.contains(&FrameType::Data), "{checked:?}");
}
