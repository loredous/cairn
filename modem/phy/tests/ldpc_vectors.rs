//! The LDPC codes against the published files: `vectors/ldpc_*.json`
//! (CRC, full codeword, every rate-matched length), `codes/*_ratematch.txt`,
//! `codes/*_lifted.txt` re-derived from the base graphs `codes/bg*.csv`, and
//! `codes/*.alist`.

mod common;

use cairn_phy::{crc, ldpc, params};
use common::*;

fn codes() -> [&'static ldpc::LiftedCode; 3] {
    [ldpc::header_code(), ldpc::control_code(), ldpc::data_code()]
}

#[test]
fn known_answer_vectors() {
    let mut checked = 0;
    for code in codes() {
        let doc = read_json(&repo().join(format!("vectors/ldpc_{}.json", code.spec.name)));
        assert_eq!(doc["format"], "cairn-ldpc-vectors");
        assert_eq!(doc["lifting"].as_u64().unwrap() as usize, code.z);
        assert_eq!(doc["base_graph"].as_u64().unwrap() as u8, code.spec.base_graph);
        assert_eq!(doc["info_bits"].as_u64().unwrap() as usize, code.k);
        assert_eq!(doc["codeword_bits"].as_u64().unwrap() as usize, code.n_codeword);
        assert_eq!(usizes(&doc["coded_lengths"]), code.spec.coded_bits);
        let m = doc["message_bits"].as_u64().unwrap() as usize;
        for v in doc["vectors"].as_array().unwrap() {
            let name = v["name"].as_str().unwrap();
            let msg = hex_to_bits(v["message_hex"].as_str().unwrap(), m);
            let info = crc::append_crc(&msg);
            assert_eq!(bits_to_hex(&info), v["info_hex"].as_str().unwrap(), "{} {name} info", code.spec.name);
            let cw = code.encode(&info);
            assert_eq!(bits_to_hex(&cw), v["codeword_hex"].as_str().unwrap(), "{} {name} codeword", code.spec.name);
            for (e, hex) in v["rate_matched_hex"].as_object().unwrap() {
                let e: usize = e.parse().unwrap();
                assert_eq!(
                    bits_to_hex(&code.rate_match(&cw, e)),
                    hex.as_str().unwrap(),
                    "{} {name} E={e}",
                    code.spec.name
                );
                checked += 1;
            }
            // And the decoder recovers it from clean LLRs at every length.
            for &e in code.spec.coded_bits {
                let tx = code.rate_match(&cw, e);
                let llr: Vec<f64> = tx.iter().map(|&b| if b == 0 { 3.0 } else { -3.0 }).collect();
                let res = code.decoder().decode(&code.rate_recover(&llr, 0), &code.rows_for_length(e), 30, 0.75);
                assert!(res.ok && res.bits == info, "{} {name} E={e} decode", code.spec.name);
            }
        }
    }
    assert_eq!(checked, 6 * (2 + 1 + 4));
}

#[test]
fn rate_matching_tables() {
    for code in codes() {
        let text =
            std::fs::read_to_string(repo().join(format!("codes/{}_ratematch.txt", code.spec.file_stem()))).unwrap();
        let mut sections: Vec<(usize, Vec<usize>)> = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            if let Some(e) = line.strip_prefix("length ") {
                sections.push((e.trim().parse().unwrap(), Vec::new()));
            } else {
                sections.last_mut().unwrap().1.extend(line.split_whitespace().map(|t| t.parse::<usize>().unwrap()));
            }
        }
        assert_eq!(sections.iter().map(|s| s.0).collect::<Vec<_>>(), code.spec.coded_bits);
        for (e, pos) in sections {
            assert_eq!(pos.len(), e);
            assert_eq!(code.tx_positions(e, 0), pos, "{} E={e}", code.spec.name);
        }
    }
}

#[test]
fn lifted_files_match_base_graphs() {
    for code in codes() {
        let text = std::fs::read_to_string(repo().join(format!("codes/bg{}.csv", code.spec.base_graph))).unwrap();
        let ils = code.spec.set_index();
        let mut want: Vec<Vec<(usize, usize)>> = vec![Vec::new(); code.base_rows];
        for line in text.lines().skip(1) {
            let v: Vec<usize> = line.split(',').map(|t| t.trim().parse().unwrap()).collect();
            want[v[0]].push((v[1], v[2 + ils] % code.z));
        }
        assert_eq!(code.layers, want, "{}", code.spec.name);
    }
    assert_eq!(params::HEADER_CODE.set_index(), 7);
    assert_eq!(params::CONTROL_CODE.set_index(), 6);
    assert_eq!(params::DATA_CODE.set_index(), 1);
}

#[test]
fn alist_matches_lifted_code() {
    for code in codes() {
        let text = std::fs::read_to_string(repo().join(format!("codes/{}.alist", code.spec.file_stem()))).unwrap();
        let mut lines = text.lines();
        let nm: Vec<usize> = lines.next().unwrap().split_whitespace().map(|t| t.parse().unwrap()).collect();
        let (n, m) = (nm[0], nm[1]);
        assert_eq!((n, m), (code.n_codeword, code.base_rows * code.z));
        // Skip max degrees and the two degree lists, then n column lines.
        for _ in 0..3 {
            lines.next();
        }
        let z = code.z;
        let mut cols: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, layer) in code.layers.iter().enumerate() {
            for &(j, s) in layer {
                for r in 0..z {
                    cols[j * z + (r + s) % z].push(i * z + r + 1);
                }
            }
        }
        for (j, col) in cols.iter_mut().enumerate() {
            col.sort_unstable();
            let got: Vec<usize> = lines
                .next()
                .unwrap()
                .split_whitespace()
                .map(|t| t.parse().unwrap())
                .filter(|&v: &usize| v != 0)
                .collect();
            assert_eq!(&got, col, "{} column {j}", code.spec.name);
        }
    }
}
