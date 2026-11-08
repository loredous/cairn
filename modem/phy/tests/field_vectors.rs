//! Every `vectors/header/` and `vectors/control/` case, and the block map
//! control blocks of `vectors/link/` (SPEC 8.2, Appendix B, ADRs 0008 and
//! 0011): the fields pack to the published bits, the bits unpack to the
//! fields, and the cases a receiver must refuse or skip are refused or
//! skipped.

mod common;

use cairn_phy::fields::{self, ControlBlock, FieldError, Header};
use common::*;
use serde_json::Value;

fn cases(kind: &str) -> Vec<Value> {
    vector_docs(kind).into_iter().map(|(_, d)| d).collect()
}

#[test]
fn every_header_vector() {
    let docs = cases("header");
    assert!(docs.len() >= 12);
    for d in &docs {
        let name = d["name"].as_str().unwrap();
        let bits = hex_to_bits(d["info_hex"].as_str().unwrap(), 144);
        if !d["valid"].as_bool().unwrap() {
            let err = Header::unpack(&bits).expect_err(name);
            assert_eq!(err == FieldError::Crc, d["error"] == "crc", "{name}: {err}");
            continue;
        }
        let h = header(&d["fields"]);
        assert_eq!(bits_to_hex(&h.message_bits().unwrap()), d["message_hex"].as_str().unwrap(), "{name}");
        assert_eq!(bits_to_hex(&h.bits().unwrap()), d["info_hex"].as_str().unwrap(), "{name}");
        assert_eq!(Header::unpack(&bits).unwrap(), h, "{name}");
    }
}

#[test]
fn every_control_vector() {
    let mut docs = cases("control");
    assert!(docs.len() >= 14);
    docs.extend(cases("link").into_iter().filter(|d| d["format"] == "cairn-control-vectors"));
    let mut types = std::collections::BTreeSet::new();
    for d in &docs {
        let name = d["name"].as_str().unwrap();
        let bits = hex_to_bits(d["info_hex"].as_str().unwrap(), 256);
        if !d["valid"].as_bool().unwrap() {
            match d["error"].as_str().unwrap() {
                "unknown" => {
                    let c = ControlBlock::unpack(&bits).unwrap();
                    let t = u8::from_str_radix(&d["message_hex"].as_str().unwrap()[..1], 16).unwrap();
                    assert!(t == 2 || t >= 8, "{name}: type {t} is not reserved (ADR 0008)");
                    assert!(matches!(c, ControlBlock::Unknown { control_type, .. } if control_type == t), "{name}");
                    // Skipped, but carried through intact.
                    assert_eq!(c.bits().unwrap(), bits);
                }
                "crc" => assert_eq!(ControlBlock::unpack(&bits), Err(FieldError::Crc), "{name}"),
                _ => assert!(matches!(ControlBlock::unpack(&bits), Err(FieldError::Range(..))), "{name}"),
            }
            continue;
        }
        let c = control(&d["fields"]);
        types.insert(c.control_type());
        assert_eq!(bits_to_hex(&c.message_bits().unwrap()), d["message_hex"].as_str().unwrap(), "{name}");
        assert_eq!(bits_to_hex(&c.bits().unwrap()), d["info_hex"].as_str().unwrap(), "{name}");
        assert_eq!(ControlBlock::unpack(&bits).unwrap(), c, "{name}");
    }
    assert_eq!(types.into_iter().collect::<Vec<_>>(), vec![0, 1, 3, 4, 5, 6, 7]);
}

#[test]
fn sha256_known_answers() {
    let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex(fields::sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(hex(fields::sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    let long = vec![b'a'; 1000];
    assert_eq!(hex(fields::sha256(&long)), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
}
