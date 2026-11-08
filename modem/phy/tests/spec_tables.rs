//! `params.rs` against the tables of `docs/SPEC.md` (SPEC 21.3: constants
//! come from the specification tables). Parses 6.1 (S1), 6.2, 6.3, 6.4
//! (interleaver multipliers), 7.1 and Appendix A.1-A.3.

mod common;

use cairn_phy::{interleave, params};

fn spec() -> String {
    std::fs::read_to_string(common::repo().join("docs/SPEC.md")).unwrap()
}

/// Lines of the section that starts with `heading`, up to the next heading.
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut lines = text.lines().skip_while(|l| !l.starts_with(heading));
    let first = lines.next().unwrap_or_else(|| panic!("no section {heading}"));
    std::iter::once(first).chain(lines.take_while(|l| !l.starts_with('#'))).collect()
}

/// Table rows (cells trimmed) of a section, header and rule excluded.
fn rows(lines: &[&str]) -> Vec<Vec<String>> {
    lines
        .iter()
        .filter(|l| l.starts_with('|'))
        .skip(2)
        .map(|l| l.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect())
        .collect()
}

fn num(s: &str) -> f64 {
    s.trim_end_matches(" dB").replace(',', "").parse().unwrap_or_else(|_| panic!("number {s:?}"))
}

#[test]
fn mcs_ladder_6_2() {
    let text = spec();
    let t = rows(&section(&text, "### 6.2"));
    assert_eq!(t.len(), params::RUNGS.len());
    for (row, rung) in t.iter().zip(&params::RUNGS) {
        assert_eq!(row[0].split_whitespace().next().unwrap(), rung.mcs.to_string());
        assert_eq!(row[1], rung.modulation());
        assert_eq!(row[2], format!("{}/{}", rung.code_rate.0, rung.code_rate.1));
        // Payload bits per data tone-symbol = Q x rate.
        let q_rate = rung.bits_per_point as f64 * rung.code_rate.0 as f64 / rung.code_rate.1 as f64;
        assert_eq!(num(&row[3]), q_rate);
        assert_eq!(num(&row[6]), rung.threshold_db, "MCS {} threshold", rung.mcs);
        // Raw rates: nominal bits per data tone-symbol x data tones x symbols/s.
        for (col, width) in [(4, 24), (5, 56)] {
            let data_tones = width - params::pilots_for(width).len();
            let sym_per_s = params::FS as f64 / (params::N_USEFUL + params::CP_SHORT) as f64;
            let bps = q_rate * data_tones as f64 * sym_per_s;
            assert!((bps - num(&row[col])).abs() < 1.0, "MCS {} at {width}: {bps} vs {}", rung.mcs, row[col]);
        }
    }
}

#[test]
fn clip_levels_7_1() {
    let text = spec();
    let t = rows(&section(&text, "### 7.1"));
    let mut seen = 0;
    for row in t {
        let level = num(&row[1]);
        let rungs = row[0].trim_start_matches("MCS ");
        let (a, b) = match rungs.split_once('–') {
            Some((a, b)) => (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()),
            None => (rungs.parse().unwrap(), rungs.parse().unwrap()),
        };
        for m in a..=b {
            assert_eq!(params::RUNGS[m].clip_db, level, "MCS {m}");
            seen += 1;
        }
    }
    assert_eq!(seen, 7);
    assert_eq!(params::CLIP_DB_REFERENCE, 7.0);
}

#[test]
fn codes_6_3_and_a_3() {
    let text = spec();
    let t = rows(&section(&text, "### 6.3"));
    for (row, code) in t.iter().zip([params::HEADER_CODE, params::CONTROL_CODE, params::DATA_CODE]) {
        assert!(row[1].starts_with(&code.info_bits.to_string().replace("1024", "1,024")));
        assert!(row[2].contains(&format!("BG{}, Z = {}", code.base_graph, code.lifting)), "{row:?}");
        for &e in code.coded_bits {
            let printed = if e >= 1000 { format!("{},{:03}", e / 1000, e % 1000) } else { e.to_string() };
            assert!(row[3].contains(&printed), "{} {e}", code.name);
        }
    }
    let a3 = rows(&section(&text, "### A.3"));
    for row in a3.iter().filter(|r| !r[0].contains("HARQ")) {
        let e = num(&row[1]) as usize;
        let rungs: Vec<usize> = row[2].split(", ").map(|s| s.parse().unwrap()).collect();
        for m in rungs {
            assert_eq!(params::RUNGS[m].coded_bits, e);
            let (n, d) = params::RUNGS[m].code_rate;
            assert_eq!(row[0], format!("{n}/{d}"));
        }
    }
}

#[test]
fn interleaver_multipliers_6_4() {
    let text = spec();
    let para = section(&text, "### 6.4").into_iter().find(|l| l.starts_with("- **Interleaver.**")).unwrap();
    for (e, a) in [(288, "a = 179"), (432, "(269"), (512, "317 for control"), (2048, "1,267")] {
        assert!(para.contains(a), "{a}");
        let printed: usize = a
            .trim_start_matches("a = ")
            .trim_start_matches('(')
            .split(' ')
            .next()
            .unwrap()
            .replace(',', "")
            .parse()
            .unwrap();
        assert_eq!(interleave::multiplier(e), printed);
    }
    assert!(para.contains("949, 845 and 761"));
}

#[test]
fn s1_phases_6_1() {
    let text = spec();
    let line = section(&text, "### 6.1").into_iter().find(|l| l.contains("S1 on base tone k")).unwrap();
    let list = line.split("u = ").nth(1).unwrap().split(" (k").next().unwrap();
    let u: Vec<u32> = list.split(", ").map(|v| v.parse().unwrap()).collect();
    assert_eq!(u, params::S1_PHASE_UNITS);
    assert!(line.contains(&format!("c_init = {}", params::S2_C_INIT)));
}

#[test]
fn appendix_a_geometry() {
    let text = spec();
    let a1 = rows(&section(&text, "### A.1"));
    let get = |name: &str| a1.iter().find(|r| r[0].starts_with(name)).unwrap().clone();
    assert!(get("Header")[1].starts_with(&format!("{} symbols", params::header_symbols())));
    assert!(get("Control block")[1].starts_with(&format!("{} symbols", params::control_symbols_per_block())));
    assert!(get("Preamble")[1].starts_with(&format!("{} symbols", params::PREAMBLE_SYMBOLS)));
    assert!(get("Width probe")[1].starts_with(&format!("{} symbols", params::PROBE_SYMBOLS)));
    assert_eq!(
        num(&get("Block symbols available")[1]) as usize,
        params::block_symbols_available(4.0, params::CP_SHORT)
    );
    let a2 = rows(&section(&text, "### A.2"));
    for row in a2 {
        let mcs: u8 = row[0].parse().unwrap();
        let frame_s = if row[3].contains("2 s frame") { 2.0 } else { 4.0 };
        let counts: Vec<usize> = row[3].split(" (").next().unwrap().split(" / ").map(|v| v.parse().unwrap()).collect();
        assert_eq!(params::blocks_per_frame(mcs, 24, frame_s), counts[0], "MCS {mcs} at 24");
        assert_eq!(params::blocks_per_frame(mcs, 56, frame_s), counts[1], "MCS {mcs} at 56");
        let cbs: Vec<usize> = row[1].split(" / ").map(|v| v.parse().unwrap()).collect();
        let r = &params::RUNGS[mcs as usize];
        assert_eq!(cbs, vec![r.bits_per_point * 22, r.bits_per_point * 52]);
        let kbps: Vec<String> = row[4].split(" / ").map(str::to_string).collect();
        for (w, k) in [(24, &kbps[0]), (56, &kbps[1])] {
            assert_eq!(&format!("{:.2}", params::net_bps(mcs, w, frame_s) / 1000.0), k, "MCS {mcs} at {w} kbps");
        }
    }
    let data = get("Data-block symbols in a 4 s DATA frame");
    assert_eq!(data[1], params::data_block_symbols(24, 4.0).to_string());
    assert_eq!(data[2], params::data_block_symbols(56, 4.0).to_string());
}
