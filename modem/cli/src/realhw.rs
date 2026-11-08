//! The real terminal and hardware behind `setup` and `selftest`
//! ([`cairn_modem::hw`]). Tests use the mocks; this file is what touches the
//! sound card and the PTT, and it keys only through the methods that take a
//! [`Confirmed`].

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cairn_audio::rt::{self, ThreadRole};
use cairn_audio::{InputStream, LevelMeter, OutputPort, OutputStream, RtConfig, RtGrant, StreamOptions};
use cairn_modem::detect::{self, PttCandidate};
use cairn_modem::hw::{AudioDev, Confirmed, Hardware, KeyReport, LevelReport, PttChoice, Terminal, TxMeasure};
use cairn_modem::txseq::{self, TxTiming};
use cairn_ptt::{PttGuard, PttSpec, SharedPtt};

use crate::signals;

/// stdin and stdout.
pub struct StdTerminal;

impl Terminal for StdTerminal {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }
    fn ask(&mut self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut l = String::new();
        match std::io::stdin().lock().read_line(&mut l) {
            Ok(0) | Err(_) => {
                println!();
                None
            }
            Ok(_) => Some(l.trim().to_string()),
        }
    }
}

/// The host's sound cards and PTT lines.
pub struct RealHardware;

/// `cm108:auto` becomes the one CM108-class hidraw node.
pub fn resolve_ptt_spec(spec: &str) -> Result<String, String> {
    match spec.split_once(':') {
        Some((k, "auto")) if k.eq_ignore_ascii_case("cm108") => {
            Ok(format!("cm108:{}", detect::resolve_cm108_auto(Path::new("/"))?.display()))
        }
        _ => Ok(spec.to_string()),
    }
}

fn open_ptt(p: &PttChoice) -> Result<SharedPtt, String> {
    let spec = PttSpec::parse(&resolve_ptt_spec(&p.spec)?, p.line, p.invert, p.gpio)?;
    let ptt = SharedPtt::new(spec.open().map_err(|e| format!("PTT {spec}: {e}"))?);
    signals::register_ptt(&ptt);
    Ok(ptt)
}

fn stream() -> StreamOptions {
    StreamOptions::default()
}

fn stopped() -> bool {
    signals::STOP.load(std::sync::atomic::Ordering::Relaxed)
}

fn measure(m: &txseq::TxReport, ptt: &SharedPtt, keyed_read: Option<bool>) -> TxMeasure {
    TxMeasure {
        keyed_ms: m.keyed_ms(),
        txdelay_ms: m.txdelay_ms(),
        txtail_ms: m.txtail_ms(),
        keyed_read,
        after_read: ptt.read_back().ok().flatten(),
    }
}

/// Transmit on this thread; what the line read while keyed comes from a
/// helper thread that samples it half way through TXDelay.
fn play(device: &str, ptt: &SharedPtt, timing: TxTiming, audio: &[f32]) -> Result<TxMeasure, String> {
    let mut out = OutputStream::open(device, &stream()).map_err(|e| e.to_string())?;
    if !out.wait_running(Duration::from_secs(2)) {
        return Err("the output device did not start".into());
    }
    std::thread::sleep(Duration::from_millis(100));
    let probe = ptt.clone();
    let wait = Duration::from_secs_f64(timing.txdelay_s / 2.0);
    let reader = std::thread::spawn(move || {
        std::thread::sleep(wait);
        probe.read_back().ok().flatten()
    });
    let r = txseq::transmit(&mut out, ptt, audio, &timing, &signals::STOP).map_err(|e| e.to_string());
    let keyed_read = reader.join().ok().flatten();
    let r = r?;
    Ok(measure(&r, ptt, keyed_read))
}

impl Hardware for RealHardware {
    fn audio_devices(&mut self) -> Result<Vec<AudioDev>, String> {
        Ok(cairn_audio::list_devices()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|d| AudioDev {
                id: d.id,
                description: d.description.join(" / "),
                input: d.input_48k,
                output: d.output_48k,
            })
            .collect())
    }

    fn ptt_candidates(&mut self) -> Vec<PttCandidate> {
        detect::ptt_candidates(Path::new("/"))
    }

    fn open_input(&mut self, device: &str) -> Result<String, String> {
        let mut input = InputStream::open(device, &stream()).map_err(|e| e.to_string())?;
        let mut buf = vec![0f32; 4800];
        let t0 = Instant::now();
        let mut got = 0;
        while t0.elapsed() < Duration::from_millis(500) {
            got += input.read(&mut buf).map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(10));
        }
        if got == 0 {
            return Err(format!("{} opened but delivered no audio in 0.5 s", input.id));
        }
        Ok(format!("{} ({}), {got} samples in 0.5 s", input.id, input.format))
    }

    fn open_output(&mut self, device: &str) -> Result<String, String> {
        let mut out = OutputStream::open(device, &stream()).map_err(|e| e.to_string())?;
        if !out.wait_running(Duration::from_secs(2)) {
            return Err(format!("{} opened but did not start", out.id));
        }
        Ok(format!("{} ({}), latency {:.1} ms", out.id, out.format, out.latency_ns() as f64 / 1e6))
    }

    fn measure_input(&mut self, device: &str, seconds: f64) -> Result<LevelReport, String> {
        let mut input = InputStream::open(device, &stream()).map_err(|e| e.to_string())?;
        let mut meter = LevelMeter::new();
        let mut buf = vec![0f32; 4800];
        let want = (seconds * 48_000.0) as u64;
        let t0 = Instant::now();
        while meter.samples() < want && !stopped() && t0.elapsed() < Duration::from_secs_f64(seconds + 3.0) {
            let n = input.read(&mut buf).map_err(|e| e.to_string())?;
            meter.push(&buf[..n]);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let l = meter.take();
        Ok(LevelReport { rms_dbfs: l.rms_dbfs, peak_dbfs: l.peak_dbfs, clipped: l.clipped })
    }

    fn ptt_idle(&mut self, p: &PttChoice) -> Result<(String, Option<bool>), String> {
        let ptt = open_ptt(p)?;
        let rb = ptt.read_back().map_err(|e| format!("read-back: {e}"))?;
        Ok((ptt.describe(), rb))
    }

    fn rt_check(&mut self) -> RtGrant {
        std::thread::spawn(|| rt::promote_current(ThreadRole::Audio, &RtConfig::default()))
            .join()
            .unwrap_or_else(|_| RtGrant::Denied("check panicked".into()))
    }

    fn capture_control(&mut self, device: &str, control: Option<&str>) -> Result<(String, f64), String> {
        let (_, id) = cairn_audio::find_device(device, cairn_audio::Direction::Input).map_err(|e| e.to_string())?;
        let mut c = cairn_audio::mixer::open_capture_gain(&id, control)?;
        Ok((c.name(), c.get_db()?))
    }

    fn port_free(&mut self, addr: &str) -> Result<(), String> {
        std::net::TcpListener::bind(addr).map(drop).map_err(|e| e.to_string())
    }

    fn executable(&mut self, path: &str) -> bool {
        fn exec(p: &Path) -> bool {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                p.is_file()
            }
        }
        if path.contains('/') {
            return exec(Path::new(path));
        }
        std::env::var_os("PATH").is_some_and(|ps| std::env::split_paths(&ps).any(|d| exec(&d.join(path))))
    }

    fn write_file(&mut self, path: &Path, text: &str) -> Result<Option<PathBuf>, String> {
        cairn_modem::config::write_with_backup(path, text).map_err(|e| e.to_string())
    }

    fn key_ptt(&mut self, _ok: &Confirmed, p: &PttChoice, seconds: f64) -> Result<KeyReport, String> {
        let ptt = open_ptt(p)?;
        let guard = PttGuard::key(&ptt).map_err(|e| format!("keying: {e}"))?;
        let keyed_read = ptt.read_back().ok().flatten();
        let t0 = Instant::now();
        while !stopped() && t0.elapsed() < Duration::from_secs_f64(seconds.min(5.0)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        guard.release().map_err(|e| format!("release: {e}"))?;
        Ok(KeyReport { keyed_read, after_read: ptt.read_back().ok().flatten() })
    }

    fn transmit(
        &mut self,
        _ok: &Confirmed,
        device: &str,
        p: &PttChoice,
        timing: TxTiming,
        audio: &[f32],
    ) -> Result<TxMeasure, String> {
        let ptt = open_ptt(p)?;
        let m = play(device, &ptt, timing, audio)?;
        // A second between transmissions: separate keyings, and the far
        // end's carrier detect sees the channel free again.
        std::thread::sleep(Duration::from_secs(1));
        Ok(m)
    }

    fn transmit_and_listen(
        &mut self,
        _ok: &Confirmed,
        device: &str,
        p: &PttChoice,
        timing: TxTiming,
        audio: &[f32],
        listen_s: f64,
    ) -> Result<(TxMeasure, Vec<f32>), String> {
        let ptt = open_ptt(p)?;
        let mut input = InputStream::open(device, &stream()).map_err(|e| e.to_string())?;
        let (dev, a, pt) = (device.to_string(), audio.to_vec(), ptt.clone());
        let tx = std::thread::spawn(move || play(&dev, &pt, timing, &a));
        let mut heard = Vec::new();
        let mut buf = vec![0f32; 4800];
        let mut done_at: Option<Instant> = None;
        let t0 = Instant::now();
        loop {
            let n = input.read(&mut buf).map_err(|e| e.to_string())?;
            heard.extend_from_slice(&buf[..n]);
            if done_at.is_none() && tx.is_finished() {
                done_at = Some(Instant::now());
            }
            if done_at.is_some_and(|t| t.elapsed() >= Duration::from_secs_f64(listen_s))
                || t0.elapsed() > Duration::from_secs(30)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let m = tx.join().map_err(|_| "the transmit thread panicked (PTT released)".to_string())??;
        Ok((m, heard))
    }
}
