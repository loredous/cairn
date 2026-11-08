//! The capture side common to every backend: the audio thread's half
//! ([`CaptureSink`]) that turns device buffers into a gap-free stream in a
//! lock-free ring, and the reader's half ([`InputStream`]).
//!
//! Sample timing is kept exact across losses. Whenever the capture
//! timestamps show that samples never reached us (an xrun, a dropped
//! buffer), or the ring was full when they arrived (the reader fell
//! behind), the sink puts exactly as many zero samples in their place and
//! reports the interval as an [`AudioGap`]. Stream index *i* is therefore
//! always the *i*-th sample period since the stream started, so frames
//! after a gap decode at their true times, and a frame that spans one can
//! be flagged. The sink never allocates, locks or blocks: it writes to
//! preallocated `rtrb` rings and atomics only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::rt::RtGrant;
use crate::{AudioError, SAMPLE_RATE};

pub(crate) const NS: u64 = 1_000_000_000;

/// Why samples are missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapCause {
    /// Never captured: the capture timestamps jumped (a backend xrun, a
    /// dropped buffer).
    Lost,
    /// Captured but dropped because the ring was full (the reader fell behind).
    Overflow,
}

/// Samples replaced by zeros in the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioGap {
    /// Stream index of the first zero sample.
    pub at: u64,
    /// Zero samples standing in for the missing ones.
    pub samples: u64,
    /// Why.
    pub cause: GapCause,
}

impl AudioGap {
    /// Length, ms.
    pub fn ms(&self) -> f64 {
        self.samples as f64 * 1e3 / SAMPLE_RATE as f64
    }
}

#[derive(Default)]
pub(crate) struct InShared {
    pub frames: AtomicU64,
    pub overflow_samples: AtomicU64,
    pub overflow_events: AtomicU64,
    pub xruns: AtomicU64,
    pub gaps: AtomicU64,
    pub gap_ns: AtomicU64,
    pub filled: AtomicU64,
    pub gap_events_lost: AtomicU64,
    pub errors: AtomicU64,
    pub period: AtomicU64,
    pub ring_high: AtomicU64,
    pub dead: AtomicBool,
    /// The source ran out (mock backend): nothing more will arrive.
    pub ended: AtomicBool,
}

/// Input counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Frames delivered by the device.
    pub frames: u64,
    /// Samples dropped because the ring was full (the reader fell behind).
    pub overflow_samples: u64,
    /// Callbacks that dropped samples.
    pub overflow_events: u64,
    /// Backend-reported xruns (ALSA overruns, recovered with prepare/start).
    pub xruns: u64,
    /// Discontinuities seen in the capture timestamps (samples lost before
    /// they reached us, whether or not the backend reported an xrun).
    pub gaps: u64,
    /// Total time lost in those gaps, ns.
    pub gap_ns: u64,
    /// Zero samples put in the stream in place of lost or dropped ones.
    pub filled_samples: u64,
    /// Other backend errors.
    pub errors: u64,
    /// Last callback size, frames.
    pub period_frames: u64,
    /// Highest ring fill seen, samples (how far the reader fell behind).
    pub ring_high_samples: u64,
    /// Ring capacity, samples.
    pub ring_capacity: u64,
}

impl InputStats {
    /// Any sample lost anywhere.
    pub fn lost_any(&self) -> bool {
        self.overflow_samples + self.xruns + self.gaps > 0
    }

    /// Ring high-water mark, ms.
    pub fn ring_high_ms(&self) -> f64 {
        self.ring_high_samples as f64 * 1e3 / SAMPLE_RATE as f64
    }

    /// Ring length, ms.
    pub fn ring_ms(&self) -> f64 {
        self.ring_capacity as f64 * 1e3 / SAMPLE_RATE as f64
    }
}

/// The audio thread's half: device buffers in, a gap-free stream out.
pub(crate) struct CaptureSink {
    prod: Producer<f32>,
    gaps: Producer<AudioGap>,
    pub sh: Arc<InShared>,
    capacity: u64,
    /// Expected capture time of the next frame (backend time base).
    next_capture: Option<Duration>,
    /// Stream index of the next sample (written plus owed).
    idx: u64,
    /// Zeros still to write before any new sample.
    owed: u64,
    /// An overflow in progress, reported before its first zero is written.
    overflow: Option<AudioGap>,
}

fn dur_frames(n: usize) -> Duration {
    Duration::from_nanos(n as u64 * NS / SAMPLE_RATE as u64)
}

impl CaptureSink {
    /// A sink and its reader's rings.
    pub fn new(ring_samples: usize) -> (CaptureSink, Consumer<f32>, Consumer<AudioGap>, Arc<InShared>) {
        let (prod, cons) = RingBuffer::new(ring_samples.max(1));
        let (gtx, grx) = RingBuffer::new(256);
        let sh = Arc::new(InShared::default());
        let sink = CaptureSink {
            prod,
            gaps: gtx,
            sh: sh.clone(),
            capacity: ring_samples.max(1) as u64,
            next_capture: None,
            idx: 0,
            owed: 0,
            overflow: None,
        };
        (sink, cons, grx, sh)
    }

    /// Free ring slots (the mock backend waits for room when not paced).
    pub fn slots(&self) -> usize {
        self.prod.slots()
    }

    /// Zeros not yet written.
    pub fn owed(&self) -> u64 {
        self.owed
    }

    fn report(&mut self, g: AudioGap) {
        if self.gaps.push(g).is_err() {
            self.sh.gap_events_lost.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn lose(&mut self, n: u64, cause: GapCause) {
        if n == 0 {
            return;
        }
        match cause {
            GapCause::Lost => {
                if let Some(o) = self.overflow.take() {
                    self.report(o);
                }
                self.report(AudioGap { at: self.idx, samples: n, cause });
            }
            GapCause::Overflow => match self.overflow.as_mut() {
                Some(o) if o.at + o.samples == self.idx => o.samples += n,
                _ => {
                    if let Some(o) = self.overflow.take() {
                        self.report(o);
                    }
                    self.overflow = Some(AudioGap { at: self.idx, samples: n, cause });
                }
            },
        }
        self.owed += n;
        self.idx += n;
        self.sh.filled.fetch_add(n, Ordering::Relaxed);
    }

    fn flush_owed(&mut self) {
        if self.owed == 0 {
            return;
        }
        let k = (self.prod.slots() as u64).min(self.owed) as usize;
        if k == 0 {
            return;
        }
        // The reader learns of a gap before it sees any of its zeros.
        if let Some(o) = self.overflow.take() {
            self.report(o);
        }
        let chunk = self.prod.write_chunk_uninit(k).expect("slots checked");
        chunk.fill_from_iter(std::iter::repeat(0.0f32));
        self.owed -= k as u64;
    }

    fn note_fill(&self) {
        let fill = self.capacity - self.prod.slots() as u64;
        self.sh.ring_high.fetch_max(fill, Ordering::Relaxed);
    }

    /// One device buffer of `frames` samples whose first was captured at
    /// `capture` (the backend's time base, from the stream's start; None
    /// when the backend gives no time).
    pub fn deliver<I: Iterator<Item = f32>>(&mut self, frames: usize, capture: Option<Duration>, samples: I) {
        let s = self.sh.clone();
        s.frames.fetch_add(frames as u64, Ordering::Relaxed);
        s.period.store(frames as u64, Ordering::Relaxed);
        if let Some(t) = capture {
            if let Some(expect) = self.next_capture {
                let period = dur_frames(frames);
                // Capture times jitter (USB frames are 1 ms); a real loss is
                // at least a period.
                let tol = (period * 2 / 5).max(Duration::from_millis(4));
                if t > expect + tol {
                    let lost = (t - expect).as_nanos() as u64;
                    s.gaps.fetch_add(1, Ordering::Relaxed);
                    s.gap_ns.fetch_add(lost, Ordering::Relaxed);
                    let n = (lost as u128 * SAMPLE_RATE as u128 + NS as u128 / 2) / NS as u128;
                    self.lose(n as u64, GapCause::Lost);
                }
            }
            self.next_capture = Some(t + dur_frames(frames));
        }
        self.flush_owed();
        if frames == 0 {
            return;
        }
        if self.owed > 0 {
            // Still no room for the zeros: these samples go too.
            s.overflow_samples.fetch_add(frames as u64, Ordering::Relaxed);
            s.overflow_events.fetch_add(1, Ordering::Relaxed);
            self.lose(frames as u64, GapCause::Overflow);
            self.note_fill();
            return;
        }
        let n = self.prod.slots().min(frames);
        if n > 0 {
            let chunk = self.prod.write_chunk_uninit(n).expect("slots checked");
            chunk.fill_from_iter(samples);
            self.idx += n as u64;
        }
        if n < frames {
            s.overflow_samples.fetch_add((frames - n) as u64, Ordering::Relaxed);
            s.overflow_events.fetch_add(1, Ordering::Relaxed);
            self.lose((frames - n) as u64, GapCause::Overflow);
        }
        self.note_fill();
    }
}

/// What keeps a capture running (dropped with the stream).
pub(crate) enum Backend {
    Cpal(#[allow(dead_code)] cpal::Stream),
    #[cfg(all(target_os = "linux", feature = "alsa"))]
    Alsa(#[allow(dead_code)] crate::alsa_in::Worker),
    Mock(#[allow(dead_code)] crate::mock::Worker),
}

/// A capture stream feeding a ring buffer of f32 samples (mono, 48 kHz),
/// gap-free in time (see the module notes): read the samples with
/// [`InputStream::read`] and the intervals that are zeros in their place
/// with [`InputStream::take_gaps`].
pub struct InputStream {
    pub(crate) _backend: Backend,
    pub(crate) cons: Consumer<f32>,
    pub(crate) gaps: Consumer<AudioGap>,
    pub(crate) sh: Arc<InShared>,
    pub(crate) capacity: u64,
    /// Device id opened.
    pub id: String,
    /// Format and sizes, e.g. `S16_LE x1 @ 48000 Hz, period 480 (10.0 ms), buffer 9600 (200.0 ms), alsa`.
    pub format: String,
    /// Effective device period, frames (0 when unknown).
    pub period_frames: u32,
    /// Effective device buffer, frames (0 when unknown).
    pub buffer_frames: u32,
    /// The backend: `alsa`, `cpal` or `mock`.
    pub backend: &'static str,
    /// What the audio thread got from the scheduler (None: not known yet).
    pub rt: Option<RtGrant>,
    pub(crate) start: Option<std::time::Instant>,
}

impl InputStream {
    /// When stream index 0 was captured, where the backend knows it exactly
    /// (the paced mock device: index *i* is captured at this time plus
    /// *i*/48 kHz), for turnaround measurements.
    pub fn capture_start(&self) -> Option<std::time::Instant> {
        self.start
    }

    /// Samples waiting in the ring.
    pub fn available(&self) -> usize {
        self.cons.slots()
    }

    /// Move up to `out.len()` samples out of the ring without blocking.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let n = self.cons.slots().min(out.len());
        if n == 0 {
            if self.sh.dead.load(Ordering::Relaxed) {
                return Err(AudioError::StreamDead);
            }
            return Ok(0);
        }
        let chunk = self.cons.read_chunk(n).expect("slots checked");
        let (a, b) = chunk.as_slices();
        out[..a.len()].copy_from_slice(a);
        out[a.len()..n].copy_from_slice(b);
        chunk.commit_all();
        Ok(n)
    }

    /// Gaps reported since the last call, in stream order. A gap is
    /// reported before any of its zero samples can be read.
    pub fn take_gaps(&mut self) -> Vec<AudioGap> {
        let mut v = Vec::new();
        while let Ok(g) = self.gaps.pop() {
            v.push(g);
        }
        v
    }

    /// The source has ended and every sample was read (mock backend; a
    /// device never ends).
    pub fn ended(&self) -> bool {
        self.sh.ended.load(Ordering::Acquire) && self.cons.slots() == 0
    }

    /// Counters.
    pub fn stats(&self) -> InputStats {
        let s = &self.sh;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        InputStats {
            frames: l(&s.frames),
            overflow_samples: l(&s.overflow_samples),
            overflow_events: l(&s.overflow_events),
            xruns: l(&s.xruns),
            gaps: l(&s.gaps),
            gap_ns: l(&s.gap_ns),
            filled_samples: l(&s.filled),
            errors: l(&s.errors),
            period_frames: l(&s.period),
            ring_high_samples: l(&s.ring_high),
            ring_capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(x: f64) -> Duration {
        Duration::from_secs_f64(x / 1e3)
    }

    fn drain(c: &mut Consumer<f32>) -> Vec<f32> {
        let mut v = Vec::new();
        while let Ok(x) = c.pop() {
            v.push(x);
        }
        v
    }

    #[test]
    fn timestamp_gap_becomes_exact_zeros() {
        let (mut sink, mut cons, mut gaps, sh) = CaptureSink::new(48_000);
        let block = |k: usize| (0..480).map(move |i| (k * 480 + i) as f32 + 1.0);
        sink.deliver(480, Some(ms(0.0)), block(0));
        sink.deliver(480, Some(ms(10.0)), block(1));
        // Blocks 2 and 3 never arrive; block 4 is captured at 40 ms.
        sink.deliver(480, Some(ms(40.0)), block(4));
        // Small jitter is not a gap.
        sink.deliver(480, Some(ms(51.0)), block(5));
        let v = drain(&mut cons);
        assert_eq!(v.len(), 6 * 480, "the stream keeps every sample period");
        assert_eq!(v[2 * 480 - 1], (2 * 480) as f32);
        assert!(v[2 * 480..4 * 480].iter().all(|&x| x == 0.0));
        assert_eq!(v[4 * 480], (4 * 480) as f32 + 1.0, "later samples at their true index");
        let g = gaps.pop().unwrap();
        assert_eq!(g, AudioGap { at: 960, samples: 960, cause: GapCause::Lost });
        assert!((g.ms() - 20.0).abs() < 1e-9);
        assert!(gaps.pop().is_err());
        assert_eq!(sh.gaps.load(Ordering::Relaxed), 1);
        assert_eq!(sh.filled.load(Ordering::Relaxed), 960);
    }

    #[test]
    fn overflow_keeps_alignment() {
        // A ring of 1000 samples; the reader stalls, then catches up.
        let (mut sink, mut cons, mut gaps, sh) = CaptureSink::new(1000);
        for k in 0..4 {
            sink.deliver(480, Some(ms(10.0 * k as f64)), (0..480).map(move |i| (k * 480 + i) as f32 + 1.0));
        }
        assert_eq!(sh.ring_high.load(Ordering::Relaxed), 1000);
        let first = drain(&mut cons);
        assert_eq!(first.len(), 1000);
        // Blocks 4 and 5 arrive while the reader catches up: the owed zeros
        // come first, so block 4 only partly fits.
        let mut all = first;
        for k in 4..6 {
            sink.deliver(480, Some(ms(10.0 * k as f64)), (0..480).map(move |i| (k * 480 + i) as f32 + 1.0));
            all.extend(drain(&mut cons));
        }
        assert_eq!(all.len(), 6 * 480, "every sample period is in the stream");
        for (i, &v) in all.iter().enumerate() {
            assert!(v == 0.0 || v == i as f32 + 1.0, "sample {i} = {v} is at its true index");
        }
        assert!(all[1000..1920].iter().all(|&x| x == 0.0));
        assert_eq!(all[1920], 1921.0);
        assert_eq!(gaps.pop().unwrap(), AudioGap { at: 1000, samples: 920, cause: GapCause::Overflow });
        assert_eq!(gaps.pop().unwrap(), AudioGap { at: 2000, samples: 400, cause: GapCause::Overflow });
        assert_eq!(sh.overflow_samples.load(Ordering::Relaxed), 1320);
        assert_eq!(sh.filled.load(Ordering::Relaxed), 1320);
    }
}
