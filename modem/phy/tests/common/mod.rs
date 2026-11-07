#![allow(dead_code)]

use std::path::PathBuf;

use cairn_phy::fields::{
    AckBlock, Address, CalReportBlock, CalRequestBlock, ConnectBlock, ControlBlock, Header, IdBlock, PathBlock,
};
use serde_json::Value;

/// Repository root (two levels above this crate).
pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The modem workspace directory.
pub fn modem_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

pub fn read_json(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Big-endian hex to `n` bits.
pub fn hex_to_bits(hex: &str, n: usize) -> Vec<u8> {
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
    let bits: Vec<u8> = bytes.iter().flat_map(|&b| (0..8).map(move |i| (b >> (7 - i)) & 1)).collect();
    assert!(bits.len() >= n, "hex too short");
    bits[..n].to_vec()
}

/// Bits to big-endian hex, zero-padded at the end.
pub fn bits_to_hex(bits: &[u8]) -> String {
    bits.chunks(8)
        .map(|c| {
            let mut v = 0u8;
            for (i, &b) in c.iter().enumerate() {
                v |= b << (7 - i);
            }
            format!("{v:02x}")
        })
        .collect()
}

pub fn usizes(v: &serde_json::Value) -> Vec<usize> {
    v.as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as usize).collect()
}

pub fn f64s(v: &serde_json::Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect()
}

/// Little-endian f32 samples.
pub fn read_f32(path: &std::path::Path) -> Vec<f64> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64).collect()
}

/// A fixture's frame configuration.
pub fn fixture_config(doc: &serde_json::Value) -> cairn_phy::FrameConfig {
    let usable = match &doc["usable_tones"] {
        serde_json::Value::Null => None,
        v => Some(usizes(v)),
    };
    cairn_phy::FrameConfig::new(
        doc["width"].as_u64().unwrap() as usize,
        doc["mcs"].as_u64().unwrap() as u8,
        doc["n_blocks"].as_u64().unwrap() as usize,
    )
    .unwrap()
    .with_cp(doc["cp"].as_u64().unwrap() as usize)
    .unwrap()
    .with_mask(usable)
    .unwrap()
    .with_controls(doc.get("n_control").map_or(0, |v| v.as_u64().unwrap() as usize))
    .unwrap()
    .with_probe(doc.get("probe").map_or(doc["width"].as_u64().unwrap() > 24, |v| v.as_bool().unwrap()))
    .unwrap()
}

/// A header-first fixture's control messages (240 bits each).
pub fn fixture_controls(doc: &serde_json::Value) -> Vec<Vec<u8>> {
    doc.get("control_message_hex")
        .map_or(Vec::new(), |v| v.as_array().unwrap().iter().map(|x| hex_to_bits(x.as_str().unwrap(), 240)).collect())
}

/// A fixture's header message (128 bits) and payloads (1008 bits each).
pub fn fixture_inputs(doc: &serde_json::Value) -> (Vec<u8>, Vec<Vec<u8>>) {
    let h = hex_to_bits(doc["header_message_hex"].as_str().unwrap(), 128);
    let p = doc["payload_hex"].as_array().unwrap().iter().map(|v| hex_to_bits(v.as_str().unwrap(), 1008)).collect();
    (h, p)
}

/// Little-endian f64 samples.
pub fn read_f64(path: &std::path::Path) -> Vec<f64> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    b.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
}

// --- published header and control block fields (vectors/header/, control/, frame/) ---

pub fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not a number: {v}"))
}

pub fn b(v: &Value) -> bool {
    v.as_bool().unwrap()
}

/// An address as published; a hashed callsign is checked against the hash.
pub fn address(v: &Value) -> Address {
    let a = if v.get("broadcast").is_some() {
        Address::Broadcast
    } else {
        let a = Address::new(v["callsign"].as_str().unwrap(), u(&v["ssid"]) as u8).unwrap();
        if let Some(h) = v.get("hash30") {
            assert_eq!(a, Address::Hashed { hash30: u(h) as u32, ssid: u(&v["ssid"]) as u8 });
        }
        a
    };
    assert_eq!(format!("{:010x}", a.encode()), v["value_hex"].as_str().unwrap());
    a
}

pub fn tone_mask(v: &Value) -> Option<u64> {
    v.as_array().map(|a| a.iter().fold(0u64, |m, k| m | 1 << u(k)))
}

pub fn header(f: &Value) -> Header {
    let h = Header {
        version: u(&f["version"]) as u8,
        frame_type: u(&f["frame_type"]) as u8,
        source: address(&f["source"]),
        destination: address(&f["destination"]),
        sequence: u(&f["sequence"]) as u16,
        mcs: u(&f["mcs"]) as u8,
        n_control: u(&f["control_blocks"]) as usize,
        n_data: u(&f["data_blocks"]) as usize,
        more_fragments: b(&f["more_fragments"]),
        session: b(&f["session"]),
        probe: b(&f["probe"]),
        repeat: b(&f["repeat"]),
    };
    assert_eq!(h.flags() as u64, u(&f["flags"]));
    h
}

fn arr8(v: &Value) -> [u8; 8] {
    let a: Vec<u8> = v.as_array().unwrap().iter().map(|x| u(x) as u8).collect();
    a.try_into().unwrap()
}

pub fn control(f: &Value) -> ControlBlock {
    match u(&f["type"]) {
        0 => ControlBlock::Ack(AckBlock {
            base_sequence: u(&f["base_sequence"]) as u16,
            bitmap: u128::from_str_radix(f["bitmap_hex"].as_str().unwrap(), 16).unwrap(),
            requested_mcs: u(&f["requested_mcs"]) as u8,
            snr_q: u(&f["snr_q"]) as u8,
            block_errors: u(&f["block_errors"]) as u8,
            tone_mask: tone_mask(&f["tone_mask"]),
            reprobe: b(&f["reprobe"]),
            long_prefix: b(&f["long_prefix"]),
            distortion: u(&f["distortion"]) as u8,
            adc_clipped: b(&f["adc_clipped"]),
        }),
        1 => ControlBlock::Connect(ConnectBlock {
            session_id: u(&f["session_id"]) as u16,
            capabilities: u(&f["capabilities"]) as u16,
            width_cap: u(&f["width_cap"]) as u8,
            requested_mcs: u(&f["requested_mcs"]) as u8,
            max_frame_time: u(&f["max_frame_time"]) as u8,
            tone_mask: tone_mask(&f["tone_mask"]).unwrap(),
            dictionary_ids: f["dictionary_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| u(x) as u32)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
            more_dictionaries: b(&f["more_dictionaries"]),
            resume: b(&f["resume"]),
        }),
        3 => ControlBlock::CalRequest(CalRequestBlock {
            tone_freq: u(&f["tone_freq"]) as u8,
            step_count: u(&f["step_count"]) as u8,
            step_length: u(&f["step_length"]) as u8,
            gap_length: u(&f["gap_length"]) as u8,
            start_level: u(&f["start_level"]) as u8,
            step_size: u(&f["step_size"]) as u8,
        }),
        4 => ControlBlock::CalReport(CalReportBlock {
            amplitudes: arr8(&f["amplitudes"]),
            distortion: arr8(&f["distortion"]),
            clipped: f["clipped"].as_array().unwrap().iter().map(b).collect::<Vec<_>>().try_into().unwrap(),
            sweep_valid: b(&f["sweep_valid"]),
        }),
        5 => ControlBlock::Id(IdBlock {
            callsign: f["callsign"].as_str().unwrap().to_string(),
            ssid: u(&f["ssid"]) as u8,
            grid: f["grid"].as_str().map(str::to_string),
        }),
        6 => ControlBlock::Path(PathBlock {
            entries: f["entries"].as_array().unwrap().iter().map(address).collect(),
            hops: u(&f["hops"]) as u8,
            used: [b(&f["used"][0]), b(&f["used"][1])],
            via: b(&f["via"]),
        }),
        t => panic!("type {t}"),
    }
}
