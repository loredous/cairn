//! `cairn_link::compress` against the published zstd body vectors
//! (`vectors/zstd/bodies.json`, written by `sim/cairn_sim/zstd_vectors.py`
//! through the dictionary evaluation's codec): the same bytes out for the
//! same input, the frame header ADR 0014 asks for, every frame a receiver
//! must accept decoded, every one it must refuse refused.
#![cfg(feature = "zstd")]

use std::path::PathBuf;

use cairn_link::compress::{
    Codec, CodecError, DECODED_BODY_MAX, Dictionaries, Dictionary, Preference, ZSTD_MAGIC, frame_header, id_hex,
};
use cairn_link::pdu::{Pdu, StreamParser, crc32, encoding, pack_frame, pdu_type};
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn doc() -> Value {
    let path = root().join("vectors/zstd/bodies.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_else(|| panic!("{k}"))
}

fn dictionary(d: &Value) -> Dictionary {
    let dict = Dictionary::load(&root().join(s(&d["dictionary"], "file"))).unwrap();
    let sha: String = dict.sha256().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(sha, s(&d["dictionary"], "sha256"), "the vector's dictionary file changed");
    assert_eq!(id_hex(dict.id()), s(&d["dictionary"], "dictionary_id_hex"));
    dict
}

fn codec(d: &Value) -> (Codec, Dictionary) {
    let dict = dictionary(d);
    let mut set = Dictionaries::new();
    set.insert(dict.clone());
    (Codec::new(set), dict)
}

#[test]
fn sent_bodies_are_byte_identical_to_the_reference() {
    let d = doc();
    assert_eq!(d["decoded_body_max"].as_u64().unwrap() as usize, DECODED_BODY_MAX);
    let (mut c, dict) = codec(&d);
    for v in d["sent"].as_array().unwrap() {
        let name = s(v, "name");
        let input = hex(s(v, "input_hex"));
        let want = hex(s(v, "body_hex"));
        let dict_id = (v["encoding"] == 2).then(|| dict.id());
        let got = c.compress(&input, dict_id).unwrap();
        assert_eq!(got, want, "{name}: compressed bytes");
        // The frame header of ADR 0014: magic, then FHD 0x00 (no content
        // size, not single-segment, no checksum, no dictionary ID).
        assert_eq!(got[..4], ZSTD_MAGIC, "{name}");
        assert_eq!(got[4], 0x00, "{name}: frame header descriptor");
        let h = frame_header(&got).unwrap();
        assert!(!h.checksum && h.dictionary_id.is_none() && h.content_size.is_none() && !h.single_segment, "{name}");
        assert_eq!(c.decompress(&want, dict_id, DECODED_BODY_MAX).unwrap(), input, "{name}: decoded");
        // As a PDU: the same bytes, and decoded with its CRC-32 checked.
        let pdu = Pdu {
            pdu_type: pdu_type::MESSAGE,
            stream: 0,
            encoding: v["encoding"].as_u64().unwrap() as u8,
            dictionary_id: dict_id,
            body: want.clone(),
            crc: Some(crc32(&input)),
        };
        assert_eq!(pdu.encode().unwrap(), hex(s(v, "pdu_hex")), "{name}: PDU");
        assert_eq!(c.decode(&pdu).unwrap(), input, "{name}");
    }
}

#[test]
fn auto_takes_the_smallest_pdu() {
    let d = doc();
    let (mut c, dict) = codec(&d);
    for v in d["sent"].as_array().unwrap() {
        let input = hex(s(v, "input_hex"));
        let p = c.encode(pdu_type::MESSAGE, 0, input.clone(), Preference::Auto, &[dict.id()]).unwrap();
        let raw = Pdu::message(input.clone()).wire_len();
        let z = c.encode(pdu_type::MESSAGE, 0, input.clone(), Preference::Zstd, &[]).unwrap().wire_len();
        let zd = c.encode(pdu_type::MESSAGE, 0, input.clone(), Preference::ZstdDict(None), &[dict.id()]).unwrap();
        assert_eq!(p.wire_len(), raw.min(z).min(zd.wire_len()), "{}", s(v, "name"));
        assert_eq!(c.decode(&p).unwrap(), input);
        // No dictionary agreed: raw or zstd only.
        let p = c.encode(pdu_type::MESSAGE, 0, input.clone(), Preference::Auto, &[]).unwrap();
        assert_ne!(p.encoding, encoding::ZSTD_DICT);
    }
}

#[test]
fn frames_with_checksum_dictionary_id_or_content_size_are_accepted() {
    let d = doc();
    let (mut c, dict) = codec(&d);
    for v in d["accept"].as_array().unwrap() {
        let name = s(v, "name");
        let body = hex(s(v, "body_hex"));
        let h = frame_header(&body).unwrap();
        assert_eq!(format!("{:02x}", h.descriptor), s(&v["header"], "descriptor_hex"), "{name}");
        assert_eq!(h.checksum, v["header"]["checksum"].as_bool().unwrap(), "{name}");
        assert_eq!(h.dictionary_id.is_some(), v["header"]["dictionary_id_flag"] != 0, "{name}");
        let dict_id = v["dictionary"].as_bool().unwrap().then(|| dict.id());
        assert_eq!(c.decompress(&body, dict_id, DECODED_BODY_MAX).unwrap(), hex(s(v, "input_hex")), "{name}");
    }
}

#[test]
fn bodies_without_magic_with_trailing_bytes_or_over_the_bound_are_refused() {
    let d = doc();
    let (mut c, _) = codec(&d);
    for v in d["reject"].as_array().unwrap() {
        let name = s(v, "name");
        let body = hex(s(v, "body_hex"));
        let got = c.decompress(&body, None, DECODED_BODY_MAX);
        match s(v, "error") {
            "missing_magic" => assert_eq!(got, Err(CodecError::MissingMagic), "{name}"),
            "trailing_bytes" => assert!(matches!(got, Err(CodecError::TrailingBytes(_))), "{name}: {got:?}"),
            "too_large" => {
                assert_eq!(got, Err(CodecError::TooLarge { bound: DECODED_BODY_MAX }), "{name}");
                // The same body under a bound it fits.
                let n = v["decoded_bytes"].as_u64().unwrap() as usize;
                assert_eq!(c.decompress(&body, None, n).unwrap().len(), n, "{name}");
                assert_eq!(c.decompress(&body, None, n - 1), Err(CodecError::TooLarge { bound: n - 1 }), "{name}");
            }
            e => panic!("{name}: {e}"),
        }
    }
}

#[test]
fn wrong_or_unknown_dictionary_fails_loudly() {
    let d = doc();
    let (mut c, dict) = codec(&d);
    let v = d["sent"].as_array().unwrap().iter().find(|v| s(v, "name") == "ics213_dict").unwrap();
    let body = hex(s(v, "body_hex"));
    let input = hex(s(v, "input_hex"));
    // Decoded without the dictionary: libzstd refuses or the CRC-32 does.
    let pdu = Pdu {
        pdu_type: pdu_type::MESSAGE,
        stream: 0,
        encoding: encoding::ZSTD,
        dictionary_id: None,
        body: body.clone(),
        crc: Some(crc32(&input)),
    };
    let e = c.decode(&pdu).unwrap_err();
    assert!(e.must_discard(), "{e:?}");
    // With another dictionary of the same ID slot: the wrong dictionary.
    let other = Dictionary::new("other", b"a different dictionary, raw content only".repeat(20));
    let mut set = Dictionaries::new();
    set.insert(other.clone());
    let mut wrong = Codec::new(set);
    let pdu = Pdu { encoding: encoding::ZSTD_DICT, dictionary_id: Some(other.id()), ..pdu };
    let e = wrong.decode(&pdu).unwrap_err();
    assert!(matches!(e, CodecError::CrcMismatch | CodecError::Malformed(_)), "{e:?}");
    // A dictionary this station does not have.
    let pdu = Pdu { dictionary_id: Some(dict.id()), ..pdu };
    assert_eq!(wrong.decode(&pdu), Err(CodecError::UnknownDictionary(dict.id())));
    assert!(!CodecError::UnknownDictionary(dict.id()).must_discard());
    // Sending with a dictionary the receiver is not known to have.
    assert_eq!(
        c.encode(pdu_type::MESSAGE, 0, input, Preference::ZstdDict(Some(dict.id())), &[]),
        Err(CodecError::UnknownDictionary(dict.id()))
    );
}

#[test]
fn compressed_pdus_round_trip_through_the_block_stream() {
    let d = doc();
    let (mut c, dict) = codec(&d);
    let mut pdus = Vec::new();
    let mut inputs = Vec::new();
    for v in d["sent"].as_array().unwrap() {
        let input = hex(s(v, "input_hex"));
        let pref = if v["encoding"] == 2 { Preference::ZstdDict(Some(dict.id())) } else { Preference::Zstd };
        pdus.push(c.encode(pdu_type::MESSAGE, 0, input.clone(), pref, &[dict.id()]).unwrap());
        inputs.push(input);
    }
    let blocks = pack_frame(&pdus).unwrap();
    let mut parser = StreamParser::default();
    let got: Vec<Pdu> = blocks.iter().flat_map(|b| parser.push(Some(b))).map(Result::unwrap).collect();
    assert_eq!(got, pdus);
    for (p, input) in got.iter().zip(&inputs) {
        assert_eq!(&c.decode(p).unwrap(), input);
    }
}
