//! A mock capture device for tests and load measurements: samples from
//! memory through the same sink, ring and gap handling as a sound card.
//!
//! Paced, it behaves like ALSA capture: the "hardware" captures by the wall
//! clock whatever the scheduler does, its audio thread wakes once a period
//! (as the period interrupt would wake it) and collects every complete
//! period; when the thread wakes so late that more than a buffer of frames
//! is waiting, the stream overruns: the xrun is counted and the frames up to
//! the restart are lost, which the capture timestamps show. Unpaced, it
//! delivers as fast as the reader frees ring space, never losing a sample
//! except the buffers dropped on purpose ([`MockCapture::drops`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::capture::{Backend, CaptureSink, InputStream, NS};
use crate::rt::{self, RtConfig, RtGrant, ThreadRole};
use crate::stream::frames_ms;
use crate::{AudioError, SAMPLE_RATE};

/// A mock capture's settings.
#[derive(Clone, Debug)]
pub struct MockCapture {
    /// The audio "captured", full scale 1.
    pub samples: Vec<f32>,
    /// Device period, frames.
    pub period: usize,
    /// Device buffer, frames (paced only): more than this waiting when the
    /// audio thread wakes is an overrun.
    pub buffer: usize,
    /// Capture at real-time pace (see the module notes).
    pub paced: bool,
    /// Frames never delivered, as `(first frame, frames)`: buffers lost on
    /// the way, as in an xrun the backend did not report.
    pub drops: Vec<(u64, u64)>,
    /// Ring length, seconds.
    pub ring_seconds: f64,
    /// Real-time priority for the audio thread.
    pub rt: RtConfig,
}

impl MockCapture {
    /// Unpaced, 10 ms periods, nothing dropped.
    pub fn new(samples: Vec<f32>) -> Self {
        MockCapture {
            samples,
            period: 480,
            buffer: 9600,
            paced: false,
            drops: Vec::new(),
            ring_seconds: 8.0,
            rt: RtConfig { enabled: false, ..Default::default() },
        }
    }
}

/// The mock's audio thread; stops and joins on drop.
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

fn at(frame: u64) -> Duration {
    Duration::from_nanos(frame * NS / SAMPLE_RATE as u64)
}

/// Deliver frames `[a, b)` less the dropped ones, each run with its time.
fn deliver(sink: &mut CaptureSink, x: &[f32], a: u64, b: u64, drops: &[(u64, u64)]) {
    let mut s = a;
    while s < b {
        if let Some(&(d, n)) = drops.iter().find(|&&(d, n)| d <= s && s < d + n) {
            s = (d + n).min(b);
            continue;
        }
        let e = drops.iter().filter(|&&(d, _)| d > s && d < b).map(|&(d, _)| d).min().unwrap_or(b);
        sink.deliver((e - s) as usize, Some(at(s)), x[s as usize..e as usize].iter().copied());
        s = e;
    }
}

fn run(m: MockCapture, mut sink: CaptureSink, stop: &AtomicBool, t0: Instant) {
    let len = m.samples.len() as u64;
    let period = m.period.max(1) as u64;
    let mut pos = 0u64;
    if m.paced {
        let mut k = 0u64;
        while pos < len && !stop.load(Ordering::Relaxed) {
            k += 1;
            let due = t0 + at(k * period);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let hw = ((t0.elapsed().as_nanos() as u64).saturating_mul(SAMPLE_RATE as u64) / NS).min(len);
            if hw.saturating_sub(pos) > m.buffer as u64 {
                // Overrun: the stream restarts now; everything not collected is lost.
                sink.sh.xruns.fetch_add(1, Ordering::Relaxed);
                pos = hw - hw % period;
                continue;
            }
            while hw - pos >= period || (hw == len && pos < len) {
                let n = period.min(len - pos);
                deliver(&mut sink, &m.samples, pos, pos + n, &m.drops);
                pos += n;
            }
        }
    } else {
        // Room for a block and the zeros of any drop it follows: unpaced,
        // nothing is lost but the drops.
        let max_drop = m.drops.iter().map(|d| d.1).max().unwrap_or(0) as usize;
        while pos < len && !stop.load(Ordering::Relaxed) {
            let n = period.min(len - pos);
            if sink.slots() < n as usize + max_drop + sink.owed() as usize {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            deliver(&mut sink, &m.samples, pos, pos + n, &m.drops);
            pos += n;
        }
    }
    sink.sh.ended.store(true, Ordering::Release);
}

impl InputStream {
    /// Capture from memory through the mock device (see [`MockCapture`]).
    pub fn mock(m: MockCapture) -> Result<InputStream, AudioError> {
        let cap = (m.ring_seconds * SAMPLE_RATE as f64) as usize;
        let (sink, cons, gaps, sh) = CaptureSink::new(cap);
        let stop = Arc::new(AtomicBool::new(false));
        let (rt_tx, rt_rx) = std::sync::mpsc::channel::<(RtGrant, Instant)>();
        let format = format!(
            "mock{} x1 @ {SAMPLE_RATE} Hz, period {} ({:.1} ms), buffer {} ({:.1} ms)",
            if m.paced { ", real-time pace" } else { "" },
            m.period,
            frames_ms(m.period as u32),
            m.buffer,
            frames_ms(m.buffer as u32)
        );
        let (period, buffer) = (m.period as u32, m.buffer as u32);
        let stop_w = stop.clone();
        let thread = std::thread::Builder::new()
            .name("cairn-mock-capture".into())
            .spawn(move || {
                let g = rt::promote_current(ThreadRole::Audio, &m.rt);
                let t0 = Instant::now();
                let _ = rt_tx.send((g, t0));
                run(m, sink, &stop_w, t0);
            })
            .map_err(|e| AudioError::Backend(format!("mock capture thread: {e}")))?;
        let (rt, start) = match rt_rx.recv_timeout(Duration::from_secs(5)) {
            Ok((g, t0)) => (Some(g), Some(t0)),
            Err(_) => (None, None),
        };
        Ok(InputStream {
            _backend: Backend::Mock(Worker { stop, thread: Some(thread) }),
            cons,
            gaps,
            sh,
            capacity: cap.max(1) as u64,
            id: "mock".into(),
            format,
            period_frames: period,
            buffer_frames: buffer,
            backend: "mock",
            rt,
            start,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{AudioGap, GapCause};

    fn read_all(s: &mut InputStream) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0f32; 1000];
        let t0 = Instant::now();
        while !s.ended() && t0.elapsed() < Duration::from_secs(20) {
            let n = s.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        out
    }

    #[test]
    fn dropped_buffers_come_back_as_zeros_at_their_place() {
        let x: Vec<f32> = (0..48_000).map(|i| (i + 1) as f32).collect();
        let mut m = MockCapture::new(x.clone());
        m.ring_seconds = 0.05; // the reader has to keep up
        m.drops = vec![(4_800, 960), (20_000, 1_500)];
        let mut s = InputStream::mock(m).unwrap();
        let y = read_all(&mut s);
        assert_eq!(y.len(), x.len());
        for (i, (&a, &b)) in x.iter().zip(&y).enumerate() {
            let dropped = (4_800..5_760).contains(&i) || (20_000..21_500).contains(&i);
            assert_eq!(b, if dropped { 0.0 } else { a }, "sample {i}");
        }
        assert_eq!(
            s.take_gaps(),
            vec![
                AudioGap { at: 4_800, samples: 960, cause: GapCause::Lost },
                AudioGap { at: 20_000, samples: 1_500, cause: GapCause::Lost }
            ]
        );
        let st = s.stats();
        assert_eq!((st.gaps, st.filled_samples, st.xruns), (2, 2_460, 0));
        assert!(st.ring_high_samples <= st.ring_capacity);
    }

    #[test]
    fn paced_capture_keeps_time() {
        let x: Vec<f32> = (0..24_000).map(|i| (i % 100) as f32 / 100.0).collect();
        let mut m = MockCapture::new(x.clone());
        m.paced = true;
        let t0 = Instant::now();
        let mut s = InputStream::mock(m).unwrap();
        let y = read_all(&mut s);
        let wall = t0.elapsed().as_secs_f64();
        assert!(wall > 0.45, "real-time pace: {wall} s for 0.5 s");
        assert_eq!(y.len(), x.len(), "{:?}", s.stats());
        if s.stats().xruns == 0 {
            assert_eq!(y, x);
        }
    }
}
