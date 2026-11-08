//! The continuous receiver (SPEC 21.3 step 3): the incremental preamble
//! detector runs on the processing thread as each 10 ms chunk arrives, each
//! detection's configuration is resolved as soon as enough of the frame has
//! arrived, the frame goes to the decoder the moment its last sample is in,
//! and a worker thread decodes it (data blocks on several threads). Live
//! audio and WAV files go through the same code; only the [`ChunkSource`]
//! differs.
//!
//! Where the frame configuration comes from is pluggable ([`ConfigSource`]):
//! [`HeaderFirst`] decodes the header once the preamble and header are in
//! and takes the frame's geometry from it (SPEC 8.2), with the link state a
//! [`LinkSource`] gives for that header for session frames (width, probe
//! extent, the mask the header's Mask ID names; ADR 0011) and, for a
//! broadcast wider than 24 tones, the width from its probe (ADR 0007);
//! [`FixedConfig`] takes everything from the command line.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver as ChanRx, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cairn_audio::{InputStats, LevelMeter, Levels};
use cairn_phy::detector::{Detection, Detector, DetectorConfig};
use cairn_phy::fields::Header;
use cairn_phy::params::{CP_LONG, FS, PREAMBLE_SYMBOLS};
use cairn_phy::rx::{extent_samples, frame_link};
use cairn_phy::{ConfigError, FrameConfig, LinkSource, LinkState, ProbeExtent, Receiver, RxOptions, RxResult};

use crate::headers;

/// Samples per processing chunk (10 ms, the sound card's period), live or
/// from a file: the granularity at which a finished frame reaches the decoder.
pub const CHUNK: usize = 480;
/// Samples kept before a frame's start for the receiver.
const PRE: usize = 64;
/// History kept beyond what pending frames need.
const KEEP: usize = FS as usize;

/// Samples after a frame's nominal end the decoder waits for: the receiver's
/// last window ends a quarter prefix before the end, and a sample-clock offset
/// of up to 100 ppm (SPEC 6.1's grid) stretches the frame by 1e-4 of its length.
pub fn post_samples(n_samples: usize) -> usize {
    n_samples.div_ceil(10_000) + 8
}

/// How to decode a resolved frame.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The configuration, which fixes the frame's length.
    pub cfg: FrameConfig,
    /// The decoded header (header-first), else None (decode `cfg` as given).
    pub header: Option<Header>,
    /// The link state for session frames (None: no session state; the
    /// frame took its width from its probe).
    pub link: Option<LinkState>,
    /// For a broadcast with a probe: its extent (ADR 0007).
    pub extent: Option<ProbeExtent>,
}

/// What a [`ConfigSource`] says about a detection.
pub enum Resolution {
    /// The frame's plan.
    Ready(Box<Plan>),
    /// Call again once this many samples from the start have arrived.
    More(usize),
    /// No frame can be decoded here, and why.
    Fail(String),
}

/// Where a detected frame's configuration comes from.
pub trait ConfigSource: Send {
    /// Samples after the detected start needed before the first
    /// [`ConfigSource::resolve`] (0 for a fixed configuration).
    fn lookahead(&self, det: &Detection) -> usize;

    /// Resolve with the audio from the frame's start onward (at least the
    /// samples last asked for).
    fn resolve(&mut self, det: &Detection, from_start: &[f64]) -> Resolution;
}

/// A configuration fixed on the command line (`listen --fixed`); the prefix
/// comes from the detector unless forced.
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
        let usable = headers::usable_from_silenced(width, silenced);
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
    fn resolve(&mut self, det: &Detection, _from_start: &[f64]) -> Resolution {
        match self.build(self.force_cp.unwrap_or(det.cp)) {
            Ok(cfg) => {
                let link =
                    LinkState { width: cfg.width, usable: cfg.usable.clone(), cp: Some(cfg.cp), ..Default::default() };
                Resolution::Ready(Box::new(Plan { cfg, header: None, link: Some(link), extent: None }))
            }
            Err(e) => Resolution::Fail(e),
        }
    }
}

/// Header-first configuration (SPEC 8.2): decode the header once the
/// preamble and header have arrived and ask the [`LinkSource`] for that
/// header's link state; a session frame (flag bit 3) is decoded at its width
/// and probe extent with the mask its Mask ID names (ADR 0011; a mask ID the
/// link state does not hold leaves the frame undecoded); a frame outside a
/// session, or one the source has no state for, at the width of its probe
/// (ADR 0007), measured once the data symbols every width has are in, or at
/// 24 tones without a probe.
pub struct HeaderFirst {
    link: Box<dyn LinkSource + Send>,
    rx: Receiver,
    opts: RxOptions,
    /// Headers decoded while waiting for a probe's data symbols, by start.
    waiting: HashMap<usize, Header>,
}

impl HeaderFirst {
    /// A source whose session frames take the link state `link` gives for
    /// their header: a fixed [`LinkState`] (whose `cp` forces the prefix;
    /// None takes the detector's) or a closure `Fn(&Header) -> Option<LinkState>`.
    pub fn new<L: LinkSource + Send + 'static>(link: L, opts: RxOptions) -> Self {
        HeaderFirst {
            link: Box::new(link),
            rx: Receiver::new(),
            opts: RxOptions { rebuild_snr: false, threads: 1, ..opts },
            waiting: HashMap::new(),
        }
    }
}

/// Samples from a frame's start through its header, with a little margin.
pub fn header_samples(cp: usize) -> usize {
    (PREAMBLE_SYMBOLS + cairn_phy::params::header_symbols()) * (cairn_phy::params::N_USEFUL + cp) + 64
}

impl ConfigSource for HeaderFirst {
    fn lookahead(&self, det: &Detection) -> usize {
        header_samples(self.link.forced_cp().unwrap_or(det.cp))
    }

    fn resolve(&mut self, det: &Detection, from_start: &[f64]) -> Resolution {
        let cp = self.link.forced_cp().unwrap_or(det.cp);
        let header = match self.waiting.remove(&det.start) {
            Some(h) => h,
            None => {
                let head = self.rx.receive_header(from_start, 0, cp, &self.opts);
                match head.header {
                    Some(h) => h,
                    None => {
                        return Resolution::Fail(
                            head.header_error.map_or("header did not decode".into(), |e| e.to_string()),
                        );
                    }
                }
            }
        };
        let link = self.link.link_for(&header);
        let extent = if (link.is_none() || !header.session) && header.probe {
            let need = extent_samples(&header, cp) + 64;
            if from_start.len() < need {
                self.waiting.insert(det.start, header);
                return Resolution::More(need);
            }
            Some(self.rx.probe_extent(from_start, 0, cp, &header, &self.opts))
        } else {
            None
        };
        match FrameConfig::from_header(&header, &frame_link(&header, link.as_ref(), extent.as_ref()), cp) {
            Ok(cfg) => Resolution::Ready(Box::new(Plan { cfg, header: Some(header), link, extent })),
            Err(ConfigError::UnknownMask(id)) => Resolution::Fail(format!(
                "{} > {} seq {}: mask ID {id} is not held; control and data blocks count as failed (ADR 0011)",
                header.source, header.destination, header.sequence
            )),
            Err(e) => Resolution::Fail(format!("header describes no decodable frame: {e}")),
        }
    }
}

/// A frame ready to decode.
pub struct Job {
    /// The detection.
    pub det: Detection,
    /// How to decode it.
    pub plan: Plan,
    /// Audio from `det.start - offset`.
    pub samples: Vec<f64>,
    /// Index of the frame start in `samples`.
    pub offset: usize,
    /// When the last sample reached the processing thread.
    pub ready_at: Instant,
}

/// A decoded frame.
pub struct FrameReport {
    /// The detection.
    pub det: Detection,
    /// The configuration decoded (after any width retry).
    pub cfg: FrameConfig,
    /// The receiver's result.
    pub rx: RxResult,
    /// Decode time, ms.
    pub decode_ms: f64,
    /// From the frame's last sample reaching the processing thread to the result, ms.
    pub latency_ms: f64,
    /// When the result was ready.
    pub done_at: Instant,
    /// From the frame's last sample at the input (where the source knows its
    /// time) to the result, ms: the receive turnaround of SPEC 7.3.
    pub turnaround_ms: Option<f64>,
}

impl FrameReport {
    /// Blocks whose CRC passed.
    pub fn blocks_ok(&self) -> usize {
        self.rx.block_ok.iter().filter(|&&b| b).count()
    }

    /// The frame decoded completely: header, every control and data block.
    pub fn complete(&self) -> bool {
        self.rx.header_ok && self.blocks_ok() == self.cfg.n_blocks && self.rx.control_ok.iter().all(|&b| b)
    }

    /// Sample clock offset, ppm, as `cairn-bench rx` reports it.
    pub fn clock_ppm(&self) -> f64 {
        -self.rx.drift / self.cfg.stride() as f64 * 1e6
    }

    /// The protocol's SNR (7 dB-equivalent channel SNR, ADR 0006), dB.
    pub fn snr_db(&self) -> f64 {
        self.rx.snr_7db_equivalent(&self.cfg).min(63.75)
    }

    /// One line for the console: source and destination, frame type,
    /// sequence, rung and width, blocks, SNR, clock, detector rho, timing.
    pub fn line(&self) -> String {
        let who = match &self.rx.header {
            Some(h) => format!(
                "{} > {}  {}{}  seq {}",
                h.source,
                h.destination,
                headers::type_name(h.frame_type),
                headers::flag_text(h),
                h.sequence
            ),
            None => format!("header {}", if self.rx.header_ok { "OK (fixed)" } else { "FAIL" }),
        };
        let width = match &self.rx.probe_extent {
            Some(e) if e.width != self.cfg.width => format!("x{} (probe said {})", self.cfg.width, e.width),
            Some(_) => format!("x{} from probe", self.cfg.width),
            None => format!("x{}", self.cfg.width),
        };
        let turnaround = self.turnaround_ms.map(|t| format!("  turnaround {t:.0} ms")).unwrap_or_default();
        format!(
            "{who}  MCS {} {width} {} prefix  blocks {}/{}  ctrl {}/{}  SNR {:5.1} dB  clock {:+6.1} ppm  rho {:.2}  decode {:.0} ms{turnaround}",
            self.cfg.mcs,
            if self.det.cp == CP_LONG { "long" } else { "short" },
            self.blocks_ok(),
            self.cfg.n_blocks,
            self.rx.control_ok.iter().filter(|&&b| b).count(),
            self.cfg.n_control,
            self.snr_db(),
            self.clock_ppm(),
            self.det.quality,
            self.decode_ms,
        )
    }

    /// One line per control block: its summary, or why it is missing.
    pub fn control_lines(&self) -> Vec<String> {
        self.rx
            .controls
            .iter()
            .zip(&self.rx.control_ok)
            .enumerate()
            .map(|(i, (c, &ok))| match c {
                Some(c) => format!("ctrl {}: {}", i + 1, headers::control_summary(c)),
                None if ok => format!("ctrl {}: malformed fields", i + 1),
                None => format!("ctrl {}: did not decode", i + 1),
            })
            .collect()
    }
}

enum Phase {
    /// Waiting for this many samples from the start, then resolve.
    Config(usize),
    Frame(Box<Plan>),
}

struct Pending {
    det: Detection,
    phase: Phase,
}

/// Something the listener has to say.
pub enum Event {
    /// A preamble was detected (its start is final).
    Detected(Detection),
    /// A detection's configuration is known: the frame is `n_samples` long
    /// from its start.
    Resolved(Detection, usize),
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

    /// The detector's early confirmation of a preamble (gated plateau and
    /// cross-correlation over threshold), once, as its 48 kHz index: the
    /// preamble correlator of SPEC 7.2's carrier detect, ahead of the final
    /// detection.
    pub fn take_early(&mut self) -> Option<usize> {
        self.detector.take_early()
    }

    /// Frames detected and not yet sent to the decoder or given up.
    pub fn pending(&self) -> usize {
        self.pending.len()
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
        // A resolution may ask for more audio, hence rounds.
        for _ in 0..4 {
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
            Phase::Config(need) => p.det.start + need,
            Phase::Frame(plan) => p.det.start + plan.cfg.n_samples() + post_samples(plan.cfg.n_samples()),
        }
    }

    fn take(&mut self, dets: Vec<Detection>) -> Vec<Event> {
        let mut ev = Vec::new();
        for d in dets {
            self.detections += 1;
            let need = self.config.lookahead(&d);
            ev.push(Event::Detected(d.clone()));
            self.pending.push_back(Pending { det: d, phase: Phase::Config(need) });
        }
        ev.extend(self.advance());
        self.trim();
        ev
    }

    fn advance(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        let end = self.end();
        let mut keep = VecDeque::new();
        'next: while let Some(mut p) = self.pending.pop_front() {
            while let Phase::Config(need) = p.phase {
                if end < p.det.start + need {
                    keep.push_back(p);
                    continue 'next;
                }
                let (seg, off) = self.slice_from(p.det.start, end - p.det.start);
                p.phase = match self.config.resolve(&p.det, &seg[off..]) {
                    Resolution::Ready(plan) => {
                        ev.push(Event::Resolved(p.det.clone(), plan.cfg.n_samples()));
                        Phase::Frame(plan)
                    }
                    Resolution::More(n) => Phase::Config(n.max(need + 1)),
                    Resolution::Fail(e) => {
                        ev.push(Event::Unresolved(p.det.clone(), e));
                        continue 'next;
                    }
                };
            }
            let Phase::Frame(plan) = &p.phase else { unreachable!() };
            let n = plan.cfg.n_samples();
            let len = n + post_samples(n);
            if end < p.det.start + len {
                keep.push_back(p);
                continue;
            }
            let (samples, offset) = self.slice_from(p.det.start, len);
            let Phase::Frame(plan) = p.phase else { unreachable!() };
            ev.push(Event::Queued(p.det.clone()));
            // A closed decoder only happens at shutdown; dropping the job is fine then.
            let _ = self.jobs.send(Job { det: p.det, plan: *plan, samples, offset, ready_at: Instant::now() });
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

/// Decode one job (the worker's work).
pub fn decode_job(rx: &mut Receiver, job: &Job, opts: &RxOptions) -> RxResult {
    let plan = &job.plan;
    match &plan.header {
        Some(h) => rx
            .receive_frame(&job.samples, h, plan.link.as_ref(), plan.extent.clone(), job.offset, plan.cfg.cp, opts)
            .unwrap_or_else(|_| rx.receive(&job.samples, &plan.cfg, job.offset, opts)),
        None => rx.receive(&job.samples, &plan.cfg, job.offset, opts),
    }
}

/// Start the decoder thread: one [`Receiver`] decoding jobs in order. With
/// `boost`, it asks the scheduler to run it at full speed ([`crate::boost`]).
pub fn spawn_decoder(jobs: ChanRx<Job>, reports: Sender<FrameReport>, opts: RxOptions, boost: bool) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("cairn-decode".into())
        .spawn(move || {
            if boost && let Err(e) = crate::boost::boost_current_thread() {
                eprintln!("note: decoder runs without a utilisation clamp ({e}); turnaround may suffer");
            }
            let mut rx = Receiver::new();
            for job in jobs {
                let t0 = Instant::now();
                let res = decode_job(&mut rx, &job, &opts);
                let done_at = Instant::now();
                let decode_ms = (done_at - t0).as_secs_f64() * 1e3;
                let latency_ms = (done_at - job.ready_at).as_secs_f64() * 1e3;
                let cfg = res.config.clone().unwrap_or(job.plan.cfg);
                let report =
                    FrameReport { det: job.det, cfg, rx: res, decode_ms, latency_ms, done_at, turnaround_ms: None };
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
    /// When sample `index` was at the input, if the source knows.
    fn sample_time(&self, _index: usize) -> Option<Instant> {
        None
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

/// A buffer played at real-time pace, as a sound card delivers it: each
/// [`CHUNK`] becomes available when its last sample would have arrived, so
/// the time of every sample is known and the receive turnaround (last sample
/// of a frame to its decoded result, SPEC 7.3) can be measured.
pub struct PacedSource {
    x: Vec<f64>,
    pos: usize,
    t0: Option<Instant>,
}

impl PacedSource {
    /// Play `x` from the first call on.
    pub fn new(x: Vec<f64>) -> Self {
        PacedSource { x, pos: 0, t0: None }
    }
}

impl ChunkSource for PacedSource {
    fn next_chunk(&mut self, out: &mut Vec<f64>) -> Option<()> {
        out.clear();
        if self.pos >= self.x.len() {
            return None;
        }
        let t0 = *self.t0.get_or_insert_with(Instant::now);
        let end = (self.pos + CHUNK).min(self.x.len());
        let due = t0 + Duration::from_secs_f64(end as f64 / FS as f64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        out.extend_from_slice(&self.x[self.pos..end]);
        self.pos = end;
        Some(())
    }
    fn sample_time(&self, index: usize) -> Option<Instant> {
        self.t0.map(|t0| t0 + Duration::from_secs_f64(index as f64 / FS as f64))
    }
    fn backlog(&self) -> usize {
        0
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
    /// Run the decoder at full speed when it wakes ([`crate::boost`]).
    pub boost: bool,
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
    let worker = spawn_decoder(job_rx, rep_tx, opts.rx.clone(), opts.boost);
    let mut listener = Listener::new(config, opts.detector.clone(), job_tx);
    let mut meter = LevelMeter::new();
    let mut totals = Totals::default();
    let mut chunk = Vec::with_capacity(CHUNK);
    let mut next_stats = opts.stats_every;

    let deliver = |mut r: FrameReport, src: &dyn ChunkSource, totals: &mut Totals, out: &mut dyn FnMut(Output)| {
        let last = r.det.start + r.cfg.n_samples();
        r.turnaround_ms = src.sample_time(last).map(|t| r.done_at.saturating_duration_since(t).as_secs_f64() * 1e3);
        totals.frames += 1;
        totals.headers_ok += r.rx.header_ok as u64;
        totals.blocks_ok += r.blocks_ok() as u64;
        totals.blocks += r.cfg.n_blocks as u64;
        totals.complete += r.complete() as u64;
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
            deliver(r, source, &mut totals, out);
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
        deliver(r, source, &mut totals, out);
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{FrameSpec, build_frame};
    use crate::headers::{HeaderSpec, build_header_frame};
    use cairn_phy::fields::{Address, frame_type};
    use cairn_phy::params::CP_SHORT;
    use cairn_phy::{Transmitter, TxFrame};

    fn noisy(frames: &[TxFrame], lead: usize, gap: usize) -> Vec<f64> {
        let mut x = vec![0.0; lead];
        for f in frames {
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

    fn decode_all(x: Vec<f64>, cfg: Box<dyn ConfigSource>) -> (Vec<FrameReport>, Totals) {
        let mut src = BufferSource::new(x);
        let mut frames = Vec::new();
        let opts = ListenOptions {
            detector: DetectorConfig::default(),
            rx: RxOptions { threads: 2, ..Default::default() },
            stats_every: 0,
            boost: false,
        };
        let t = run(&mut src, cfg, opts, &mut |o| {
            if let Output::Frame(f) = o {
                frames.push(*f)
            }
        });
        (frames, t)
    }

    #[test]
    fn decodes_fixed_frames_across_chunk_boundaries() {
        let spec = FrameSpec { mcs: 2, width: 24, cp: CP_SHORT, silenced: vec![], blocks: Some(3), frame_s: None };
        let mut tx = Transmitter::new();
        let frames: Vec<TxFrame> =
            (0..3).map(|i| build_frame(&mut tx, &spec.config().unwrap(), 1000 + i as u64)).collect();
        // Odd lead and gap so frames straddle chunks at different phases;
        // the last frame ends 100 samples before the end of the input.
        let x = noisy(&frames, 12_345, 7_777);
        let x = x[..x.len() - 7_677].to_vec();
        let (frames, t) = decode_all(x, Box::new(FixedConfig::new(24, 2, 3, &[], None).unwrap()));
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
    fn decodes_real_headers_first() {
        // Broadcasts at 24 and 40 tones (width from the probe), a BEACON with
        // the ID block, and a session DATA frame at the link's 30 tones with a mask.
        let me = Address::new("N0CALL", 3).unwrap();
        let peer = Address::new("N1CALL", 0).unwrap();
        let usable: Vec<usize> = (0..30).filter(|&k| k != 11).collect();
        let link = LinkState { width: 30, usable: Some(usable.clone()), ..Default::default() };
        let mut tx = Transmitter::new();
        let mut specs = vec![
            HeaderSpec::broadcast(me.clone(), frame_type::UI, 0, 24, 2, 3),
            HeaderSpec::broadcast(me.clone(), frame_type::UI, 1, 40, 1, 4),
            HeaderSpec::broadcast(me.clone(), frame_type::BEACON, 2, 24, 0, 0),
        ];
        specs[1].id = true;
        let mut data = HeaderSpec::broadcast(me.clone(), frame_type::DATA, 100, 30, 3, 5);
        data.destination = peer;
        data.usable = Some(usable);
        specs.push(data);
        let frames: Vec<TxFrame> =
            specs.iter().enumerate().map(|(i, s)| build_header_frame(&mut tx, s, 7 + i as u64).unwrap().0).collect();
        let x = noisy(&frames, 9_999, 5_321);
        let (got, t) = decode_all(x, Box::new(HeaderFirst::new(link, RxOptions::default())));
        assert_eq!(got.len(), 4);
        assert_eq!(t.complete, 4, "every frame and block decoded");
        for (r, f) in got.iter().zip(&frames) {
            assert_eq!(r.rx.header, f.header, "header fields");
            assert_eq!(r.cfg, f.config, "geometry from the header (and the probe)");
            assert!(r.rx.block_bits.iter().zip(&f.block_bits).all(|(a, b)| a == b));
        }
        assert_eq!(got[1].rx.probe_extent.as_ref().unwrap().width, 40);
        assert!(got[2].control_lines()[0].contains("ID N0CALL-3"), "{:?}", got[2].control_lines());
        assert!(got[3].control_lines()[0].starts_with("ctrl 1: ACK"), "{:?}", got[3].control_lines());
        assert!(got[0].line().contains("N0CALL-3 > *  UI"), "{}", got[0].line());
    }

    #[test]
    fn silence_and_noise_give_nothing() {
        let mut rng = cairn_phy::channel::Rng::new(9);
        let x: Vec<f64> = (0..10 * FS as usize).map(|_| 0.005 * rng.normal()).collect();
        let (frames, t) = decode_all(x, Box::new(HeaderFirst::new(LinkState::default(), RxOptions::default())));
        assert!(frames.is_empty());
        assert_eq!(t.detections, 0);
    }
}
