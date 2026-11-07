//! The continuous receiver (SPEC 21.3 step 3): the streaming preamble
//! detector runs across chunk boundaries on the processing thread, each
//! detection waits until its whole frame has arrived, and a worker thread
//! decodes it. Live audio and WAV files go through the same code; only the
//! [`ChunkSource`] differs.
//!
//! Where the frame configuration comes from is pluggable
//! ([`ConfigSource`]): today a fixed configuration from the command line
//! ([`FixedConfig`]); once the header carries the frame's fields, a source
//! that decodes the header first and returns the configuration it names.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver as ChanRx, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use cairn_audio::{InputStats, LevelMeter, Levels};
use cairn_phy::detector::{Detection, Detector, DetectorConfig};
use cairn_phy::params::{CP_LONG, FS};
use cairn_phy::{FrameConfig, Receiver, RxOptions, RxResult};

/// Samples per processing chunk (50 ms), live or from a file: the
/// granularity at which a finished frame reaches the decoder.
pub const CHUNK: usize = 2400;
/// Samples kept before a frame's start for the receiver.
const PRE: usize = 64;
/// Samples kept after a frame's nominal end (clock drift, the last window).
const POST: usize = 1200;
/// History kept beyond what pending frames need.
const KEEP: usize = FS as usize;

/// Where a detected frame's configuration comes from.
pub trait ConfigSource: Send {
    /// Samples after the detected start needed before [`ConfigSource::resolve`]
    /// can run (0 for a fixed configuration; preamble plus header for a
    /// header-first source).
    fn lookahead(&self, det: &Detection) -> usize;

    /// The frame's configuration, given the audio from its start onward
    /// (at least `lookahead` samples), or why there is none.
    fn resolve(&mut self, det: &Detection, from_start: &[f64]) -> Result<FrameConfig, String>;
}

/// A configuration fixed on the command line; the prefix comes from the
/// detector unless forced.
pub struct FixedConfig {
    width: usize,
    mcs: u8,
    n_blocks: usize,
    usable: Option<Vec<usize>>,
    force_cp: Option<usize>,
}

impl FixedConfig {
    /// Validate and build. `silenced` are tones masked in data symbols.
    pub fn new(
        width: usize,
        mcs: u8,
        n_blocks: usize,
        silenced: &[usize],
        force_cp: Option<usize>,
    ) -> Result<Self, String> {
        let usable =
            if silenced.is_empty() { None } else { Some((0..width).filter(|k| !silenced.contains(k)).collect()) };
        let f = FixedConfig { width, mcs, n_blocks, usable, force_cp };
        f.build(force_cp.unwrap_or(cairn_phy::params::CP_SHORT))?;
        Ok(f)
    }

    fn build(&self, cp: usize) -> Result<FrameConfig, String> {
        FrameConfig::new(self.width, self.mcs, self.n_blocks)
            .and_then(|c| c.with_cp(cp))
            .and_then(|c| c.with_mask(self.usable.clone()))
            .map_err(|e| e.to_string())
    }
}

impl ConfigSource for FixedConfig {
    fn lookahead(&self, _det: &Detection) -> usize {
        0
    }
    fn resolve(&mut self, det: &Detection, _from_start: &[f64]) -> Result<FrameConfig, String> {
        self.build(self.force_cp.unwrap_or(det.cp))
    }
}

/// A frame ready to decode.
pub struct Job {
    /// The detection.
    pub det: Detection,
    /// Its configuration.
    pub cfg: FrameConfig,
    /// Audio from `det.start - offset`.
    pub samples: Vec<f64>,
    /// Index of the frame start in `samples`.
    pub offset: usize,
    /// When the last sample arrived.
    pub ready_at: Instant,
}

/// A decoded frame.
pub struct FrameReport {
    /// The detection.
    pub det: Detection,
    /// The configuration used.
    pub cfg: FrameConfig,
    /// The receiver's result.
    pub rx: RxResult,
    /// Decode time, ms.
    pub decode_ms: f64,
    /// From the frame's last sample arriving to the result, ms.
    pub latency_ms: f64,
}

impl FrameReport {
    /// Blocks whose CRC passed.
    pub fn blocks_ok(&self) -> usize {
        self.rx.block_ok.iter().filter(|&&b| b).count()
    }

    /// Sample clock offset, ppm, as `cairn-bench rx` reports it.
    pub fn clock_ppm(&self) -> f64 {
        -self.rx.drift / self.cfg.stride() as f64 * 1e6
    }

    /// The protocol's SNR (7 dB-equivalent channel SNR, ADR 0006), dB.
    pub fn snr_db(&self) -> f64 {
        self.rx.snr_7db_equivalent(&self.cfg)
    }

    /// One line for the console.
    pub fn line(&self) -> String {
        format!(
            "frame  {} prefix  MCS {} x{}  header {}  blocks {}/{}  SNR {:5.1} dB  clock {:+6.1} ppm  rho {:.2}  decode {:.0} ms",
            if self.det.cp == CP_LONG { "long " } else { "short" },
            self.cfg.mcs,
            self.cfg.width,
            if self.rx.header_ok { "OK  " } else { "FAIL" },
            self.blocks_ok(),
            self.cfg.n_blocks,
            self.snr_db(),
            self.clock_ppm(),
            self.det.quality,
            self.decode_ms,
        )
    }
}

enum Phase {
    Config,
    Frame(FrameConfig),
}

struct Pending {
    det: Detection,
    phase: Phase,
}

/// Something the listener has to say.
pub enum Event {
    /// A frame went to the decoder.
    Queued(Detection),
    /// A detection whose configuration could not be resolved.
    Unresolved(Detection, String),
}

/// The processing-thread half: detector, history and pending frames.
pub struct Listener {
    detector: Detector,
    hist: Vec<f64>,
    hist_start: usize,
    pending: VecDeque<Pending>,
    config: Box<dyn ConfigSource>,
    jobs: Sender<Job>,
    /// Detections so far.
    pub detections: u64,
}

impl Listener {
    /// A listener sending decode jobs to `jobs`.
    pub fn new(config: Box<dyn ConfigSource>, detector: DetectorConfig, jobs: Sender<Job>) -> Self {
        Listener {
            detector: Detector::new(detector),
            hist: Vec::new(),
            hist_start: 0,
            pending: VecDeque::new(),
            config,
            jobs,
            detections: 0,
        }
    }

    fn end(&self) -> usize {
        self.hist_start + self.hist.len()
    }

    /// Samples taken so far.
    pub fn position(&self) -> usize {
        self.end()
    }

    fn slice_from(&self, start: usize, len: usize) -> (Vec<f64>, usize) {
        // Audio from start - PRE (zero-padded before the first sample).
        let lo = start.saturating_sub(PRE);
        let pad = PRE - (start - lo);
        let mut v = vec![0.0; pad];
        let a = lo - self.hist_start;
        let b = (start + len - self.hist_start).min(self.hist.len());
        v.extend_from_slice(&self.hist[a..b]);
        (v, PRE)
    }

    /// Feed audio; returns what happened.
    pub fn push(&mut self, x: &[f64]) -> Vec<Event> {
        self.hist.extend_from_slice(x);
        let dets = self.detector.process(x);
        self.take(dets)
    }

    /// End of input: flush the detector and complete pending frames with silence.
    pub fn finish(&mut self) -> Vec<Event> {
        let dets = self.detector.flush();
        let mut ev = self.take(dets);
        // A resolved configuration may need more audio than the lookahead, hence rounds.
        for _ in 0..3 {
            let Some(need) = self.pending.iter().map(|p| self.needed_end(p)).max() else { break };
            if need > self.end() {
                let more = need - self.end();
                self.hist.extend(std::iter::repeat_n(0.0, more));
            }
            ev.extend(self.advance());
        }
        ev
    }

    fn needed_end(&self, p: &Pending) -> usize {
        match &p.phase {
            Phase::Config => p.det.start + self.config.lookahead(&p.det),
            Phase::Frame(c) => p.det.start + c.n_samples() + POST,
        }
    }

    fn take(&mut self, dets: Vec<Detection>) -> Vec<Event> {
        for d in dets {
            self.detections += 1;
            self.pending.push_back(Pending { det: d, phase: Phase::Config });
        }
        let ev = self.advance();
        self.trim();
        ev
    }

    fn advance(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        let end = self.end();
        let mut keep = VecDeque::new();
        while let Some(mut p) = self.pending.pop_front() {
            if let Phase::Config = p.phase {
                let need = self.config.lookahead(&p.det);
                if end < p.det.start + need {
                    keep.push_back(p);
                    continue;
                }
                let (seg, off) = self.slice_from(p.det.start, need);
                match self.config.resolve(&p.det, &seg[off..]) {
                    Ok(cfg) => p.phase = Phase::Frame(cfg),
                    Err(e) => {
                        ev.push(Event::Unresolved(p.det.clone(), e));
                        continue;
                    }
                }
            }
            let Phase::Frame(cfg) = &p.phase else { unreachable!() };
            let len = cfg.n_samples() + POST;
            if end < p.det.start + len {
                keep.push_back(p);
                continue;
            }
            let (samples, offset) = self.slice_from(p.det.start, len);
            let Phase::Frame(cfg) = p.phase else { unreachable!() };
            ev.push(Event::Queued(p.det.clone()));
            // A closed decoder only happens at shutdown; dropping the job is fine then.
            let _ = self.jobs.send(Job { det: p.det, cfg, samples, offset, ready_at: Instant::now() });
        }
        self.pending = keep;
        ev
    }

    fn trim(&mut self) {
        let mut keep_from = self.end().saturating_sub(KEEP);
        for p in &self.pending {
            keep_from = keep_from.min(p.det.start.saturating_sub(PRE));
        }
        if keep_from > self.hist_start + KEEP {
            self.hist.drain(..keep_from - self.hist_start);
            self.hist_start = keep_from;
        }
    }
}

/// Start the decoder thread: one [`Receiver`] decoding jobs in order.
pub fn spawn_decoder(jobs: ChanRx<Job>, reports: Sender<FrameReport>, opts: RxOptions) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("cairn-decode".into())
        .spawn(move || {
            let mut rx = Receiver::new();
            for job in jobs {
                let t0 = Instant::now();
                let res = rx.receive(&job.samples, &job.cfg, job.offset, &opts);
                let decode_ms = t0.elapsed().as_secs_f64() * 1e3;
                let latency_ms = job.ready_at.elapsed().as_secs_f64() * 1e3;
                let report = FrameReport { det: job.det, cfg: job.cfg, rx: res, decode_ms, latency_ms };
                if reports.send(report).is_err() {
                    break;
                }
            }
        })
        .expect("spawn decoder thread")
}

/// Where audio comes from.
pub trait ChunkSource {
    /// Fill `out` with the next [`CHUNK`] samples (fewer only at the end);
    /// `None` at the end of input or when stopped.
    fn next_chunk(&mut self, out: &mut Vec<f64>) -> Option<()>;
    /// Capture counters, for live input.
    fn input_stats(&self) -> Option<InputStats> {
        None
    }
    /// Samples waiting to be processed (live input), for the queue depth.
    fn backlog(&self) -> usize {
        0
    }
}

/// A WAV file (or any buffer) in [`CHUNK`]s, as fast as it can be processed.
pub struct BufferSource {
    x: Vec<f64>,
    pos: usize,
}

impl BufferSource {
    /// Serve `x`.
    pub fn new(x: Vec<f64>) -> Self {
        BufferSource { x, pos: 0 }
    }
}

impl ChunkSource for BufferSource {
    fn next_chunk(&mut self, out: &mut Vec<f64>) -> Option<()> {
        out.clear();
        if self.pos >= self.x.len() {
            return None;
        }
        let end = (self.pos + CHUNK).min(self.x.len());
        out.extend_from_slice(&self.x[self.pos..end]);
        self.pos = end;
        Some(())
    }
}

/// Periodic status.
pub struct Stats {
    /// Stream position, samples.
    pub position: usize,
    /// Input levels over the interval.
    pub levels: Levels,
    /// Capture counters (live).
    pub input: Option<InputStats>,
    /// Samples waiting (live).
    pub backlog: usize,
    /// Detections so far.
    pub detections: u64,
    /// Frames decoded so far.
    pub frames: u64,
    /// Frames whose header decoded.
    pub headers_ok: u64,
}

/// Everything the run loop reports.
pub enum Output {
    /// A decoded frame.
    Frame(Box<FrameReport>),
    /// A detection without a configuration.
    Unresolved(Detection, String),
    /// Periodic status.
    Stats(Stats),
}

/// Listener settings.
pub struct ListenOptions {
    /// Detector settings.
    pub detector: DetectorConfig,
    /// Receiver settings.
    pub rx: RxOptions,
    /// Stats interval, samples (0: never).
    pub stats_every: usize,
}

/// Totals at the end of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Samples processed.
    pub samples: usize,
    /// Detections.
    pub detections: u64,
    /// Frames decoded.
    pub frames: u64,
    /// Headers decoded.
    pub headers_ok: u64,
    /// Data blocks decoded / sent.
    pub blocks_ok: u64,
    /// Data blocks in decoded frames.
    pub blocks: u64,
    /// Frames with the header and every block decoded.
    pub complete: u64,
}

/// Run until the source ends: detect, decode on the worker, report.
pub fn run(
    source: &mut dyn ChunkSource,
    config: Box<dyn ConfigSource>,
    opts: ListenOptions,
    out: &mut dyn FnMut(Output),
) -> Totals {
    let (job_tx, job_rx) = channel();
    let (rep_tx, rep_rx) = channel();
    let worker = spawn_decoder(job_rx, rep_tx, opts.rx.clone());
    let mut listener = Listener::new(config, opts.detector.clone(), job_tx);
    let mut meter = LevelMeter::new();
    let mut totals = Totals::default();
    let mut chunk = Vec::with_capacity(CHUNK);
    let mut next_stats = opts.stats_every;

    let deliver = |r: FrameReport, totals: &mut Totals, out: &mut dyn FnMut(Output)| {
        totals.frames += 1;
        totals.headers_ok += r.rx.header_ok as u64;
        totals.blocks_ok += r.blocks_ok() as u64;
        totals.blocks += r.cfg.n_blocks as u64;
        totals.complete += (r.rx.header_ok && r.blocks_ok() == r.cfg.n_blocks) as u64;
        out(Output::Frame(Box::new(r)));
    };

    while source.next_chunk(&mut chunk).is_some() {
        meter.push_f64(&chunk);
        for e in listener.push(&chunk) {
            if let Event::Unresolved(d, why) = e {
                out(Output::Unresolved(d, why));
            }
        }
        while let Ok(r) = rep_rx.try_recv() {
            deliver(r, &mut totals, out);
        }
        if opts.stats_every > 0 && listener.position() >= next_stats {
            next_stats += opts.stats_every;
            out(Output::Stats(Stats {
                position: listener.position(),
                levels: meter.take(),
                input: source.input_stats(),
                backlog: source.backlog(),
                detections: listener.detections,
                frames: totals.frames,
                headers_ok: totals.headers_ok,
            }));
        }
    }
    totals.samples = listener.position();
    for e in listener.finish() {
        if let Event::Unresolved(d, why) = e {
            out(Output::Unresolved(d, why));
        }
    }
    totals.detections = listener.detections;
    drop(listener); // closes the job channel
    worker.join().expect("decoder thread");
    for r in rep_rx.try_iter() {
        deliver(r, &mut totals, out);
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{FrameSpec, build_frame};
    use cairn_phy::Transmitter;
    use cairn_phy::params::CP_SHORT;

    fn signal(specs: &[FrameSpec], lead: usize, gap: usize) -> Vec<f64> {
        let mut tx = Transmitter::new();
        let mut x = vec![0.0; lead];
        for (i, s) in specs.iter().enumerate() {
            let f = build_frame(&mut tx, &s.config().unwrap(), 1000 + i as u64);
            x.extend(f.output());
            x.extend(std::iter::repeat_n(0.0, gap));
        }
        // A little noise so the detector's normalisation sees a floor.
        let mut rng = cairn_phy::channel::Rng::new(5);
        for v in &mut x {
            *v += 1e-4 * rng.normal();
        }
        x
    }

    fn decode_all(x: Vec<f64>, cfg: FixedConfig) -> (Vec<FrameReport>, Totals) {
        let mut src = BufferSource::new(x);
        let mut frames = Vec::new();
        let opts = ListenOptions { detector: DetectorConfig::default(), rx: RxOptions::default(), stats_every: 0 };
        let t = run(&mut src, Box::new(cfg), opts, &mut |o| {
            if let Output::Frame(f) = o {
                frames.push(*f)
            }
        });
        (frames, t)
    }

    #[test]
    fn decodes_frames_across_chunk_boundaries() {
        let spec = FrameSpec { mcs: 2, width: 24, cp: CP_SHORT, silenced: vec![], blocks: Some(3), frame_s: None };
        // Odd lead and gap so frames straddle chunks at different phases;
        // the last frame ends 100 samples before the end of the input.
        let x = signal(&[spec.clone(), spec.clone(), spec.clone()], 12_345, 7_777);
        let x = x[..x.len() - 7_677].to_vec();
        let (frames, t) = decode_all(x, FixedConfig::new(24, 2, 3, &[], None).unwrap());
        assert_eq!(frames.len(), 3, "every frame decoded");
        for f in &frames {
            assert!(f.rx.header_ok);
            assert_eq!(f.blocks_ok(), 3);
            assert!(f.snr_db() > 30.0, "{}", f.snr_db());
        }
        assert_eq!(t.detections, 3);
        assert_eq!(t.blocks_ok, 9);
    }

    #[test]
    fn silence_and_noise_give_nothing() {
        let mut rng = cairn_phy::channel::Rng::new(9);
        let x: Vec<f64> = (0..10 * FS as usize).map(|_| 0.005 * rng.normal()).collect();
        let (frames, t) = decode_all(x, FixedConfig::new(24, 2, 3, &[], None).unwrap());
        assert!(frames.is_empty());
        assert_eq!(t.detections, 0);
    }
}
