//! Native ALSA capture (Linux, the `alsa` feature).
//!
//! cpal sizes the ALSA buffer at two periods, so with 10 ms periods an
//! audio thread that wakes more than about 10 ms late loses samples, which
//! happens on a desktop busy with parallel compiles. Here the period (how
//! often the thread wakes, hence the delivery latency) and the buffer (the
//! headroom before an overrun) are set separately: 10 ms periods in a
//! 200 ms buffer by default. The thread blocks in `snd_pcm_wait`, reads
//! each completed period with `snd_pcm_readi` into a preallocated buffer
//! and hands it to the [`CaptureSink`]; it never allocates while running.
//!
//! Capture times come from the PCM status timestamp (`CLOCK_MONOTONIC`,
//! the time of the newest frame) less the frames still waiting, so an
//! overrun (EPIPE, recovered with prepare and start) shows as a jump in
//! capture time and the sink puts exactly the missing samples back as zeros.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM, TstampType};
use alsa::{Direction, ValueOr};

use crate::capture::{Backend, CaptureSink, InputStream};
use crate::rt::{self, RtGrant, ThreadRole};
use crate::stream::{StreamOptions, frames_ms};
use crate::{AudioError, SAMPLE_RATE};

/// The capture thread; stops and joins on drop.
pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    S16,
    S32,
    F32,
}

fn err(id: &str, what: &str, e: alsa::Error) -> AudioError {
    let hint = match e.errno() {
        libc::EBUSY => " (another program, e.g. PipeWire, holds it; try the pipewire device or stop it)",
        libc::EACCES | libc::EPERM => " (is the user in the audio group?)",
        _ => "",
    };
    AudioError::Backend(format!("{id}: {what}: {e}{hint}"))
}

/// Hardware parameters: 48 kHz exactly, mono if offered, S16 before F32
/// before S32, the period and buffer asked for (nearest the device allows).
fn configure(pcm: &PCM, id: &str, period: u32, buffer: u32) -> Result<(Fmt, u32, u32, u32), AudioError> {
    let hw = HwParams::any(pcm).map_err(|e| err(id, "hw params", e))?;
    hw.set_access(Access::RWInterleaved).map_err(|e| err(id, "interleaved access", e))?;
    let fmt = if hw.set_format(Format::s16()).is_ok() {
        Fmt::S16
    } else if hw.set_format(Format::float()).is_ok() {
        Fmt::F32
    } else if hw.set_format(Format::s32()).is_ok() {
        Fmt::S32
    } else {
        return Err(AudioError::Config(format!("{id}: no S16, FLOAT or S32 capture format (try its plughw: device)")));
    };
    let ch = hw.set_channels_near(1).map_err(|e| err(id, "channels", e))?;
    hw.set_rate(SAMPLE_RATE, ValueOr::Nearest).map_err(|e| err(id, "48 kHz", e))?;
    let p = hw.set_period_size_near(period as alsa::pcm::Frames, ValueOr::Nearest).map_err(|e| err(id, "period", e))?;
    let b = hw.set_buffer_size_near((buffer as alsa::pcm::Frames).max(2 * p)).map_err(|e| err(id, "buffer size", e))?;
    pcm.hw_params(&hw).map_err(|e| err(id, "apply hw params", e))?;
    let cur = pcm.hw_params_current().map_err(|e| err(id, "hw params", e))?;
    let rate = cur.get_rate().map_err(|e| err(id, "rate", e))?;
    if rate != SAMPLE_RATE {
        return Err(AudioError::Config(format!("{id}: runs at {rate} Hz, not 48 kHz (try its plughw: device)")));
    }
    let p = cur.get_period_size().unwrap_or(p) as u32;
    let b = cur.get_buffer_size().unwrap_or(b) as u32;
    let sw = pcm.sw_params_current().map_err(|e| err(id, "sw params", e))?;
    sw.set_avail_min(p as alsa::pcm::Frames).map_err(|e| err(id, "avail min", e))?;
    sw.set_start_threshold(1).map_err(|e| err(id, "start threshold", e))?;
    sw.set_tstamp_mode(true).map_err(|e| err(id, "timestamps", e))?;
    sw.set_tstamp_type(TstampType::Monotonic).map_err(|e| err(id, "timestamp type", e))?;
    pcm.sw_params(&sw).map_err(|e| err(id, "apply sw params", e))?;
    Ok((fmt, ch, p, b))
}

fn monotonic() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Open and start capture on ALSA PCM `id`.
pub(crate) fn open(id: &str, opts: &StreamOptions) -> Result<InputStream, AudioError> {
    let period = opts.period_frames.unwrap_or(480);
    let pcm = PCM::new(id, Direction::Capture, false).map_err(|e| err(id, "open", e))?;
    let (fmt, ch, period, buffer) = configure(&pcm, id, period, opts.buffer_frames())?;
    let cap = (opts.capture_ring_seconds * SAMPLE_RATE as f64) as usize;
    let (sink, cons, gaps, sh) = CaptureSink::new(cap);
    let stop = Arc::new(AtomicBool::new(false));
    let (rt_tx, rt_rx) = std::sync::mpsc::channel::<RtGrant>();
    let rt_cfg = opts.rt.clone();
    let stop_w = stop.clone();
    let thread = std::thread::Builder::new()
        .name("cairn-capture".into())
        .spawn(move || {
            let _ = rt_tx.send(rt::promote_current(ThreadRole::Audio, &rt_cfg));
            run(pcm, fmt, ch as usize, period as usize, sink, &stop_w);
        })
        .map_err(|e| AudioError::Backend(format!("capture thread: {e}")))?;
    let rt = rt_rx.recv_timeout(Duration::from_secs(5)).ok();
    let name = match fmt {
        Fmt::S16 => "S16",
        Fmt::S32 => "S32",
        Fmt::F32 => "FLOAT",
    };
    let format = format!(
        "{name} x{ch} @ {SAMPLE_RATE} Hz, period {period} ({:.1} ms), buffer {buffer} ({:.1} ms), alsa",
        frames_ms(period),
        frames_ms(buffer)
    );
    Ok(InputStream {
        _backend: Backend::Alsa(Worker { stop, thread: Some(thread) }),
        cons,
        gaps,
        sh,
        capacity: cap.max(1) as u64,
        id: id.to_string(),
        format,
        period_frames: period,
        buffer_frames: buffer,
        backend: "alsa",
        rt,
        start: None,
    })
}

/// The capture loop (the audio thread).
fn run(pcm: PCM, fmt: Fmt, ch: usize, period: usize, mut sink: CaptureSink, stop: &AtomicBool) {
    // Preallocated before the stream starts; reused for every period.
    let mut s16 = vec![0i16; if fmt == Fmt::S16 { period * ch } else { 0 }];
    let mut s32 = vec![0i32; if fmt == Fmt::S32 { period * ch } else { 0 }];
    let mut f32b = vec![0f32; if fmt == Fmt::F32 { period * ch } else { 0 }];
    let io16 = (fmt == Fmt::S16).then(|| pcm.io_i16()).and_then(Result::ok);
    let io32 = (fmt == Fmt::S32).then(|| pcm.io_i32()).and_then(Result::ok);
    let iof = (fmt == Fmt::F32).then(|| pcm.io_f32()).and_then(Result::ok);
    let sh = sink.sh.clone();
    let mut t0: Option<Duration> = None;
    if pcm.start().is_err() {
        // A start threshold of 1 starts it on the first read anyway.
    }
    while !stop.load(Ordering::Relaxed) {
        match pcm.wait(Some(200)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                if recover(&pcm, &e, &sh) {
                    continue;
                }
                break;
            }
        }
        let r = match fmt {
            Fmt::S16 => io16.as_ref().map(|io| io.readi(&mut s16)),
            Fmt::S32 => io32.as_ref().map(|io| io.readi(&mut s32)),
            Fmt::F32 => iof.as_ref().map(|io| io.readi(&mut f32b)),
        };
        let n = match r {
            Some(Ok(n)) => n,
            Some(Err(e)) => {
                if recover(&pcm, &e, &sh) {
                    continue;
                }
                break;
            }
            None => {
                sh.dead.store(true, Ordering::Relaxed);
                break;
            }
        };
        if n == 0 {
            continue;
        }
        // Time of the newest captured frame, less the frames after ours.
        let (ts, avail) = match pcm.status() {
            Ok(st) => {
                let t = st.get_htstamp();
                let t = Duration::new(t.tv_sec as u64, t.tv_nsec as u32);
                (if t.is_zero() { monotonic() } else { t }, st.get_avail().max(0) as u64)
            }
            Err(_) => (monotonic(), 0),
        };
        let back = Duration::from_nanos((avail + n as u64) * 1_000_000_000 / SAMPLE_RATE as u64);
        let first = ts.saturating_sub(back);
        let base = *t0.get_or_insert(first);
        let cap = Some(first.saturating_sub(base));
        match fmt {
            Fmt::S16 => sink.deliver(n, cap, s16.chunks_exact(ch).take(n).map(|f| f[0] as f32 / 32768.0)),
            Fmt::S32 => sink.deliver(n, cap, s32.chunks_exact(ch).take(n).map(|f| f[0] as f32 / 2_147_483_648.0)),
            Fmt::F32 => sink.deliver(n, cap, f32b.chunks_exact(ch).take(n).map(|f| f[0])),
        }
    }
    let _ = pcm.drop();
}

/// Recover from a capture error: an overrun (EPIPE) is counted and
/// restarted, a suspend resumed; false when the device is gone.
fn recover(pcm: &PCM, e: &alsa::Error, sh: &crate::capture::InShared) -> bool {
    match e.errno() {
        libc::EPIPE => {
            sh.xruns.fetch_add(1, Ordering::Relaxed);
            pcm.prepare().is_ok() && pcm.start().is_ok()
        }
        libc::ESTRPIPE => {
            sh.xruns.fetch_add(1, Ordering::Relaxed);
            for _ in 0..50 {
                match pcm.resume() {
                    Ok(()) => return true,
                    Err(e) if e.errno() == libc::EAGAIN => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            pcm.prepare().is_ok() && pcm.start().is_ok()
        }
        libc::EAGAIN | libc::EINTR => true,
        libc::ENODEV | libc::EBADFD | libc::ENXIO => {
            sh.dead.store(true, Ordering::Relaxed);
            false
        }
        _ => {
            sh.errors.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(10));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens ALSA's software `null` PCM (no sound card): the open, the
    /// parameter negotiation and the read loop run. It returns frames as fast
    /// as they are read, so nothing about timing is checked. Opt-in: some
    /// build hosts have no ALSA configuration at all.
    #[test]
    #[ignore = "needs an ALSA configuration with the null PCM (cargo test -- --ignored)"]
    fn null_pcm_capture_runs() {
        let opts = StreamOptions { buffer_ms: 100.0, ..Default::default() };
        let mut s = open("null", &opts).expect("open null");
        assert_eq!(s.period_frames, 480);
        assert!(s.buffer_frames >= 960, "{}", s.format);
        let mut buf = vec![0f32; 4800];
        let t0 = std::time::Instant::now();
        let mut got = 0;
        while got < 48_000 && t0.elapsed() < Duration::from_secs(5) {
            got += s.read(&mut buf).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(got >= 48_000, "{got}");
        assert!(buf.iter().all(|&v| v == 0.0));
    }
}
