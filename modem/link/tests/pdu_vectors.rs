//! `cairn_link::pdu` against the published PDU vectors (`vectors/link/`)
//! and against `cairn_phy::pdu`, byte for byte.
#![cfg_attr(not(feature = "phy"), allow(unused_imports))]

use cairn_link::pdu::{self, FragmentRole, Pdu, Reassembler};
use cairn_link::{Address, Block, Flags, FrameType, Instant, LinkFrame, Seq, TxParams};
use proptest::prelude::*;
use serde_json::Value;

fn vector(name: &str) -> Value {
    let path = format!("{}/../../vectors/link/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn pdu_of(v: &Value, decoded: Option<&[u8]>) -> Pdu {
    let encoding = v["encoding"].as_u64().unwrap() as u8;
    let dictionary_id = v["dictionary_id_hex"].as_str().map(|d| hex(d).try_into().unwrap());
    let body = hex(v["body_hex"].as_str().unwrap());
    Pdu {
        pdu_type: v["type"].as_u64().unwrap() as u8,
        stream: v["stream"].as_u64().unwrap() as u8,
        encoding,
        dictionary_id,
        crc: (encoding != 0).then(|| pdu::crc32(decoded.unwrap())),
        body,
    }
}

#[test]
fn pdu_blocks_vector() {
    let v = vector("pdu_blocks");
    assert_eq!(
        pdu::crc32(b"123456789"),
        u32::from_str_radix(v["crc32_check"]["crc32_hex"].as_str().unwrap(), 16).unwrap()
    );
    let decoded = hex(v["decoded_hex"].as_str().unwrap());
    let pdus = v["pdus"].as_array().unwrap();
    let mut blocks: Vec<Block> = Vec::new();
    for g in 0..2 {
        let group: Vec<Pdu> = pdus.iter().filter(|p| p["group"] == g).map(|p| pdu_of(p, Some(&decoded))).collect();
        for (p, pv) in group.iter().zip(pdus.iter().filter(|p| p["group"] == g)) {
            assert_eq!(p.encode().unwrap(), hex(pv["pdu_hex"].as_str().unwrap()));
            assert_eq!(p.wire_len() - 2, pv["length"].as_u64().unwrap() as usize);
        }
        blocks.extend(pdu::pack_frame(&group).unwrap());
    }
    let want: Vec<Vec<u8>> = v["blocks_hex"].as_array().unwrap().iter().map(|b| hex(b.as_str().unwrap())).collect();
    assert_eq!(blocks.iter().map(|b| b.to_vec()).collect::<Vec<_>>(), want);
    // Parsing gives the PDUs back, with their CRC.
    let mut parser = pdu::StreamParser::default();
    let mut out = Vec::new();
    for b in &blocks {
        out.extend(parser.push(Some(b)));
    }
    assert_eq!(out.len(), pdus.len());
    for (got, pv) in out.iter().zip(pdus) {
        let got = got.as_ref().unwrap();
        assert_eq!(got.body, hex(pv["body_hex"].as_str().unwrap()));
        assert_eq!(got.crc, Some(u32::from_str_radix(pv["crc32_hex"].as_str().unwrap(), 16).unwrap()));
    }
}

#[test]
fn pdu_broadcast_fragments_vector() {
    let v = vector("pdu_broadcast_fragments");
    let message = hex(v["message_hex"].as_str().unwrap());
    let r = &v["reassembled"];
    let msg = Pdu { body: message.clone(), crc: None, ..pdu_of(r, None) };
    let slices = pdu::fragment(&msg, v["slice_bytes"].as_u64().unwrap() as usize).unwrap();
    let frames = v["frames"].as_array().unwrap();
    assert_eq!(slices.len(), frames.len());
    let src = Address::callsign("N0CALL", 1).unwrap();
    let mut reasm = Reassembler::default();
    let mut delivered = Vec::new();
    for (i, (s, fv)) in slices.iter().zip(frames).enumerate() {
        let role = match (i == 0, i + 1 == slices.len()) {
            (true, _) => FragmentRole::First,
            (false, false) => FragmentRole::Middle,
            (false, true) => FragmentRole::Last,
        };
        let (first, more) = role.flags();
        assert_eq!((first, more), (fv["first_fragment"].as_bool().unwrap(), fv["more_fragments"].as_bool().unwrap()));
        assert_eq!(s.encode().unwrap(), hex(fv["pdu"]["pdu_hex"].as_str().unwrap()));
        let blocks = pdu::pack_frame(std::slice::from_ref(s)).unwrap();
        let want: Vec<Vec<u8>> =
            fv["blocks_hex"].as_array().unwrap().iter().map(|b| hex(b.as_str().unwrap())).collect();
        assert_eq!(blocks.iter().map(|b| b.to_vec()).collect::<Vec<_>>(), want);
        let seq = fv["sequence"].as_u64().unwrap() as u16;
        // The header, through the adapter, matches the vector bit for bit.
        #[cfg(feature = "phy")]
        {
            let f = LinkFrame {
                ftype: FrameType::Ui,
                src,
                dst: Address::BROADCAST,
                flags: Flags { first_fragment: first, more_fragments: more, ..Default::default() },
                seq: Seq(seq),
                mcs: 1,
                mask_id: 0,
                controls: vec![],
                data: blocks.clone(),
                tx: TxParams::default(),
            };
            f.validate().unwrap();
            let bits = cairn_link::adapter::header_to_phy(&f).unwrap().bits().unwrap();
            let bytes: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| a << 1 | b)).collect();
            assert_eq!(bytes, hex(fv["header_info_hex"].as_str().unwrap()));
        }
        let parsed = pdu::unpack_frame(&blocks.iter().copied().map(Some).collect::<Vec<_>>());
        delivered.extend(reasm.on_frame(src, seq, role, parsed, Instant::ZERO).unwrap());
    }
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].body, message);
    assert_eq!(delivered[0].encode().unwrap(), hex(r["pdu_hex"].as_str().unwrap()));
}

#[cfg(feature = "phy")]
proptest! {
    /// Byte-identical with `cairn_phy::pdu::pack_blocks` and its parser.
    #[test]
    fn identical_to_cairn_phy_pdu(
        pdus in proptest::collection::vec((any::<u8>(), any::<u8>(), proptest::collection::vec(any::<u8>(), 0..400)), 1..8)
    ) {
        let ours: Vec<Pdu> = pdus.iter().map(|(t, s, b)| Pdu { pdu_type: *t, stream: *s, ..Pdu::message(b.clone()) }).collect();
        let theirs: Vec<cairn_phy::pdu::Pdu> =
            pdus.iter().map(|(t, s, b)| cairn_phy::pdu::Pdu::raw(*t, b, *s)).collect();
        let a = pdu::pack_frame(&ours).unwrap();
        let b = cairn_phy::pdu::pack_blocks(&theirs).unwrap();
        prop_assert_eq!(a.iter().map(|x| x.to_vec()).collect::<Vec<_>>(), b.iter().map(|x| x.to_vec()).collect::<Vec<_>>());
        let back = cairn_phy::pdu::unpack_blocks(&a);
        prop_assert_eq!(back.len(), ours.len());
        for (x, y) in back.iter().zip(&ours) {
            prop_assert_eq!(&x.as_ref().unwrap().body, &y.body);
        }
    }
}
