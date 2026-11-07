//! Opt-in checks against real hardware. Ignored by default (and in CI);
//! run them with a device and, optionally, a PTT:
//!
//! ```sh
//! CAIRN_TEST_DEVICE=All-In-One-Cable CAIRN_TEST_PTT=serial:All-In-One-Cable \
//!     cargo test --release -p cairn-modem --test hardware -- --ignored --test-threads 1
//! ```
//!
//! The PTT is keyed for under a second in total. Treat it as live.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use cairn_audio::{InputStream, OutputPort, OutputStream, StreamOptions};
use cairn_modem::frames::level_tone;
use cairn_modem::txseq::{TxTiming, transmit};
use cairn_ptt::{NullPtt, PttSpec, SerialLineSpec, SharedPtt};

fn device() -> Option<String> {
    std::env::var("CAIRN_TEST_DEVICE").ok().filter(|s| !s.is_empty())
}

fn ptt() -> Option<SharedPtt> {
    let spec = std::env::var("CAIRN_TEST_PTT").ok().filter(|s| !s.is_empty())?;
    let spec = PttSpec::parse(&spec, SerialLineSpec::Dtr, false, 3).expect("CAIRN_TEST_PTT");
    Some(SharedPtt::new(spec.open().expect("open PTT")))
}

#[test]
#[ignore = "needs CAIRN_TEST_DEVICE"]
fn capture_two_seconds_without_loss() {
    let Some(dev) = device() else { return };
    let mut input = InputStream::open(&dev, &StreamOptions::default()).expect("open input");
    let mut buf = vec![0f32; 4800];
    let mut got = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        got += input.read(&mut buf).expect("read");
        std::thread::sleep(Duration::from_millis(5));
    }
    let s = input.stats();
    assert!((88_000..=104_000).contains(&got), "{got} samples in 2 s");
    assert!(!s.lost_any(), "{s:?}");
}

#[test]
#[ignore = "needs CAIRN_TEST_DEVICE"]
fn timed_transmission() {
    let Some(dev) = device() else { return };
    let ptt = ptt().unwrap_or_else(|| SharedPtt::new(Box::new(NullPtt::new(false))));
    let mut out = OutputStream::open(&dev, &StreamOptions::default()).expect("open output");
    assert!(out.wait_running(Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(100));
    let audio: Vec<f32> = level_tone(0.3).iter().map(|&v| v as f32).collect();
    let timing = TxTiming { txdelay_s: 0.100, txtail_s: 0.010 };
    let r = transmit(&mut out, &ptt, &audio, &timing, &AtomicBool::new(false)).expect("transmit");
    assert!((r.txdelay_ms() - 100.0).abs() < 2.0, "TXDelay {} ms", r.txdelay_ms());
    assert!(r.txtail_ms() >= 9.9 && r.txtail_ms() < 20.0, "TXTail {} ms", r.txtail_ms());
    assert_eq!(r.underruns, 0);
    assert_ne!(ptt.read_back().unwrap(), Some(true), "released");
    assert_eq!(out.stats().xruns, 0);
}

#[test]
#[ignore = "needs CAIRN_TEST_PTT"]
fn ptt_keys_and_releases() {
    let Some(p) = ptt() else { return };
    let g = cairn_ptt::PttGuard::key(&p).expect("key");
    if let Some(k) = p.read_back().unwrap() {
        assert!(k, "reads keyed");
    }
    std::thread::sleep(Duration::from_millis(200));
    g.release().expect("release");
    if let Some(k) = p.read_back().unwrap() {
        assert!(!k, "reads unkeyed");
    }
}
