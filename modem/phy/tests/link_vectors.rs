//! The link-layer additions of ADRs 0008-0012 on the PHY side: the PDU
//! vectors of `vectors/link/` bit for bit (ADR 0009), and frames built and
//! received with block maps, masks named by ID through a link-state
//! callback, the session width and the probe extents (ADR 0008, ADR 0011,
//! ADR 0012). The block map control blocks and link frames are checked bit
//! for bit in `field_vectors.rs` and `bitmap_vectors.rs`.

mod common;

use cairn_phy::fields::{
    self, ALL_TONES, AckBlock, BlockMapBlock, ConnectBlock, ControlBlock, FieldError, Header, IdBlock, capability,
    frame_type,
};
use cairn_phy::pdu::{self, Block, Pdu, Reassembler};
use cairn_phy::{ConfigError, FrameConfig, LinkState, Receiver, RxOptions, Transmitter, params};
use common::*;
use serde_json::Value;

fn hex(s: &Value) -> Vec<u8> {
    let s = s.as_str().unwrap();
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn block(v: &Value) -> Block {
    hex(v).try_into().unwrap()
}

fn pdu_of(p: &Value) -> Pdu {
    Pdu {
        pdu_type: u(&p["type"]) as u8,
        stream: u(&p["stream"]) as u8,
        encoding: u(&p["encoding"]) as u8,
        dictionary_id: p["dictionary_id_hex"].as_str().map(|_| hex(&p["dictionary_id_hex"]).try_into().unwrap()),
        body: hex(&p["body_hex"]),
        crc: u32::from_str_radix(p["crc32_hex"].as_str().unwrap(), 16).unwrap(),
    }
}

fn link_doc(name: &str) -> Value {
    read_json(&repo().join("vectors/link").join(format!("{name}.json")))
}

// --- PDU packing (ADR 0009) -----------------------------------------------------------

#[test]
fn pdu_blocks_vector() {
    let d = link_doc("pdu_blocks");
    assert_eq!(format!("{:08x}", pdu::crc32(b"123456789")), d["crc32_check"]["crc32_hex"].as_str().unwrap());
    let decoded = hex(&d["decoded_hex"]);
    let pdus: Vec<Pdu> = d["pdus"].as_array().unwrap().iter().map(pdu_of).collect();
    let want: Vec<Block> = d["blocks_hex"].as_array().unwrap().iter().map(block).collect();
    let mut blocks = Vec::new();
    let mut groups: Vec<(u64, Vec<Pdu>)> = Vec::new();
    for (p, v) in pdus.iter().zip(d["pdus"].as_array().unwrap()) {
        assert_eq!(to_hex(&p.encode().unwrap()), v["pdu_hex"].as_str().unwrap());
        assert_eq!(p.length() as u64, u(&v["length"]));
        assert_eq!(Pdu::decode(&hex(&v["pdu_hex"])).unwrap(), *p);
        assert!(p.crc_ok());
        if p.encoding != 0 {
            assert_eq!(pdu::crc32(&decoded), p.crc, "the CRC-32 covers the decoded body");
        }
        match groups.last_mut() {
            Some((g, list)) if *g == u(&v["group"]) => list.push(p.clone()),
            _ => groups.push((u(&v["group"]), vec![p.clone()])),
        }
    }
    for (_, g) in &groups {
        blocks.extend(pdu::pack_blocks(g).unwrap());
    }
    assert_eq!(blocks, want, "packed blocks");
    let back: Vec<Pdu> = pdu::unpack_blocks(&want).into_iter().map(Result::unwrap).collect();
    assert_eq!(back, pdus, "parsed PDUs");
    for (b, info) in want.iter().zip(d["block_info_hex"].as_array().unwrap()) {
        // B.4: the payload bits, bytes MSB first, then the CRC-16.
        let bits = cairn_phy::crc::append_crc(&pdu::block_bits(b));
        assert_eq!(bits_to_hex(&bits), info.as_str().unwrap());
        assert_eq!(pdu::bits_to_payload(&bits), *b);
    }
}

#[test]
fn pdu_broadcast_fragments_vector() {
    let d = link_doc("pdu_broadcast_fragments");
    let message = hex(&d["message_hex"]);
    let frames = d["frames"].as_array().unwrap();
    let stream = u(&frames[0]["pdu"]["stream"]) as u8;
    let frags = pdu::fragment(pdu::pdu_type::MESSAGE, &message, u(&d["slice_bytes"]) as usize, stream).unwrap();
    assert_eq!(frags.len(), frames.len());
    let mut r = Reassembler::new();
    let mut done = None;
    for (f, v) in frags.iter().zip(frames) {
        assert_eq!((f.first, f.more), (b(&v["first_fragment"]), b(&v["more_fragments"])));
        assert_eq!(f.pdu, pdu_of(&v["pdu"]));
        let blocks = pdu::pack_blocks(std::slice::from_ref(&f.pdu)).unwrap();
        assert_eq!(blocks, v["blocks_hex"].as_array().unwrap().iter().map(block).collect::<Vec<_>>());
        let h = header(&v["header"]);
        assert_eq!((h.first_fragment, h.more_fragments, h.n_data), (f.first, f.more, blocks.len()));
        assert_eq!(bits_to_hex(&h.bits().unwrap()), v["header_info_hex"].as_str().unwrap());
        done = r.feed("N0CALL-1", h.sequence, h.first_fragment, h.more_fragments, f.pdu.clone());
    }
    let done = done.expect("message complete");
    assert_eq!(done.body, message);
    assert_eq!(done, pdu_of(&d["reassembled"]));
}

#[test]
fn pdu_rules() {
    // The one-byte rule and filler.
    let first = Pdu::raw(1, &[0; 116], 0);
    let second = Pdu::raw(1, b"0123456789", 0);
    let blocks = pdu::pack_blocks(&[first.clone(), second.clone()]).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0][125], 0);
    assert_eq!(&blocks[1][..19], &second.encode().unwrap()[..]);
    assert!(blocks[1][19..].iter().all(|&x| x == 0));
    let mut odd = blocks[0];
    odd[125] = 0x55; // a parser skips the lone byte whatever it holds
    let back: Vec<Pdu> = pdu::unpack_blocks(&[odd, blocks[1]]).into_iter().map(Result::unwrap).collect();
    assert_eq!(back, vec![first, second]);
    // Random streams in two runs, filler between.
    let mut rng = cairn_phy::channel::Rng::new(3);
    for _ in 0..30 {
        let n = 1 + (rng.next_u64() % 7) as usize;
        let pdus: Vec<Pdu> = (0..n)
            .map(|_| {
                let len = (rng.next_u64() % 400) as usize;
                let body: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                Pdu::raw((rng.next_u64() % 7) as u8, &body, rng.next_u64() as u8)
            })
            .collect();
        let cut = (rng.next_u64() as usize) % (n + 1);
        let mut blocks = pdu::pack_blocks(&pdus[..cut]).unwrap();
        blocks.extend(pdu::pack_blocks(&pdus[cut..]).unwrap());
        let back: Vec<Pdu> = pdu::unpack_blocks(&blocks).into_iter().map(Result::unwrap).collect();
        assert_eq!(back, pdus);
    }
    // Errors.
    assert_eq!(Pdu { encoding: 2, ..Pdu::raw(0, b"", 0) }.encode(), Err(pdu::PduError::DictionaryId));
    assert!(matches!(Pdu::raw(0, &vec![0; 65535], 0).encode(), Err(pdu::PduError::TooLong(_))));
    assert_eq!(Pdu::decode(&[0, 3, 1, 2, 3]), Err(pdu::PduError::Short(3)));
    assert_eq!(pdu::stream_byte(1, 3), Ok(0x43));
    assert_eq!(pdu::max_body(1, false), 117);
    // Reassembly: a gap discards, a continuation without a start is dropped.
    let frags = pdu::fragment(1, &[7; 700], 300, 0).unwrap();
    let mut r = Reassembler::new();
    assert_eq!(r.feed(1u8, 10, true, true, frags[0].pdu.clone()), None);
    assert_eq!(r.feed(1u8, 12, false, true, frags[1].pdu.clone()), None);
    assert_eq!(r.feed(1u8, 13, false, false, frags[2].pdu.clone()), None);
    assert_eq!(r.feed(2u8, 65535, true, true, frags[0].pdu.clone()), None);
    assert_eq!(r.feed(2u8, 0, false, true, frags[1].pdu.clone()), None);
    assert_eq!(r.feed(2u8, 1, false, false, frags[2].pdu.clone()).unwrap().body, vec![7; 700]);
}

// --- fields (ADR 0008, ADR 0010, ADR 0011, ADR 0012) --------------------------------------

#[test]
fn cumulative_ack_and_block_plans() {
    let (base, bm) = AckBlock::cumulative(65534, &[65535, 0, 3, 65534, 200, 65533]);
    assert_eq!((base, bm), (65534, 1 << 1 | 1 << 2 | 1 << 5));
    let a = AckBlock { base_sequence: base, bitmap: bm, ..Default::default() };
    assert!(a.acknowledged(65533) && !a.acknowledged(65534) && a.acknowledged(0) && !a.acknowledged(1));
    assert_eq!(fields::serial_diff(65535, 0), 1);
    assert_eq!(fields::plan_blocks(&[10, 11, 12]).unwrap(), (10, None));
    assert_eq!(fields::plan_blocks(&[65535, 0]).unwrap(), (65535, None));
    let (s, m) = fields::plan_blocks(&[7, 9, 10]).unwrap();
    assert_eq!((s, m.unwrap().sequences()), (7, vec![7, 9, 10]));
    for bad in [&[][..], &[3, 3], &[5, 4], &[0, 128]] {
        assert!(matches!(fields::plan_blocks(bad), Err(FieldError::Rule(_))), "{bad:?}");
    }
    let h = Header {
        sequence: 100,
        n_control: 2,
        n_data: 3,
        session: true,
        block_map: true,
        ..Header::new(frame_type::DATA, station(1), station(2))
    };
    let map = |base, bitmap| Some(ControlBlock::BlockMap(BlockMapBlock { base_sequence: base, bitmap }));
    let ack = Some(ControlBlock::Ack(AckBlock::default()));
    assert_eq!(
        fields::block_sequences(&h, &[ack.clone(), map(100, 0b1 | 1 << 4 | 1 << 10)]),
        Some(vec![100, 104, 110])
    );
    for bad in [None, map(101, 0b111), map(100, 0b110 | 1 << 9), map(100, 0b1111)] {
        assert_eq!(fields::block_sequences(&h, &[ack.clone(), bad]), None);
    }
    // Dictionary-only B.3.
    let d = ControlBlock::Connect(ConnectBlock::dictionaries([1, 2, 3, 4]));
    assert_eq!(ControlBlock::unpack(&d.bits().unwrap()).unwrap(), d);
    for (cap, mft) in [(0, 40), (23, 0), (10, 40)] {
        let c = ControlBlock::Connect(ConnectBlock { width_cap: cap, max_frame_time: mft, ..Default::default() });
        assert!(c.bits().is_err());
    }
}

fn station(ssid: u8) -> cairn_phy::Address {
    cairn_phy::Address::new("N0CALL", ssid).unwrap()
}

fn hdr(t: u8, n_control: usize) -> Header {
    Header { n_control, ..Header::new(t, station(1), station(2)) }
}

#[test]
fn control_orders() {
    let b3 = ControlBlock::Connect(ConnectBlock {
        session_id: 7,
        capabilities: capability::WIDE,
        width_cap: 29,
        ..Default::default()
    });
    let id = ControlBlock::Id(IdBlock { callsign: "N0CALL".into(), ssid: 1, grid: None });
    let ack = ControlBlock::Ack(AckBlock { tone_mask: Some(ALL_TONES), ..Default::default() });
    let resume = ControlBlock::Connect(ConnectBlock { resume: true, ..Default::default() });
    let map = ControlBlock::BlockMap(BlockMapBlock::for_sequences(&[0, 2]).unwrap());
    let probe_b3 = ControlBlock::Connect(ConnectBlock::link_test(capability::WIDE, 39, 40));
    let reply = ControlBlock::Ack(AckBlock { requested_mcs: 3, tone_mask: Some(0xFF_FFFF), ..Default::default() });
    let session = |t, n| Header { session: true, ..hdr(t, n) };
    let ok: Vec<(Header, Vec<ControlBlock>)> = vec![
        (Header { probe: true, ..hdr(frame_type::CONNECT, 2) }, vec![b3.clone(), id.clone()]),
        (session(frame_type::CONNECT_ACK, 3), vec![resume.clone(), id.clone(), ack.clone()]),
        (hdr(frame_type::PROBE, 0), vec![]),
        (Header { probe: true, ..hdr(frame_type::PROBE, 1) }, vec![probe_b3.clone()]),
        (hdr(frame_type::SOUND, 1), vec![reply.clone()]),
        (hdr(frame_type::DISCONNECT, 2), vec![ControlBlock::Ack(AckBlock::default()), id.clone()]),
        (
            Header { n_data: 2, block_map: true, mask_id: 3, ..session(frame_type::DATA, 3) },
            vec![ack.clone(), map.clone(), id.clone()],
        ),
    ];
    for (h, c) in &ok {
        fields::check_controls(h, c).unwrap_or_else(|e| panic!("{h:?}: {e}"));
    }
    let bad: Vec<(Header, Vec<ControlBlock>)> = vec![
        (hdr(frame_type::CONNECT, 2), vec![id.clone(), b3.clone()]),
        (session(frame_type::CONNECT_ACK, 3), vec![b3.clone(), id.clone(), ack.clone()]),
        (Header { mask_id: 1, ..session(frame_type::CONNECT_ACK, 2) }, vec![b3.clone(), id.clone()]),
        (Header { probe: true, ..hdr(frame_type::PROBE, 0) }, vec![]),
        (hdr(frame_type::PROBE, 1), vec![b3.clone()]),
        (session(frame_type::DISCONNECT, 1), vec![ack.clone()]),
        (Header { n_data: 2, ..session(frame_type::DATA, 2) }, vec![ack.clone(), map.clone()]),
        (Header { n_data: 2, block_map: true, ..session(frame_type::DATA, 2) }, vec![map.clone(), ack.clone()]),
        (Header { n_data: 1, first_fragment: true, ..session(frame_type::DATA, 1) }, vec![ack.clone()]),
    ];
    for (h, c) in &bad {
        assert!(fields::check_controls(h, c).is_err(), "{h:?} {c:?}");
    }
}

// --- frames: block maps, masks by ID, width and probe extent (ADR 0008, ADR 0011) ----------

fn padded(x: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0; 600];
    y.extend_from_slice(x);
    y.extend(vec![0.0; 600]);
    y
}

#[test]
fn block_map_frame_with_mask_callback() {
    let mut rng = cairn_phy::channel::Rng::new(11);
    let seqs = [65534u16, 1, 2, 9];
    let (seq, map) = fields::plan_blocks(&seqs).unwrap();
    let map = ControlBlock::BlockMap(map.unwrap());
    let m1: Vec<usize> = (0..30).filter(|&k| k != 11).collect();
    let own = ConnectBlock { capabilities: capability::WIDE, width_cap: 29, ..Default::default() };
    let tx_link = LinkState::session(&own, capability::WIDE, Some(m1.clone()), 1);
    assert_eq!((tx_link.width, tx_link.probe_extent()), (30, 30));
    let h = Header {
        sequence: seq,
        mcs: 2,
        n_control: 2,
        n_data: 4,
        session: true,
        block_map: true,
        mask_id: 1,
        ..Header::new(frame_type::DATA, station(1), station(2))
    };
    let (base, bitmap) = AckBlock::cumulative(3, &[]);
    let ack =
        ControlBlock::Ack(AckBlock { base_sequence: base, bitmap, tone_mask: Some(ALL_TONES), ..Default::default() });
    let payloads: Vec<Block> = (0..4).map(|_| std::array::from_fn(|_| rng.next_u64() as u8)).collect();
    let bits: Vec<Vec<u8>> = payloads.iter().map(pdu::block_bits).collect();
    let mut tx = Transmitter::new();
    let f = tx.build_frame(&h, &[ack.clone(), map.clone()], &bits, &tx_link).unwrap();
    let sent = f.header.clone().unwrap();
    assert!(sent.probe && f.config.n_tones() == 30 && f.config.masked == vec![11]);
    let x = padded(&f.samples);
    let opts = RxOptions::default();

    // The receiver holds mask 0 (all usable) and the pending mask 1.
    let seen = std::cell::RefCell::new(Vec::new());
    let source = |hd: &Header| {
        seen.borrow_mut().push(hd.clone());
        Some(LinkState { width: 30, pending: Some((1, Some(m1.clone()))), ..LinkState::from_cap(30, None) })
    };
    let r = Receiver::new().receive_header_first(&x, &source, 600, params::CP_SHORT, &opts);
    assert_eq!(*seen.borrow(), vec![sent.clone()]);
    assert!(r.block_ok.iter().all(|&v| v) && r.control_ok.iter().all(|&v| v));
    assert_eq!(r.block_sequences.as_deref(), Some(&seqs[..]));
    let got = r.delivered();
    assert_eq!(got.iter().map(|(s, _)| *s).collect::<Vec<_>>(), seqs);
    assert_eq!(got.iter().map(|(_, p)| *p).collect::<Vec<_>>(), payloads);

    // A mask ID the receiver does not hold: header only, every block failed.
    let r = Receiver::new().receive_header_first(&x, &LinkState::from_cap(30, None), 600, params::CP_SHORT, &opts);
    assert_eq!((r.header.as_ref(), r.unknown_mask), (Some(&sent), Some(1)));
    assert_eq!((r.block_ok.len(), r.control_ok.len()), (4, 2));
    assert!(!r.block_ok.iter().chain(&r.control_ok).any(|&v| v) && r.delivered().is_empty());
    assert_eq!(r.block_sequences, None);
    assert_eq!(FrameConfig::from_header(&sent, &LinkState::from_cap(30, None), 60), Err(ConfigError::UnknownMask(1)));

    // Flag bit 7 set and the block map breaks its rules (a fifth bit, sent
    // with a good CRC through the raw builder): nothing delivered. A map
    // that fails its CRC is covered by sim/tests/test_link_fields.py.
    let mut msgs: Vec<Vec<u8>> = f.control_bits.iter().map(|b| b[..240].to_vec()).collect();
    msgs[1][30] ^= 1;
    let broken = tx.build_with_controls(&f.config, &f.header_bits[..128], &msgs, &bits);
    let r = Receiver::new().receive_header_first(&padded(&broken.samples), &source, 600, params::CP_SHORT, &opts);
    assert!(matches!(&r.controls[1], Some(ControlBlock::BlockMap(m)) if m.bitmap.count_ones() == 5));
    assert!(r.block_ok.iter().all(|&v| v));
    assert_eq!(r.block_sequences, None);
    assert!(r.delivered().is_empty());

    // The builder refuses a map that disagrees with the header.
    let wrong = ControlBlock::BlockMap(BlockMapBlock::for_sequences(&[65535, 1, 2, 9]).unwrap());
    assert!(tx.build_frame(&h, &[ack.clone(), wrong], &bits, &tx_link).is_err());
    assert!(tx.build_frame(&Header { block_map: false, ..h.clone() }, &[ack, map], &bits, &tx_link).is_err());
}

#[test]
fn connect_ack_unmasked_and_probe_extents() {
    // CONNECT-ACK: unmasked whatever the link's mask, probe to W; with no
    // link state (None from the callback) its width comes from the probe.
    let b3 = ConnectBlock {
        session_id: 1,
        capabilities: capability::WIDE,
        width_cap: 35,
        requested_mcs: 3,
        ..Default::default()
    };
    let id = IdBlock { callsign: "N0CALL".into(), ssid: 2, grid: None };
    let h = Header {
        n_control: 2,
        session: true,
        probe: true,
        ..Header::new(frame_type::CONNECT_ACK, station(2), station(1))
    };
    let masked: Vec<usize> = (0..36).filter(|&k| k != 4).collect();
    let link = LinkState::session(&b3, capability::WIDE, Some(masked), 0);
    let mut tx = Transmitter::new();
    let f = tx.build_frame(&h, &[ControlBlock::Connect(b3.clone()), ControlBlock::Id(id)], &[], &link).unwrap();
    assert!(f.config.masked.is_empty() && f.config.n_tones() == 36);
    let none = |_: &Header| None;
    let r =
        Receiver::new().receive_header_first(&padded(&f.samples), &none, 600, params::CP_SHORT, &RxOptions::default());
    assert!(r.control_ok.iter().all(|&v| v));
    assert_eq!(r.probe_extent.as_ref().unwrap().width, 36);

    // A link-test PROBE probes tones 0 ..= its width cap (ADR 0011, ADR 0012).
    let lt = ConnectBlock::link_test(capability::WIDE, 47, 40);
    let h =
        Header { sequence: 99, n_control: 1, probe: true, ..Header::new(frame_type::PROBE, station(1), station(2)) };
    fields::check_controls(&h, &[ControlBlock::Connect(lt.clone())]).unwrap();
    let f = tx.build_frame(&h, &[ControlBlock::Connect(lt.clone())], &[], &LinkState::probe_to(47)).unwrap();
    assert_eq!(f.config.n_tones(), 48);
    let r =
        Receiver::new().detect_and_receive(&padded(&f.samples), &LinkState::default(), &RxOptions::default()).unwrap();
    assert_eq!(r.controls, vec![Some(ControlBlock::Connect(lt))]);
    assert_eq!(r.probe_extent.as_ref().unwrap().width, 48);

    // Session width: cap + 1 only when the peer is wide.
    assert_eq!(LinkState::session(&b3, 0, None, 0).width, 24);
    assert_eq!(fields::session_width(55, capability::WIDE), 56);
    let bad = LinkState { mask_id: 1, pending: Some((1, None)), ..Default::default() };
    let data = Header { session: true, ..Header::new(frame_type::DATA, station(1), station(2)) };
    assert_eq!(bad.mask_for(&data), Err(ConfigError::MaskIds));
}
