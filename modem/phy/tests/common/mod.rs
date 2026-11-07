#![allow(dead_code)]

use std::path::PathBuf;

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
