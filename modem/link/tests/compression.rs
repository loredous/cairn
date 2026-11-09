//! Compressed PDUs through a session on the simulated air of `common`
//! (SPEC §12.2, ADR 0014 `zstd-frame-format`): dictionaries negotiated in
//! CONNECT/CONNECT-ACK (with the second, dictionary-only B.3 block), MESSAGE
//! and FILE-CHUNK PDUs in encodings 1 and 2 delivered exactly once and
//! decoded, the negotiated set kept across a resume, and compressed
//! broadcast fragments reassembled from their decoded slices.
#![cfg(feature = "zstd")]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use cairn_link::compress::{Codec, Dictionaries, Dictionary, DictionaryId, Preference, id_from_u32, id_to_u32};
use cairn_link::pdu::{FragmentRole, Pdu, Reassembler, encoding, pdu_type, unpack_frame};
use cairn_link::{Address, ControlBlock, FrameType, HostCommand, HostEvent, Instant};
use common::*;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The 64 kB evaluation dictionary, and `n` raw-content stand-ins.
fn dictionaries(extra: &[&str]) -> (Dictionary, Vec<Dictionary>) {
    let eval = Dictionary::load(&repo().join("dictionaries/eval/eval-forms-64k.zdict")).unwrap();
    let others = extra.iter().map(|s| Dictionary::new(*s, s.repeat(64).into_bytes())).collect();
    (eval, others)
}

fn set(ds: &[&Dictionary]) -> Dictionaries {
    let mut s = Dictionaries::new();
    for d in ds {
        s.insert((*d).clone());
    }
    s
}

/// Messages of 100-500 bytes cut from public-domain NWS text in the corpus
/// (training material for the evaluation dictionary, so encoding 2 wins).
fn messages() -> Vec<Vec<u8>> {
    let dir = repo().join("dictionaries/corpus/nws");
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    let mut out = Vec::new();
    for (i, p) in names.iter().take(12).enumerate() {
        let text = std::fs::read(p).unwrap();
        let n = 100 + (i * 37) % 400;
        out.push(text[..n.min(text.len())].to_vec());
    }
    out
}

fn connect(sim: &mut Sim, a: usize, b: usize, at: Instant) {
    let peer = sim.addr(b);
    sim.host(a, at, HostCommand::Connect { peer, new_session: false });
}

#[test]
fn compressed_messages_and_file_chunks_cross_a_session() {
    let (eval, others) = dictionaries(&["alpha ", "bravo ", "charlie ", "delta ", "echo "]);
    // A offers five (a second B.3 block); B has two, one in common.
    let a_set = set(&[&others[0], &others[1], &others[2], &others[3], &eval]);
    let b_set = set(&[&others[4], &eval]);
    let mut sim = Sim::new(5);
    let mut a = cfg("N0AAA", 51);
    a.dict_ids = Codec::new(a_set.clone()).offered_ids();
    let mut b = cfg("N0BBB", 52);
    b.dict_ids = Codec::new(b_set.clone()).offered_ids();
    sim.add(a, Duration::from_millis(150));
    sim.add(b, Duration::from_millis(150));
    sim.set_snr(0, 1, 25.0);
    connect(&mut sim, 0, 1, t(0.0));
    sim.run_while(t(30.0), |s| !s.has_event(0, |e| matches!(e, HostEvent::Connected { .. })));

    // The setup frames: A's CONNECT lists all five over two B.3 blocks,
    // B's CONNECT-ACK the one they share.
    let setup = |st: usize, ft: FrameType| {
        sim.frames_from(st).iter().find_map(|x| x.frame.clone().filter(|f| f.ftype == ft)).expect("setup frame")
    };
    let connect_frame = setup(0, FrameType::Connect);
    assert_eq!(connect_frame.controls.len(), 3, "B.3, ID, dictionary-only B.3");
    match (&connect_frame.controls[0], &connect_frame.controls[2]) {
        (ControlBlock::Connect(first), ControlBlock::Connect(second)) => {
            assert!(first.more_dicts);
            assert_eq!(second.dict_ids, [id_to_u32(eval.id()), 0, 0, 0]);
            assert_eq!((second.width_cap, second.max_frame_time), (0, 0), "the dictionary-only marker");
        }
        c => panic!("{c:?}"),
    }
    let ack_frame = setup(1, FrameType::ConnectAck);
    assert_eq!(ack_frame.controls.len(), 2);
    match &ack_frame.controls[0] {
        ControlBlock::Connect(b3) => assert_eq!(b3.dict_ids, [id_to_u32(eval.id()), 0, 0, 0]),
        c => panic!("{c:?}"),
    }
    let ia = sim.stations[0].st.session_info().unwrap();
    let ib = sim.stations[1].st.session_info().unwrap();
    assert_eq!(ia.dictionaries, vec![id_to_u32(eval.id())]);
    assert_eq!(ib.dictionaries, vec![id_to_u32(eval.id())]);
    assert_eq!(ib.peer_dictionaries.len(), 5);

    // A's host compresses against the negotiated set and queues.
    let usable: Vec<DictionaryId> = ia.dictionaries.iter().map(|&d| id_from_u32(d)).collect();
    let mut tx = Codec::new(a_set);
    let mut sent: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut id = 0u64;
    for m in messages() {
        let pdu = tx.encode(pdu_type::MESSAGE, 0, m.clone(), Preference::Auto, &usable).unwrap();
        sim.host(0, sim.now, HostCommand::Send { id, pdu });
        sent.push((pdu_type::MESSAGE, m));
        id += 1;
    }
    let file = std::fs::read(repo().join("dictionaries/corpus/ecfr_part97/part97.txt")).unwrap();
    for (k, chunk) in file[..12_000].chunks(2048).enumerate() {
        let mut body = 7u32.to_be_bytes().to_vec();
        body.extend_from_slice(&((k * 2048) as u32).to_be_bytes());
        body.extend_from_slice(chunk);
        let pdu = tx.encode(pdu_type::FILE_CHUNK, 3 << 6, body.clone(), Preference::Auto, &usable).unwrap();
        assert_ne!(pdu.encoding, encoding::RAW, "a 2 kB text chunk compresses");
        sim.host(0, sim.now, HostCommand::Send { id, pdu });
        sent.push((pdu_type::FILE_CHUNK, body));
        id += 1;
    }
    // One forced into encoding 1 although raw would be smaller.
    let short = b"QSL? 73".to_vec();
    let pdu = tx.encode(pdu_type::MESSAGE, 0, short.clone(), Preference::Zstd, &usable).unwrap();
    sim.host(0, sim.now, HostCommand::Send { id, pdu });
    sent.push((pdu_type::MESSAGE, short));

    let n = sent.len();
    sim.run_while(t(300.0), |s| s.stations[1].delivered.len() < n);
    let got = &sim.stations[1].delivered;
    assert_eq!(got.len(), n, "every PDU delivered once");
    let mut rx = Codec::new(b_set);
    let mut seen = [0usize; 3];
    // The MESSAGE PDUs (priority 0) come first, then the file (priority 3).
    let mut want = sent.clone();
    want.sort_by_key(|(t, _)| (*t == pdu_type::FILE_CHUNK) as u8);
    for ((_, from, pdu), (ptype, body)) in got.iter().zip(&want) {
        assert_eq!(*from, sim.addr(0));
        assert_eq!(pdu.pdu_type, *ptype);
        assert_eq!(&rx.decode(pdu).unwrap(), body, "decoded and CRC-32 checked");
        seen[pdu.encoding as usize] += 1;
    }
    eprintln!("delivered raw/zstd/zstd+dict: {seen:?}");
    assert!(seen[1] > 0 && seen[2] > 0, "both compressed encodings crossed: {seen:?}");
    assert_clean(&sim);
}

#[test]
fn a_resume_keeps_the_negotiated_dictionaries() {
    let (eval, _) = dictionaries(&[]);
    let ids = vec![id_to_u32(eval.id())];
    let mut sim = pair_with(23, 25.0, |c| c.dict_ids = ids.clone());
    sim.stations[0].auto_resume = Some(Duration::from_secs(1));
    connect(&mut sim, 0, 1, t(0.0));
    send_chunks(&mut sim, 0, t(0.1), &payload(60_000, 3), 1000);
    sim.outage(0, 1, t(15.0), t(135.0));
    sim.run_while(t(600.0), |s| !s.has_event(0, |e| matches!(e, HostEvent::Connected { resumed: true, .. })));
    sim.run_until(sim.now + Duration::from_secs(2));
    assert!(sim.has_event(0, |e| matches!(e, HostEvent::Connected { resumed: true, .. })));
    assert!(sim.has_event(1, |e| matches!(e, HostEvent::LinkLost { .. })), "both ends lost the link");
    for st in 0..2 {
        assert_eq!(sim.stations[st].st.session_info().unwrap().dictionaries, ids, "station {st}");
    }
    assert_clean(&sim);
}

#[test]
fn no_common_dictionary_means_encodings_0_and_1_only() {
    let (eval, others) = dictionaries(&["alpha "]);
    let mut sim = Sim::new(7);
    let mut a = cfg("N0AAA", 71);
    a.dict_ids = vec![id_to_u32(eval.id())];
    let mut b = cfg("N0BBB", 72);
    b.dict_ids = vec![id_to_u32(others[0].id())];
    sim.add(a, Duration::from_millis(150));
    sim.add(b, Duration::from_millis(150));
    sim.set_snr(0, 1, 25.0);
    connect(&mut sim, 0, 1, t(0.0));
    sim.run_while(t(30.0), |s| !s.has_event(0, |e| matches!(e, HostEvent::Connected { .. })));
    let info = sim.stations[0].st.session_info().unwrap();
    assert!(info.dictionaries.is_empty());
    let mut tx = Codec::new(set(&[&eval]));
    let usable: Vec<DictionaryId> = info.dictionaries.iter().map(|&d| id_from_u32(d)).collect();
    for m in messages() {
        let p = tx.encode(pdu_type::MESSAGE, 0, m, Preference::Auto, &usable).unwrap();
        assert_ne!(p.encoding, encoding::ZSTD_DICT);
    }
}

#[test]
fn compressed_broadcast_slices_reassemble_from_their_decoded_bodies() {
    let (eval, _) = dictionaries(&[]);
    let mut codec = Codec::new(set(&[&eval]));
    let msg: Vec<u8> = messages().concat();
    let src = Address::callsign("N0AAA", 0).unwrap();
    // Three slices, each compressed on its own, one PDU per frame.
    let slices: Vec<Pdu> = msg
        .chunks(msg.len().div_ceil(3))
        .enumerate()
        .map(|(i, c)| {
            let pref = if i == 1 { Preference::Zstd } else { Preference::ZstdDict(Some(eval.id())) };
            codec.encode(pdu_type::MESSAGE, 0, c.to_vec(), pref, &[eval.id()]).unwrap()
        })
        .collect();
    let roles = [FragmentRole::First, FragmentRole::Middle, FragmentRole::Last];
    let frames: Vec<_> = slices
        .iter()
        .map(|p| {
            let blocks = cairn_link::pdu::pack_frame(std::slice::from_ref(p)).unwrap();
            unpack_frame(&blocks.into_iter().map(Some).collect::<Vec<_>>())
        })
        .collect();
    let mut with_codec = Reassembler::default();
    let mut raw_only = Reassembler::default();
    let mut decode = |p: &Pdu| codec.decode(p).ok();
    let mut got = Vec::new();
    for (i, (f, role)) in frames.iter().zip(roles).enumerate() {
        got.extend(with_codec.on_frame_with(src, 40 + i as u16, role, f.clone(), t(0.0), &mut decode).unwrap());
        assert!(raw_only.on_frame(src, 40 + i as u16, role, f.clone(), t(0.0)).unwrap().is_empty());
    }
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, msg);
    assert_eq!(got[0].encoding, encoding::RAW, "delivered as the decoded message");
}
