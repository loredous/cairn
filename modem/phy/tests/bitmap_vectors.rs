//! Every stage of every `vectors/bitmap/` frame (ADR 0005) and every
//! `vectors/frame/` frame (real SPEC 8.2 headers and Appendix B control
//! blocks): bits, I/Q levels, segment geometry, the tone grid (to its 9
//! printed decimals) and, where published, the time-domain samples (to their
//! 6 printed decimals).

mod common;

use cairn_phy::frame::{FrameConfig, LinkState};
use cairn_phy::tx::{Transmitter, block_stages, control_stream, header_stream};
use cairn_phy::{Receiver, RxOptions, gold, interleave, ldpc, mapping, params};
use common::*;
use serde_json::Value;

fn check_stages(name: &str, v: &Value, st: &cairn_phy::tx::BlockStages) {
    for (field, bits) in [
        ("info_hex", &st.info),
        ("rate_matched_hex", &st.rate_matched),
        ("scrambler_hex", &st.scrambler),
        ("scrambled_hex", &st.scrambled),
        ("interleaved_hex", &st.interleaved),
    ] {
        assert_eq!(bits_to_hex(bits), v[field].as_str().unwrap(), "{name} {field}");
    }
}

fn iq_flat(bits: &[u8], q: usize) -> Vec<i64> {
    mapping::iq_levels(bits, q).into_iter().flat_map(|(i, q)| [i as i64, q as i64]).collect()
}

fn link(doc: &Value) -> LinkState {
    let usable = match &doc["usable_tones"] {
        Value::Null => None,
        v => Some(usizes(v)),
    };
    LinkState { width: doc["width"].as_u64().unwrap() as usize, usable, cp: None, cap: None }
}

fn config(doc: &Value) -> FrameConfig {
    let cp = doc["prefix_samples"].as_u64().unwrap() as usize;
    if let Some(f) = doc["header"].get("fields") {
        // Frame vectors: the geometry comes from the header (SPEC 8.2).
        let cfg = FrameConfig::from_header(&header(f), &link(doc), cp).unwrap();
        assert_eq!(doc["clip_db"].as_f64().unwrap(), cfg.clip_db);
        return cfg.with_clip(false);
    }
    let usable = link(doc).usable;
    FrameConfig::new(
        doc["width"].as_u64().unwrap() as usize,
        doc["mcs"].as_u64().unwrap() as u8,
        doc["data_blocks"].as_u64().unwrap() as usize,
    )
    .unwrap()
    .with_cp(doc["prefix_samples"].as_u64().unwrap() as usize)
    .unwrap()
    .with_mask(usable)
    .unwrap()
    .with_clip(false)
}

fn check_case(path: &std::path::Path) -> (usize, usize) {
    let doc = read_json(path);
    let name = doc["name"].as_str().unwrap().to_string();
    assert_eq!(doc["format"], "cairn-bitmap-vectors");
    let cfg = config(&doc);
    assert_eq!(doc["control_blocks"].as_u64().unwrap() as usize, cfg.n_control, "{name}");
    assert_eq!(doc["data_blocks"].as_u64().unwrap() as usize, cfg.n_blocks, "{name}");
    let rung = cfg.rung();
    let q = rung.bits_per_point;
    assert_eq!(doc["modulation"].as_str().unwrap(), rung.modulation(), "{name}");
    assert_eq!(doc["bits_per_point"].as_u64().unwrap() as usize, q);
    assert_eq!(doc["code_rate"].as_str().unwrap(), format!("{}/{}", rung.code_rate.0, rung.code_rate.1));
    assert_eq!(doc["coded_bits"].as_u64().unwrap() as usize, rung.coded_bits);
    assert_eq!(doc["interleaver_a"].as_u64().unwrap() as usize, interleave::multiplier(rung.coded_bits));
    assert_eq!(usizes(&doc["masked_tones"]), cfg.masked, "{name}");
    assert_eq!(doc["total_symbols"].as_u64().unwrap() as usize, cfg.total_symbols(), "{name}");

    // Segment geometry.
    for seg in doc["segments"].as_array().unwrap() {
        let first = seg["first_symbol"].as_u64().unwrap() as usize;
        let n = seg["symbols"].as_u64().unwrap() as usize;
        match seg["kind"].as_str().unwrap() {
            "preamble" => {
                assert_eq!((first, n), (0, params::PREAMBLE_SYMBOLS));
                assert_eq!(usizes(&seg["tones"]), (0..params::BASE_TONES).collect::<Vec<_>>());
            }
            "header" => {
                assert_eq!((first, n), (cfg.first_header(), cfg.header_symbols()));
                assert_eq!(usizes(&seg["data_tones"]), cfg.base_data_tones);
                assert_eq!(usizes(&seg["pilot_tones"]), params::pilots_for(params::BASE_TONES));
                assert_eq!(seg["gain"].as_f64().unwrap(), 1.0);
            }
            "probe" => {
                assert!(cfg.has_probe);
                assert_eq!((first, n), (cfg.first_probe(), params::PROBE_SYMBOLS));
                assert_eq!(usizes(&seg["tones"]), (0..cfg.width).collect::<Vec<_>>());
                assert_eq!(usizes(&seg["pilot_tones"]), params::pilots_for(cfg.width));
            }
            "control" => {
                assert_eq!((first, n), (cfg.first_control(), cfg.control_symbols()), "{name}");
                assert_eq!(usizes(&seg["data_tones"]), cfg.control_tones, "{name}");
                assert_eq!(usizes(&seg["pilot_tones"]), cfg.control_pilots(), "{name}");
                assert_eq!(seg["gain"].as_f64().unwrap(), cfg.control_gain(), "{name}");
            }
            "data" => {
                assert_eq!((first, n), (cfg.first_data(), cfg.data_symbols()), "{name}");
                assert_eq!(usizes(&seg["data_tones"]), cfg.data_tones, "{name}");
                assert_eq!(usizes(&seg["pilot_tones"]), cfg.data_pilots, "{name}");
                assert_eq!(seg["gain"].as_f64().unwrap(), cfg.data_gain(), "{name}");
            }
            k => panic!("unknown segment {k}"),
        }
    }

    // Header.
    let h = &doc["header"];
    assert_eq!(h["position"], 0);
    assert_eq!(h["c_init"], 1);
    assert_eq!(h["coded_bits"].as_u64().unwrap() as usize, params::HEADER_CODED_BITS);
    assert_eq!(h["interleaver_a"].as_u64().unwrap() as usize, interleave::multiplier(params::HEADER_CODED_BITS));
    let hmsg = hex_to_bits(h["message_hex"].as_str().unwrap(), params::HEADER_INFO_BITS - 16);
    if let Some(f) = h.get("fields") {
        assert_eq!(header(f).message_bits().unwrap(), hmsg, "{name} header fields");
    }
    let hst = block_stages(ldpc::header_code(), &hmsg, params::HEADER_CODED_BITS, params::HEADER_POSITION);
    check_stages(&format!("{name} header"), h, &hst);
    let hstream = header_stream(&cfg, &hst.info);
    let h_pad = h["pad_bits"].as_u64().unwrap() as usize;
    assert_eq!(hstream.len(), params::HEADER_CODED_BITS + h_pad);
    assert_eq!(bits_to_hex(&hstream[params::HEADER_CODED_BITS..]), h["pad_hex"].as_str().unwrap());
    assert_eq!(bits_to_hex(&gold::pad_bits(h_pad)), h["pad_hex"].as_str().unwrap());
    let want: Vec<i64> = h["points_iq"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    assert_eq!(iq_flat(&hstream, 2), want, "{name} header points");

    // Control blocks (MCS 0, positions 1 .. n_control).
    let mut cmsgs = Vec::new();
    let mut cinfos = Vec::new();
    let ct = cfg.control_tones.len();
    let empty = Vec::new();
    for (i, c) in doc.get("controls").and_then(Value::as_array).unwrap_or(&empty).iter().enumerate() {
        assert_eq!(c["index"].as_u64().unwrap() as usize, i);
        assert_eq!(c["position"].as_u64().unwrap() as usize, 1 + i);
        assert_eq!(c["c_init"].as_u64().unwrap() as usize, 2 + i);
        let start_point = i * params::CONTROL_CODED_BITS / 2;
        assert_eq!(c["first_symbol_offset"].as_u64().unwrap() as usize, start_point / ct);
        assert_eq!(c["first_tone_slot"].as_u64().unwrap() as usize, start_point % ct);
        let msg = hex_to_bits(c["message_hex"].as_str().unwrap(), params::CONTROL_INFO_BITS - 16);
        assert_eq!(control(&c["fields"]).message_bits().unwrap(), msg, "{name} control {i} fields");
        let st = block_stages(ldpc::control_code(), &msg, params::CONTROL_CODED_BITS, 1 + i as u32);
        check_stages(&format!("{name} control {i}"), c, &st);
        cinfos.push(st.info.clone());
        cmsgs.push(msg);
    }
    assert_eq!(cmsgs.len(), cfg.n_control);
    if cfg.n_control > 0 {
        let cstream = control_stream(&cfg, &cinfos);
        let cd = &doc["control"];
        let c_pad = cd["pad_bits"].as_u64().unwrap() as usize;
        assert_eq!(cstream.len(), cfg.n_control * params::CONTROL_CODED_BITS + c_pad, "{name} control padding");
        assert_eq!(bits_to_hex(&cstream[cstream.len() - c_pad..]), cd["pad_hex"].as_str().unwrap());
        assert_eq!(bits_to_hex(&gold::pad_bits(c_pad)), cd["pad_hex"].as_str().unwrap());
        assert_eq!(cd["point_scale"].as_f64().unwrap(), mapping::axis_scale(2));
        let want: Vec<i64> = cd["points_iq"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(iq_flat(&cstream, 2), want, "{name} control points");
    }

    // Data blocks (positions 1 + n_control + index).
    let n_c = cfg.n_control;
    let mut payloads = Vec::new();
    let mut stream = Vec::new();
    let d = cfg.data_tones.len();
    for (i, b) in doc["blocks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(b["index"].as_u64().unwrap() as usize, i);
        assert_eq!(b["position"].as_u64().unwrap() as usize, 1 + n_c + i);
        assert_eq!(b["c_init"].as_u64().unwrap() as usize, 2 + n_c + i);
        let start_point = i * rung.coded_bits / q;
        assert_eq!(b["first_symbol_offset"].as_u64().unwrap() as usize, start_point / d);
        assert_eq!(b["first_tone_slot"].as_u64().unwrap() as usize, start_point % d);
        let payload = hex_to_bits(b["payload_hex"].as_str().unwrap(), params::DATA_PAYLOAD_BITS);
        let st = block_stages(ldpc::data_code(), &payload, rung.coded_bits, (1 + n_c + i) as u32);
        check_stages(&format!("{name} block {i}"), b, &st);
        stream.extend_from_slice(&st.interleaved);
        payloads.push(payload);
    }
    assert_eq!(doc.get("data").is_some(), cfg.n_blocks > 0, "{name}");
    if let Some(dd) = doc.get("data") {
        let d_pad = dd["pad_bits"].as_u64().unwrap() as usize;
        assert_eq!(stream.len() + d_pad, cfg.data_symbols() * d * q, "{name} data padding");
        let pad = gold::pad_bits(d_pad);
        assert_eq!(bits_to_hex(&pad), dd["pad_hex"].as_str().unwrap());
        stream.extend(pad);
        assert_eq!(dd["point_scale"].as_f64().unwrap(), mapping::axis_scale(q));
        let want: Vec<i64> = dd["points_iq"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(iq_flat(&stream, q), want, "{name} data points");
    }

    // Grid and samples from the frame builder.
    let mut txr = Transmitter::new();
    let frame = txr.build_with_controls(&cfg, &hmsg, &cmsgs, &payloads);
    assert_eq!(frame.header_bits, hst.info);
    assert_eq!(frame.control_bits, cinfos);
    if let Some(f) = doc["header"].get("fields") {
        // The same frame from the header and control block fields.
        let controls: Vec<_> =
            doc["controls"].as_array().map_or(Vec::new(), |a| a.iter().map(|c| control(&c["fields"])).collect());
        let mut l = link(&doc);
        l.cp = Some(cfg.cp);
        let mut hdr_frame = txr.build_frame(&header(f), &controls, &payloads, &l).unwrap();
        assert_eq!(hdr_frame.config.clip_db, cfg.clip_db);
        hdr_frame.config.clip = false;
        let unclipped = txr.build_with_controls(&hdr_frame.config, &hmsg, &cmsgs, &payloads);
        assert_eq!(unclipped.samples, frame.samples, "{name}");
    }
    let (re, im) = (doc["grid"]["re"].as_array().unwrap(), doc["grid"]["im"].as_array().unwrap());
    assert_eq!(re.len(), frame.grid.rows);
    let mut worst: f64 = 0.0;
    for (s, (r, i)) in re.iter().zip(im).enumerate() {
        let (r, i) = (f64s(r), f64s(i));
        assert_eq!(r.len(), cfg.width);
        for k in 0..cfg.width {
            let v = frame.grid.at(s, k);
            worst = worst.max((v.re - r[k]).abs()).max((v.im - i[k]).abs());
        }
    }
    // Printed to 9 decimals: rounding error at most 5e-10.
    assert!(worst <= 5.0e-10 + 1e-15, "{name}: grid differs by {worst:e}");
    let mut n_samples = 0;
    if let Some(samples) = doc.get("samples") {
        assert_eq!(doc["samples_scale"].as_f64().unwrap(), (2.0 / cfg.width as f64).sqrt());
        let want = f64s(samples);
        assert_eq!(want.len(), frame.samples.len(), "{name} sample count");
        let worst = want.iter().zip(&frame.samples).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(worst <= 5.0e-7 + 1e-12, "{name}: samples differ by {worst:e}");
        n_samples = want.len();
        if doc["header"].get("fields").is_some() {
            // The published samples decode header first, knowing only width and mask.
            let mut x = vec![0.0; 700];
            x.extend(&want);
            x.extend(vec![0.0; 700]);
            let r = Receiver::new().detect_and_receive(&x, &link(&doc), &RxOptions::default()).unwrap();
            assert_eq!(r.header.as_ref(), Some(&header(&doc["header"]["fields"])), "{name}");
            assert_eq!(r.config.as_ref().unwrap().cp, cfg.cp);
            assert_eq!(r.control_bits, cinfos, "{name}");
            assert!(r.control_ok.iter().chain(&r.block_ok).all(|&v| v), "{name}");
        }
    }
    (frame.grid.rows, n_samples)
}

#[test]
fn every_bitmap_vector() {
    let dir = repo().join("vectors/bitmap");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 13);
    let mut with_samples = 0;
    for p in &paths {
        let (_, n) = check_case(p);
        with_samples += (n > 0) as usize;
    }
    assert_eq!(with_samples, 3);
}

#[test]
fn every_frame_vector() {
    let dir = repo().join("vectors/frame");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 7);
    let mut with_samples = 0;
    for p in &paths {
        let (_, n) = check_case(p);
        with_samples += (n > 0) as usize;
    }
    assert!(with_samples >= 2);
}
