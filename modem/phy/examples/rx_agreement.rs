//! Block-by-block agreement of the Rust receiver with the simulator's on a
//! directory of fixtures (`gen_rx_fixtures.py --sweep N --out DIR`): same
//! audio, same start, same configuration.
//!
//!     cargo run --release --example rx_agreement -- DIR

use std::collections::BTreeMap;
use std::path::Path;

use cairn_phy::{FrameConfig, Receiver, RxOptions, wav};

fn bits(hex: &str, n: usize) -> Vec<u8> {
    (0..n).map(|i| (u8::from_str_radix(&hex[i / 4..i / 4 + 1], 16).unwrap() >> (3 - i % 4)) & 1).collect()
}

fn main() {
    let dir = std::env::args().nth(1).expect("fixture directory");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    let mut rx = Receiver::new();
    // Per SNR: blocks, both ok, rust only, python only, both fail, header disagreements, iteration mismatches.
    let mut stats: BTreeMap<String, [usize; 7]> = BTreeMap::new();
    for p in paths {
        let text = std::fs::read_to_string(&p).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        let usable = doc["usable_tones"].as_array().map(|a| a.iter().map(|v| v.as_u64().unwrap() as usize).collect());
        let cfg = FrameConfig::new(
            doc["width"].as_u64().unwrap() as usize,
            doc["mcs"].as_u64().unwrap() as u8,
            doc["n_blocks"].as_u64().unwrap() as usize,
        )
        .unwrap()
        .with_cp(doc["cp"].as_u64().unwrap() as usize)
        .unwrap()
        .with_mask(usable)
        .unwrap();
        let py = &doc["python"]["rx"];
        if py.is_null() {
            continue;
        }
        let samples = wav::read(Path::new(&p.with_extension("wav"))).unwrap();
        let r = rx.receive(&samples, &cfg, py["start"].as_u64().unwrap() as usize, &RxOptions::default());
        let key = format!("{} dB", doc["snr_db"]);
        let s = stats.entry(key).or_default();
        let py_hdr = py["header_ok"].as_bool().unwrap();
        s[5] += (py_hdr != r.header_ok) as usize;
        let py_iter: Vec<u64> = py["iterations"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
        for (i, pc) in py["block_correct"].as_array().unwrap().iter().enumerate() {
            let want = bits(doc["payload_hex"][i].as_str().unwrap(), 1008);
            let rc = r.header_ok && r.block_ok[i] && r.block_bits[i][..1008] == want[..];
            let pc = py_hdr && pc.as_bool().unwrap();
            s[0] += 1;
            s[match (rc, pc) {
                (true, true) => 1,
                (true, false) => 2,
                (false, true) => 3,
                (false, false) => 4,
            }] += 1;
            s[6] += (py_iter[i] as usize != r.iterations[i]) as usize;
        }
    }
    println!(
        "| SNR | Blocks | Both decode | Rust only | Python only | Neither | Header disagreements | Iteration-count mismatches |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (k, s) in stats {
        println!("| {k} | {} | {} | {} | {} | {} | {} | {} |", s[0], s[1], s[2], s[3], s[4], s[5], s[6]);
    }
}
