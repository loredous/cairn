//! Input and output streams with lock-free rings and timing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::device::{Direction, find_device};
use crate::{AudioError, SAMPLE_RATE};

const NS: u64 = 1_000_000_000;

/// Stream settings.
#[derive(Clone, Debug)]
pub struct StreamOptions {
    /// Callback period in frames (`None`: the backend's default). The ALSA
    /// backend double-buffers, so output latency is about two periods.
    pub period_frames: Option<u32>,
    /// Ring buffer length, seconds.
    pub ring_seconds: f64,
}

impl Default for StreamOptions {
    fn default() -> Self {
        StreamOptions { period_frames: Some(480), ring_seconds: 4.0 }
    }
}

/// Sample types the callbacks convert (the formats USB interfaces offer).
trait Sample: cpal::SizedSample + Send + 'static {
    const SILENCE: Self;
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl Sample for i16 {
    const SILENCE: Self = 0;
    fn to_f32(self) -> f32 {
        self as f32 / 32768.0
    }
    fn from_f32(v: f32) -> Self {
        (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
    }
}

impl Sample for f32 {
    const SILENCE: Self = 0.0;
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(v: f32) -> Self {
        v.clamp(-1.0, 1.0)
    }
}

/// The 48 kHz format to open: mono before more channels, i16 before f32.
fn pick_format(dev: &cpal::Device, dir: Direction) -> Result<(SampleFormat, u16, SupportedBufferSize), AudioError> {
    let ranges: Vec<cpal::SupportedStreamConfigRange> = match dir {
        Direction::Input => dev.supported_input_configs()?.collect(),
        Direction::Output => dev.supported_output_configs()?.collect(),
    };
    let mut ok: Vec<_> = ranges
        .into_iter()
        .filter(|c| c.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= c.max_sample_rate())
        .filter(|c| matches!(c.sample_format(), SampleFormat::I16 | SampleFormat::F32))
        .collect();
    ok.sort_by_key(|c| (c.channels() != 1, c.channels(), c.sample_format() != SampleFormat::I16));
    let c = ok.first().ok_or_else(|| {
        AudioError::Config("the device offers no 48 kHz i16 or f32 format (try its plughw: device)".into())
    })?;
    Ok((c.sample_format(), c.channels(), *c.buffer_size()))
}

fn stream_config(channels: u16, sizes: SupportedBufferSize, opts: &StreamOptions) -> StreamConfig {
    let buffer_size = match (opts.period_frames, sizes) {
        (Some(p), SupportedBufferSize::Range { min, max }) if (min..=max).contains(&p) => BufferSize::Fixed(p),
        (Some(p), SupportedBufferSize::Unknown) => BufferSize::Fixed(p),
        _ => BufferSize::Default,
    };
    StreamConfig { channels, sample_rate: SAMPLE_RATE, buffer_size }
}

fn ns_since(origin: Instant) -> u64 {
    origin.elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Input

#[derive(Default)]
struct InShared {
    frames: AtomicU64,
    overflow_samples: AtomicU64,
    overflow_events: AtomicU64,
    xruns: AtomicU64,
    gaps: AtomicU64,
    gap_ns: AtomicU64,
    errors: AtomicU64,
    period: AtomicU64,
    dead: AtomicBool,
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
    /// Other backend errors.
    pub errors: u64,
    /// Last callback size, frames.
    pub period_frames: u64,
}

impl InputStats {
    /// Any sample lost anywhere.
    pub fn lost_any(&self) -> bool {
        self.overflow_samples + self.xruns + self.gaps > 0
    }
}

/// A capture stream feeding a ring buffer of f32 samples (mono, 48 kHz).
pub struct InputStream {
    _stream: cpal::Stream,
    cons: Consumer<f32>,
    sh: Arc<InShared>,
    /// Device id opened.
    pub id: String,
    /// Format description, e.g. `i16 x1 @ 48000 Hz, period 480`.
    pub format: String,
}

fn build_input<T: Sample>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    mut prod: Producer<f32>,
    sh: Arc<InShared>,
) -> Result<cpal::Stream, cpal::Error> {
    let ch = cfg.channels as usize;
    let sh_err = sh.clone();
    // Gap detection state, owned by the callback.
    let mut next_capture: Option<Duration> = None;
    let mut first_capture: Option<cpal::StreamInstant> = None;
    dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let frames = data.len() / ch;
            sh.frames.fetch_add(frames as u64, Ordering::Relaxed);
            sh.period.store(frames as u64, Ordering::Relaxed);
            // Timestamp continuity, in the stream's own time base.
            let cap = info.timestamp().capture;
            let t0 = *first_capture.get_or_insert(cap);
            let t = cap.duration_since(t0);
            if let Some(expect) = next_capture {
                let period = Duration::from_nanos(frames as u64 * NS / SAMPLE_RATE as u64);
                let tol = (period * 2 / 5).max(Duration::from_millis(4));
                if t > expect + tol {
                    sh.gaps.fetch_add(1, Ordering::Relaxed);
                    sh.gap_ns.fetch_add((t - expect).as_nanos() as u64, Ordering::Relaxed);
                }
            }
            next_capture = Some(t + Duration::from_nanos(frames as u64 * NS / SAMPLE_RATE as u64));

            let n = prod.slots().min(frames);
            if n < frames {
                sh.overflow_samples.fetch_add((frames - n) as u64, Ordering::Relaxed);
                sh.overflow_events.fetch_add(1, Ordering::Relaxed);
            }
            if n > 0 {
                let chunk = prod.write_chunk_uninit(n).expect("slots checked");
                chunk.fill_from_iter(data.chunks_exact(ch).map(|f| f[0].to_f32()));
            }
        },
        move |e: cpal::Error| match e.kind() {
            cpal::ErrorKind::Xrun => {
                sh_err.xruns.fetch_add(1, Ordering::Relaxed);
            }
            cpal::ErrorKind::DeviceNotAvailable => sh_err.dead.store(true, Ordering::Relaxed),
            _ => {
                sh_err.errors.fetch_add(1, Ordering::Relaxed);
            }
        },
        None,
    )
}

impl InputStream {
    /// Open the capture device matching `query` (see [`find_device`]) and start it.
    pub fn open(query: &str, opts: &StreamOptions) -> Result<Self, AudioError> {
        let (dev, id) = find_device(query, Direction::Input)?;
        let (fmt, ch, sizes) = pick_format(&dev, Direction::Input)?;
        let mut cfg = stream_config(ch, sizes, opts);
        let cap = (opts.ring_seconds * SAMPLE_RATE as f64) as usize;
        let sh = Arc::new(InShared::default());
        let build = |cfg: StreamConfig| -> Result<(cpal::Stream, Consumer<f32>), cpal::Error> {
            let (prod, cons) = RingBuffer::new(cap);
            let s = match fmt {
                SampleFormat::I16 => build_input::<i16>(&dev, cfg, prod, sh.clone())?,
                _ => build_input::<f32>(&dev, cfg, prod, sh.clone())?,
            };
            Ok((s, cons))
        };
        let (stream, cons) = match build(cfg) {
            Ok(x) => x,
            Err(e) if cfg.buffer_size != BufferSize::Default && e.kind() == cpal::ErrorKind::UnsupportedConfig => {
                cfg.buffer_size = BufferSize::Default;
                build(cfg)?
            }
            Err(e) => return Err(open_error(&id, e)),
        };
        stream.play()?;
        let period = match cfg.buffer_size {
            BufferSize::Fixed(p) => format!("period {p}"),
            BufferSize::Default => "default period".into(),
        };
        let format = format!("{fmt} x{ch} @ {SAMPLE_RATE} Hz, {period}");
        Ok(InputStream { _stream: stream, cons, sh, id, format })
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
            errors: l(&s.errors),
            period_frames: l(&s.period),
        }
    }
}

fn open_error(id: &str, e: cpal::Error) -> AudioError {
    let hint = match e.kind() {
        cpal::ErrorKind::DeviceBusy => {
            " (another program, e.g. PipeWire, holds it; try the pipewire device or stop it)"
        }
        cpal::ErrorKind::PermissionDenied => " (is the user in the audio group?)",
        _ => "",
    };
    AudioError::Backend(format!("cannot open {id}: {e}{hint}"))
}

// ---------------------------------------------------------------------------
// Output

/// One output callback: `n` queued samples from absolute index `first`
/// (then silence, up to `frames`) start reaching the DAC at `play_ns`
/// (port time base). Idle callbacks (`n` = 0) are reported too, so the
/// transmitter can time its first sample before it queues anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingEvent {
    /// Index of the first sample (counting every sample ever written).
    pub first: u64,
    /// Queued samples in this callback.
    pub n: u32,
    /// Callback size, frames.
    pub frames: u32,
    /// When the callback ran, ns.
    pub cb_ns: u64,
    /// Predicted DAC time of the callback's first frame, ns.
    pub play_ns: u64,
}

/// Recent [`TimingEvent`]s, answering "when does sample i play".
#[derive(Clone, Debug, Default)]
pub struct TimingTrack {
    events: VecDeque<TimingEvent>,
}

impl TimingTrack {
    const KEEP: usize = 2048;

    /// Record an event.
    pub fn add(&mut self, e: TimingEvent) {
        if self.events.len() == Self::KEEP {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    /// The newest event.
    pub fn last(&self) -> Option<&TimingEvent> {
        self.events.back()
    }

    /// Predicted DAC time of a sample queued now into an empty ring: the
    /// next callback's first frame, if the last callback ran less than half
    /// a period before `now_ns` (so the next one is safely ahead).
    pub fn next_start_ns(&self, now_ns: u64) -> Option<u64> {
        let e = self.events.back()?;
        let period = e.frames as u64 * NS / SAMPLE_RATE as u64;
        (e.frames > 0 && now_ns.saturating_sub(e.cb_ns) < period / 2).then_some(e.play_ns + period)
    }

    /// DAC time of sample `index`, if a recorded callback carried it.
    pub fn play_time_ns(&self, index: u64) -> Option<u64> {
        // Events are in index order; search from the newest.
        self.events
            .iter()
            .rev()
            .find(|e| e.first <= index && index < e.first + e.n as u64)
            .map(|e| e.play_ns + (index - e.first) * NS / SAMPLE_RATE as u64)
    }
}

/// Output counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputStats {
    /// Callbacks that ran out of queued samples before the expected end of
    /// a transmission (the feeder fell behind).
    pub underruns: u64,
    /// Backend-reported xruns.
    pub xruns: u64,
    /// Other backend errors.
    pub errors: u64,
    /// Callback-to-DAC latency of the last callback, ns.
    pub latency_ns: u64,
    /// Last callback size, frames.
    pub period_frames: u64,
}

/// A sink for transmit audio that knows when each sample reaches the DAC.
/// Implemented by [`OutputStream`] and by test doubles with a virtual clock.
pub trait OutputPort {
    /// Queue samples without blocking; returns how many were accepted
    /// (fewer when the ring is full).
    fn write(&mut self, samples: &[f32]) -> Result<usize, AudioError>;
    /// Samples accepted since the port opened (the index of the next one).
    fn written(&self) -> u64;
    /// Samples the device has taken from the ring.
    fn consumed(&self) -> u64;
    /// The ring should not run dry before sample index `end`; if it does,
    /// count an underrun.
    fn expect_until(&mut self, end: u64);
    /// Predicted DAC time of sample `index` (ns, [`OutputPort::now_ns`] base),
    /// once the device has taken it.
    fn play_time_ns(&mut self, index: u64) -> Option<u64>;
    /// Predicted DAC time of a sample written now, when the ring is empty
    /// and a callback has just run (see [`TimingTrack::next_start_ns`]);
    /// `None` otherwise: poll again shortly.
    fn next_start_ns(&mut self) -> Option<u64>;
    /// Callback-to-DAC latency estimate, ns (0 before the first callback).
    fn latency_ns(&self) -> u64;
    /// The port's clock, ns.
    fn now_ns(&self) -> u64;
    /// Sleep on the port's clock.
    fn sleep_ns(&mut self, ns: u64);
    /// Counters.
    fn stats(&self) -> OutputStats;
}

#[derive(Default)]
struct OutShared {
    consumed: AtomicU64,
    expected_end: AtomicU64,
    underruns: AtomicU64,
    xruns: AtomicU64,
    errors: AtomicU64,
    latency_ns: AtomicU64,
    period: AtomicU64,
    dead: AtomicBool,
}

/// A playback stream fed from a ring buffer. Plays silence when the ring is
/// empty, so the device runs continuously and the latency stays known.
pub struct OutputStream {
    _stream: cpal::Stream,
    prod: Producer<f32>,
    timing_rx: Consumer<TimingEvent>,
    track: TimingTrack,
    sh: Arc<OutShared>,
    written: u64,
    origin: Instant,
    /// Device id opened.
    pub id: String,
    /// Format description.
    pub format: String,
}

fn build_output<T: Sample>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    mut cons: Consumer<f32>,
    mut timing_tx: Producer<TimingEvent>,
    sh: Arc<OutShared>,
    origin: Instant,
) -> Result<cpal::Stream, cpal::Error> {
    let ch = cfg.channels as usize;
    let sh_err = sh.clone();
    dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let now = ns_since(origin);
            let ts = info.timestamp();
            let lat = ts.playback.duration_since(ts.callback).as_nanos() as u64;
            sh.latency_ns.store(lat, Ordering::Relaxed);
            let frames = data.len() / ch;
            sh.period.store(frames as u64, Ordering::Relaxed);
            let first = sh.consumed.load(Ordering::Relaxed);
            let n = cons.slots().min(frames);
            if n > 0 {
                let chunk = cons.read_chunk(n).expect("slots checked");
                let (a, b) = chunk.as_slices();
                for (frame, &v) in data.chunks_exact_mut(ch).zip(a.iter().chain(b)) {
                    frame.fill(T::from_f32(v));
                }
                chunk.commit_all();
                sh.consumed.store(first + n as u64, Ordering::Release);
            }
            // A full timing ring (nobody reading while idle) just drops events.
            let _ = timing_tx.push(TimingEvent {
                first,
                n: n as u32,
                frames: frames as u32,
                cb_ns: now,
                play_ns: now + lat,
            });
            data[n * ch..].fill(T::SILENCE);
            if n < frames && first + (n as u64) < sh.expected_end.load(Ordering::Acquire) {
                sh.underruns.fetch_add(1, Ordering::Relaxed);
            }
        },
        move |e: cpal::Error| match e.kind() {
            cpal::ErrorKind::Xrun => {
                sh_err.xruns.fetch_add(1, Ordering::Relaxed);
            }
            cpal::ErrorKind::DeviceNotAvailable => sh_err.dead.store(true, Ordering::Relaxed),
            _ => {
                sh_err.errors.fetch_add(1, Ordering::Relaxed);
            }
        },
        None,
    )
}

impl OutputStream {
    /// Open the playback device matching `query` and start it (playing silence).
    pub fn open(query: &str, opts: &StreamOptions) -> Result<Self, AudioError> {
        let (dev, id) = find_device(query, Direction::Output)?;
        let (fmt, ch, sizes) = pick_format(&dev, Direction::Output)?;
        let mut cfg = stream_config(ch, sizes, opts);
        let cap = (opts.ring_seconds * SAMPLE_RATE as f64) as usize;
        let sh = Arc::new(OutShared::default());
        let origin = Instant::now();
        type Built = (cpal::Stream, Producer<f32>, Consumer<TimingEvent>);
        let build = |cfg: StreamConfig| -> Result<Built, cpal::Error> {
            let (prod, cons) = RingBuffer::new(cap);
            let (ttx, trx) = RingBuffer::new(1024);
            let s = match fmt {
                SampleFormat::I16 => build_output::<i16>(&dev, cfg, cons, ttx, sh.clone(), origin)?,
                _ => build_output::<f32>(&dev, cfg, cons, ttx, sh.clone(), origin)?,
            };
            Ok((s, prod, trx))
        };
        let (stream, prod, timing_rx) = match build(cfg) {
            Ok(x) => x,
            Err(e) if cfg.buffer_size != BufferSize::Default && e.kind() == cpal::ErrorKind::UnsupportedConfig => {
                cfg.buffer_size = BufferSize::Default;
                build(cfg).map_err(|e| open_error(&id, e))?
            }
            Err(e) => return Err(open_error(&id, e)),
        };
        stream.play()?;
        let period = match cfg.buffer_size {
            BufferSize::Fixed(p) => format!("period {p}"),
            BufferSize::Default => "default period".into(),
        };
        let format = format!("{fmt} x{ch} @ {SAMPLE_RATE} Hz, {period}");
        Ok(OutputStream {
            _stream: stream,
            prod,
            timing_rx,
            track: TimingTrack::default(),
            sh,
            written: 0,
            origin,
            id,
            format,
        })
    }

    /// Wait (up to `timeout`) until the device has run a callback, so the
    /// latency is known before a transmission is timed.
    pub fn wait_running(&mut self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while self.sh.period.load(Ordering::Relaxed) == 0 {
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    fn pump_timing(&mut self) {
        while let Ok(e) = self.timing_rx.pop() {
            self.track.add(e);
        }
    }
}

impl OutputPort for OutputStream {
    fn write(&mut self, samples: &[f32]) -> Result<usize, AudioError> {
        if self.sh.dead.load(Ordering::Relaxed) {
            return Err(AudioError::StreamDead);
        }
        self.pump_timing();
        let n = self.prod.slots().min(samples.len());
        if n > 0 {
            let chunk = self.prod.write_chunk_uninit(n).expect("slots checked");
            chunk.fill_from_iter(samples[..n].iter().copied());
            self.written += n as u64;
        }
        Ok(n)
    }

    fn written(&self) -> u64 {
        self.written
    }

    fn consumed(&self) -> u64 {
        self.sh.consumed.load(Ordering::Acquire)
    }

    fn expect_until(&mut self, end: u64) {
        self.sh.expected_end.store(end, Ordering::Release);
    }

    fn play_time_ns(&mut self, index: u64) -> Option<u64> {
        self.pump_timing();
        self.track.play_time_ns(index)
    }

    fn next_start_ns(&mut self) -> Option<u64> {
        self.pump_timing();
        if self.consumed() != self.written {
            return None;
        }
        self.track.next_start_ns(self.now_ns())
    }

    fn latency_ns(&self) -> u64 {
        self.sh.latency_ns.load(Ordering::Relaxed)
    }

    fn now_ns(&self) -> u64 {
        ns_since(self.origin)
    }

    fn sleep_ns(&mut self, ns: u64) {
        std::thread::sleep(Duration::from_nanos(ns));
    }

    fn stats(&self) -> OutputStats {
        let s = &self.sh;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        OutputStats {
            underruns: l(&s.underruns),
            xruns: l(&s.xruns),
            errors: l(&s.errors),
            latency_ns: l(&s.latency_ns),
            period_frames: l(&s.period),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_track_interpolates() {
        let mut t = TimingTrack::default();
        t.add(TimingEvent { first: 0, n: 480, frames: 480, cb_ns: 0, play_ns: 1_000_000 });
        t.add(TimingEvent { first: 480, n: 100, frames: 480, cb_ns: 10_000_000, play_ns: 11_000_000 });
        assert_eq!(t.play_time_ns(0), Some(1_000_000));
        assert_eq!(t.play_time_ns(48), Some(2_000_000));
        assert_eq!(t.play_time_ns(579), Some(11_000_000 + 99 * NS / 48_000));
        assert_eq!(t.play_time_ns(580), None);
        // Just after the last callback: next start is one period after its play time.
        assert_eq!(t.next_start_ns(12_000_000), Some(21_000_000));
        // Too close to the next callback to be sure of making it.
        assert_eq!(t.next_start_ns(16_000_000), None);
    }

    #[test]
    fn sample_conversion() {
        assert_eq!(i16::from_f32(1.0), 32767);
        assert_eq!(i16::from_f32(-1.0), -32768);
        assert_eq!(i16::from_f32(0.5), 16384);
        assert_eq!((-32768i16).to_f32(), -1.0);
    }
}
