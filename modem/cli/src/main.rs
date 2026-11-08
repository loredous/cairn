//! `cairn-modem`: the Cairn Stage 2 modem's command line (SPEC 21.3 steps 1-4).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use cairn_audio::{InputStats, InputStream, LevelMeter, Levels, OutputPort, OutputStream, StreamOptions};
use cairn_modem::frames::{
    FrameSpec, PlacedFrame, PlacedHeader, PlacedTone, bits_hex, build_frame, level_tone, manifest_json, parse_prefix,
    parse_tones, peak_rms_dbfs,
};
use cairn_modem::headers::{self, HeaderSpec};
use cairn_modem::listen::{
    self, BufferSource, CHUNK, ChunkSource, ConfigSource, FixedConfig, HeaderFirst, ListenOptions, Output, PacedSource,
};
use cairn_modem::txseq::{self, TxTiming};
use cairn_phy::detector::DetectorConfig;
use cairn_phy::fields::{Address, frame_type};
use cairn_phy::params::{CP_LONG, CP_SHORT, FS};
use cairn_phy::{LinkState, RxOptions, Transmitter};
use cairn_ptt::{PttGuard, PttSpec, SerialLineSpec, SharedPtt};

mod signals;

/// Longest keyed transmission without `--force`, s.
const MAX_KEYED_S: f64 = 5.0;
/// Longest keyed transmission even with `--force`, s.
const HARD_MAX_KEYED_S: f64 = 300.0;

#[derive(Parser)]
#[command(name = "cairn-modem", version, about = "Cairn Stage 2 modem: audio, PTT, transmit and listen (SPEC 21.3)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List audio devices (select one with --device and a name substring)
    Devices,
    /// Show the input level (RMS, peak, DC, clipping) and capture counters
    Levels(LevelsArgs),
    /// Key the PTT for a few seconds, without audio
    PttTest(PttTestArgs),
    /// Send the SPEC 7.1 level-setting tone (1 kHz at the data's peak amplitude) for setting drive
    Tone(ToneArgs),
    /// Build frames with cairn-phy and transmit them (or write a WAV and manifest)
    Tx(TxArgs),
    /// Capture continuously (or read a WAV), detect and decode every frame
    Listen(ListenArgs),
}

#[derive(Args, Clone)]
struct AudioOpts {
    /// Audio device: a substring of the ALSA name or description (e.g. All-In-One-Cable), or "default"
    #[arg(short = 'd', long, env = "CAIRN_DEVICE", default_value = "default")]
    device: String,
    /// Callback period in frames (480 = 10 ms)
    #[arg(long, default_value_t = 480)]
    period: u32,
}

impl AudioOpts {
    fn stream(&self) -> StreamOptions {
        StreamOptions { period_frames: Some(self.period), ..Default::default() }
    }
}

#[derive(Args, Clone)]
struct PttOpts {
    /// PTT: none, vox, serial:PATH (a path or a /dev/serial/by-id/ substring), cm108:/dev/hidrawN, rigctld[:HOST[:PORT]]
    #[arg(long, env = "CAIRN_PTT", default_value = "none")]
    ptt: String,
    /// Serial line that keys (dtr or rts); the other line is held low
    #[arg(long, default_value = "dtr")]
    ptt_line: SerialLineSpec,
    /// Serial PTT is active low
    #[arg(long)]
    ptt_invert: bool,
    /// CM108 GPIO pin (1-8)
    #[arg(long, default_value_t = cairn_ptt::CM108_DEFAULT_GPIO)]
    ptt_gpio: u8,
}

impl PttOpts {
    fn open(&self) -> Result<SharedPtt, String> {
        let spec = PttSpec::parse(&self.ptt, self.ptt_line, self.ptt_invert, self.ptt_gpio)?;
        let ptt = spec.open().map_err(|e| format!("PTT {spec}: {e}"))?;
        let shared = SharedPtt::new(ptt);
        signals::register_ptt(&shared);
        Ok(shared)
    }
}

#[derive(Args, Clone)]
struct TimingOpts {
    /// TXDelay: PTT to first audio sample at the DAC, ms (SPEC 7.3)
    #[arg(long, default_value_t = 150.0)]
    txdelay: f64,
    /// TXTail: last sample at the DAC to PTT release, ms (SPEC 7.3)
    #[arg(long, default_value_t = 10.0)]
    txtail: f64,
}

impl TimingOpts {
    fn timing(&self) -> Result<TxTiming, String> {
        if !(0.0..=1000.0).contains(&self.txdelay) || !(0.0..=200.0).contains(&self.txtail) {
            return Err("TXDelay must be 0-1000 ms and TXTail 0-200 ms".into());
        }
        if !(20.0..=500.0).contains(&self.txdelay) || self.txtail > 50.0 {
            eprintln!("note: SPEC 7.3 ranges are TXDelay 20-500 ms and TXTail 0-50 ms");
        }
        Ok(TxTiming { txdelay_s: self.txdelay / 1e3, txtail_s: self.txtail / 1e3 })
    }
}

#[derive(Args, Clone)]
struct FrameOpts {
    /// Data rung, MCS 0-6
    #[arg(long, default_value_t = 2)]
    mcs: u8,
    /// Width, tones (24-56)
    #[arg(long, default_value_t = 24)]
    width: usize,
    /// Prefix: short (60 samples) or long (120); listen: force it instead of using the detector's
    #[arg(long)]
    prefix: Option<String>,
    /// Tones silenced in data symbols, e.g. 8-11,30 (never 3 or 20); session frames only
    #[arg(long)]
    mask: Option<String>,
    /// Data blocks per frame (default: as many as fit --frame-s)
    #[arg(long)]
    blocks: Option<usize>,
    /// Target frame time, s (default SPEC 10.2: 2 s at MCS 0-1, 4 s above)
    #[arg(long)]
    frame_s: Option<f64>,
}

impl FrameOpts {
    fn spec(&self) -> Result<FrameSpec, String> {
        let cp = match &self.prefix {
            Some(p) => parse_prefix(p)?,
            None => CP_SHORT,
        };
        let silenced = match &self.mask {
            Some(m) => parse_tones(m)?,
            None => Vec::new(),
        };
        if self.frame_s.is_some_and(|f| !(0.2..=4.0).contains(&f)) {
            return Err("--frame-s must be 0.2-4.0 s (SPEC 7.3: 4 s is the ceiling)".into());
        }
        let s =
            FrameSpec { mcs: self.mcs, width: self.width, cp, silenced, blocks: self.blocks, frame_s: self.frame_s };
        s.config()?;
        Ok(s)
    }
}

#[derive(Args)]
struct LevelsArgs {
    #[command(flatten)]
    audio: AudioOpts,
    /// Stop after this many seconds (0: until Ctrl-C)
    #[arg(long, default_value_t = 0.0)]
    seconds: f64,
    /// Report interval, s
    #[arg(long, default_value_t = 1.0)]
    interval: f64,
}

#[derive(Args)]
struct PttTestArgs {
    #[command(flatten)]
    ptt: PttOpts,
    /// Keyed time, s
    #[arg(long, default_value_t = 1.0)]
    seconds: f64,
    /// Allow more than 5 s
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct ToneArgs {
    #[command(flatten)]
    audio: AudioOpts,
    #[command(flatten)]
    ptt: PttOpts,
    #[command(flatten)]
    timing: TimingOpts,
    /// Tone length, s
    #[arg(long, default_value_t = 3.0)]
    seconds: f64,
    /// Allow a keyed transmission longer than 5 s
    #[arg(long)]
    force: bool,
    /// Write a WAV instead of playing
    #[arg(long)]
    wav: Option<PathBuf>,
}

#[derive(Args)]
struct TxArgs {
    #[command(flatten)]
    audio: AudioOpts,
    #[command(flatten)]
    ptt: PttOpts,
    #[command(flatten)]
    timing: TimingOpts,
    #[command(flatten)]
    frame: FrameOpts,
    /// Frames to send (each its own keyed transmission)
    #[arg(long, default_value_t = 1)]
    count: usize,
    /// Unkeyed gap between transmissions, s
    #[arg(long, default_value_t = 1.0)]
    gap: f64,
    /// Send the level-setting tone for this long first, s
    #[arg(long)]
    tone: Option<f64>,
    /// Payload seed; frame i uses seed * 1000 + i, as cairn-bench tx
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// This station's callsign and SSID, e.g. N0CALL-1: the header's source (SPEC 8.3, station ID)
    #[arg(long, env = "CAIRN_CALLSIGN")]
    callsign: Option<String>,
    /// Destination callsign (default: broadcast)
    #[arg(long)]
    to: Option<String>,
    /// Frame type: ui (broadcast data, the default), beacon, data (a session frame, with an ACK block), probe
    #[arg(long = "type", default_value = "ui")]
    frame_type: String,
    /// First sequence number: the frame counter of a broadcast (+1 per frame), the first block's in a session
    #[arg(long, default_value_t = 0)]
    seq: u16,
    /// Send the ID control block (B.6) in every frame (always in a BEACON)
    #[arg(long)]
    id: bool,
    /// Grid locator for the ID block, 4 or 6 characters
    #[arg(long)]
    grid: Option<String>,
    /// Session frames: this station's cap in tones, the width probe's extent (default: the width)
    #[arg(long)]
    cap: Option<usize>,
    /// Text to send as the payload (zero-padded); default: random payloads for the seed
    #[arg(long)]
    message: Option<String>,
    /// Random header bits as cairn-bench tx (no callsign; not decodable header first)
    #[arg(long)]
    random_header: bool,
    /// Write a WAV (and a cairn-bench/1 manifest next to it) instead of playing
    #[arg(long)]
    wav: Option<PathBuf>,
    /// Leading silence in the WAV, s
    #[arg(long, default_value_t = 1.0)]
    lead: f64,
    /// Allow a keyed transmission longer than 5 s
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct ListenArgs {
    #[command(flatten)]
    audio: AudioOpts,
    #[command(flatten)]
    frame: FrameOpts,
    /// Take the whole configuration from --mcs/--width/--blocks/--mask instead of the header
    #[arg(long)]
    fixed: bool,
    /// Session link: the peer's cap in tones, the extent of its width probe (default: --width)
    #[arg(long)]
    peer_cap: Option<usize>,
    /// Play the WAV at real-time pace, as a sound card would deliver it, and report the receive turnaround
    #[arg(long)]
    realtime: bool,
    /// Print the payload of each decoded frame as text
    #[arg(long)]
    text: bool,
    /// Data-block decoder threads
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Do not ask the scheduler to run the decoder at full speed (Linux uclamp)
    #[arg(long)]
    no_boost: bool,
    /// Read this WAV (48 kHz mono 16-bit) instead of the sound card, as fast as possible
    #[arg(long)]
    wav: Option<PathBuf>,
    /// Stop after this many seconds of audio (0: until Ctrl-C or the end of the WAV)
    #[arg(long, default_value_t = 0.0)]
    seconds: f64,
    /// Status interval, s of audio (0: none)
    #[arg(long, default_value_t = 5.0)]
    stats: f64,
    /// Preamble detection threshold (rho)
    #[arg(long, default_value_t = cairn_phy::detector::XCORR_THRESHOLD)]
    rho: f64,
    /// Also save the captured audio to this WAV (live input only; at most 10 minutes)
    #[arg(long)]
    record: Option<PathBuf>,
    /// Fail unless exactly this many frames decode with every block OK (for scripted checks)
    #[arg(long)]
    expect: Option<u64>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    signals::install();
    let r = match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::Levels(a) => levels(a),
        Cmd::PttTest(a) => ptt_test(a),
        Cmd::Tone(a) => tone(a),
        Cmd::Tx(a) => tx(a),
        Cmd::Listen(a) => listen_cmd(a),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn stopped() -> bool {
    signals::STOP.load(Ordering::Relaxed)
}

/// Sleep up to `d`, waking early on a signal.
fn nap(d: Duration) {
    let t0 = Instant::now();
    while !stopped() && t0.elapsed() < d {
        std::thread::sleep((d - t0.elapsed()).min(Duration::from_millis(20)));
    }
}

fn utc_hms(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", s / 3600, s / 60 % 60, s % 60, d.subsec_millis())
}

// --- devices, levels, ptt-test -------------------------------------------------

fn devices() -> Result<(), String> {
    let list = cairn_audio::list_devices().map_err(|e| e.to_string())?;
    println!("{:<44} {:>5} {:>5}  description", "device", "in", "out");
    for d in list {
        let flag = |listed: bool, ok: bool| match (listed, ok) {
            (true, true) => "48k",
            (true, false) => "yes",
            _ => "-",
        };
        println!(
            "{:<44} {:>5} {:>5}  {}",
            d.id,
            flag(d.input, d.input_48k),
            flag(d.output, d.output_48k),
            d.description.join(" / ")
        );
    }
    println!("\nSelect with --device and any part of the name or description, e.g. --device All-In-One-Cable.");
    Ok(())
}

fn stats_text(s: &InputStats) -> String {
    format!(
        "xruns {} gaps {} ({:.1} ms) overflow {} errors {}",
        s.xruns,
        s.gaps,
        s.gap_ns as f64 / 1e6,
        s.overflow_samples,
        s.errors
    )
}

fn levels_text(l: &Levels) -> String {
    format!(
        "rms {:6.1} dBFS (with DC {:6.1})  peak {:6.1} dBFS  dc {:+.4}  {}{}",
        l.rms_dbfs,
        l.rms_total_dbfs,
        l.peak_dbfs,
        l.dc,
        l.status(),
        if l.clipping() { format!(" ({} clipped samples)", l.clipped) } else { String::new() }
    )
}

fn levels(a: LevelsArgs) -> Result<(), String> {
    let mut input = InputStream::open(&a.audio.device, &a.audio.stream()).map_err(|e| e.to_string())?;
    println!("input {} ({})", input.id, input.format);
    let mut meter = LevelMeter::new();
    let mut total = LevelMeter::new();
    let mut buf = vec![0f32; 9600];
    let interval = (a.interval.max(0.1) * FS as f64) as u64;
    let limit = (a.seconds * FS as f64) as u64;
    let mut seen = 0u64;
    while !stopped() && (limit == 0 || seen < limit) {
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        meter.push(&buf[..n]);
        total.push(&buf[..n]);
        seen += n as u64;
        if meter.samples() >= interval {
            println!(
                "{:7.1} s  {}  | {}",
                seen as f64 / FS as f64,
                levels_text(&meter.take()),
                stats_text(&input.stats())
            );
        }
    }
    if total.samples() > 0 {
        println!("overall    {}  | {}", levels_text(&total.take()), stats_text(&input.stats()));
    }
    Ok(())
}

fn ptt_test(a: PttTestArgs) -> Result<(), String> {
    check_keyed(a.seconds, a.force)?;
    let ptt = a.ptt.open()?;
    println!("PTT {}", ptt.describe());
    let show = |what: &str| match ptt.read_back() {
        Ok(Some(k)) => println!("  {what}: line reads {}", if k { "KEYED" } else { "unkeyed" }),
        Ok(None) => println!("  {what}: (no read-back)"),
        Err(e) => println!("  {what}: read-back failed: {e}"),
    };
    show("before");
    let guard = PttGuard::key(&ptt).map_err(|e| format!("keying: {e}"))?;
    let t0 = Instant::now();
    show("keyed");
    nap(Duration::from_secs_f64(a.seconds));
    guard.release().map_err(|e| format!("release: {e}"))?;
    println!("  keyed for {:.3} s", t0.elapsed().as_secs_f64());
    show("after");
    if stopped() {
        return Err("interrupted (PTT released)".into());
    }
    Ok(())
}

fn check_keyed(seconds: f64, force: bool) -> Result<(), String> {
    if seconds.is_nan() || seconds <= 0.0 {
        return Err("keyed time must be positive".into());
    }
    if seconds > HARD_MAX_KEYED_S {
        return Err(format!("refusing to key for {seconds:.1} s (limit {HARD_MAX_KEYED_S} s)"));
    }
    if seconds > MAX_KEYED_S && !force {
        return Err(format!("refusing to key for {seconds:.1} s without --force (limit {MAX_KEYED_S} s)"));
    }
    Ok(())
}

// --- transmit -----------------------------------------------------------------------

/// One keyed transmission.
struct Item {
    label: String,
    samples: Vec<f64>,
    frame: Option<(FrameSpec, cairn_phy::FrameConfig, u64, Option<PlacedHeader>)>,
}

fn play(items: &[Item], a: &AudioOpts, p: &PttOpts, timing: &TxTiming, gap_s: f64) -> Result<(), String> {
    let ptt = p.open()?;
    let mut out = OutputStream::open(&a.device, &a.stream()).map_err(|e| e.to_string())?;
    if !out.wait_running(Duration::from_secs(2)) {
        return Err("the output device did not start".into());
    }
    // Let the latency estimate settle on a few callbacks.
    nap(Duration::from_millis(100));
    println!("output {} ({}), latency {:.1} ms", out.id, out.format, out.latency_ns() as f64 / 1e6);
    println!("PTT {}", ptt.describe());
    if ptt.is_null() {
        println!("note: no PTT line; the audio plays unkeyed");
    }
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            nap(Duration::from_secs_f64(gap_s));
        }
        if stopped() {
            return Err("interrupted (PTT released)".into());
        }
        let audio: Vec<f32> = it.samples.iter().map(|&v| v as f32).collect();
        let r = txseq::transmit(&mut out, &ptt, &audio, timing, &signals::STOP).map_err(|e| match e {
            txseq::TxError::Aborted => "interrupted (PTT released)".to_string(),
            e => format!("{e} (PTT released)"),
        })?;
        println!(
            "tx {}/{}: {}; keyed {:.3} s, TXDelay {:.1} ms, TXTail {:.1} ms (measured at the DAC), underruns {}",
            i + 1,
            items.len(),
            it.label,
            r.keyed_ms() / 1e3,
            r.txdelay_ms(),
            r.txtail_ms(),
            r.underruns
        );
        if let Ok(Some(true)) = ptt.read_back() {
            return Err("PTT still reads keyed after release".into());
        }
    }
    let s = out.stats();
    println!("output: underruns {} xruns {} errors {}", s.underruns, s.xruns, s.errors);
    Ok(())
}

fn write_wav(path: &Path, items: &[Item], lead_s: f64, gap_s: f64, seed: u64, tone_first: bool) -> Result<(), String> {
    let mut x = vec![0.0; (lead_s * FS as f64).round() as usize];
    let gap = (gap_s * FS as f64).round() as usize;
    let mut placed = Vec::new();
    let mut tone = None;
    for (i, it) in items.iter().enumerate() {
        let start = x.len();
        if let Some((spec, cfg, fseed, header)) = &it.frame {
            let (peak_dbfs, rms_dbfs) = peak_rms_dbfs(&it.samples);
            placed.push(PlacedFrame {
                index: placed.len(),
                spec: spec.clone(),
                cfg: cfg.clone(),
                seed: *fseed,
                start,
                n_samples: it.samples.len(),
                peak_dbfs,
                rms_dbfs,
                header: header.clone(),
            });
        } else if i == 0 && tone_first {
            tone = Some(PlacedTone { start, seconds: it.samples.len() as f64 / FS as f64 });
        }
        x.extend_from_slice(&it.samples);
        x.extend(std::iter::repeat_n(0.0, gap));
    }
    x.extend(std::iter::repeat_n(0.0, FS as usize / 2));
    cairn_phy::wav::write(path, &x).map_err(|e| format!("{}: {e}", path.display()))?;
    let mpath = path.with_extension("json");
    let m = manifest_json(&placed, tone, x.len(), seed, lead_s, gap_s);
    std::fs::write(&mpath, m).map_err(|e| format!("{}: {e}", mpath.display()))?;
    println!(
        "wrote {} ({:.2} s, {} frames) and {}",
        path.display(),
        x.len() as f64 / FS as f64,
        placed.len(),
        mpath.display()
    );
    Ok(())
}

fn tone(a: ToneArgs) -> Result<(), String> {
    let timing = a.timing.timing()?;
    let item = Item {
        label: format!("level tone 1000 Hz, {:.1} s, peak {:.0} dBFS", a.seconds, cairn_phy::params::PEAK_DBFS),
        samples: level_tone(a.seconds),
        frame: None,
    };
    if let Some(path) = &a.wav {
        return write_wav(path, &[item], 1.0, 0.5, 0, true);
    }
    check_keyed(txseq::keyed_seconds(item.samples.len(), &timing), a.force)?;
    println!("Set the radio's drive so this tone gives 70% of full deviation (SPEC 7.1).");
    play(&[item], &a.audio, &a.ptt, &timing, 0.0)
}

/// The header spec for frame `i` of a `tx` run with real headers.
fn header_spec(a: &TxArgs, spec: &FrameSpec, i: usize) -> Result<HeaderSpec, String> {
    let Some(call) = &a.callsign else {
        return Err("give --callsign (the header's source, SPEC 8.3 station ID) or --random-header".into());
    };
    let (source, text) = headers::parse_station(call)?;
    if source == Address::Broadcast {
        return Err("--callsign cannot be the broadcast address".into());
    }
    let destination = match &a.to {
        Some(t) => headers::parse_station(t)?.0,
        None => Address::Broadcast,
    };
    let ft = headers::parse_type(&a.frame_type)?;
    let mut h = HeaderSpec::broadcast(source, ft, 0, spec.width, spec.mcs, 0);
    h.callsign = text;
    h.destination = destination;
    h.cp = spec.cp;
    h.id = a.id || ft == frame_type::BEACON;
    h.grid = a.grid.clone();
    h.message = a.message.as_ref().map(|m| m.as_bytes().to_vec());
    if h.is_session() {
        h.usable = headers::usable_from_silenced(spec.width, &spec.silenced);
        h.cap = a.cap;
    } else if !spec.silenced.is_empty() || a.cap.is_some() {
        return Err("--mask and --cap apply to session frames; a broadcast uses its width, unmasked (ADR 0007)".into());
    }
    // Data blocks: --blocks, else what fits the frame time after the control
    // blocks; none in a BEACON or PROBE unless asked.
    let n_control = h.controls()?.len();
    let ctrl_symbols = cairn_phy::FrameConfig::new(24, 0, 0)
        .and_then(|c| c.with_controls(n_control))
        .and_then(|c| c.with_mask(h.usable.clone()))
        .map(|c| c.control_symbols())
        .map_err(|e| e.to_string())?;
    h.n_data = match (ft, spec.blocks) {
        (_, Some(b)) => b,
        (frame_type::BEACON | frame_type::PROBE | frame_type::ACK, None) => 0,
        _ => spec.n_blocks_with(ctrl_symbols)?,
    };
    if let Some(m) = &h.message {
        if spec.blocks.is_none() {
            // Just enough blocks for the message.
            h.n_data = h.n_data.min(m.len().div_ceil(126).max(1));
        }
    }
    // A broadcast counts frames; a session numbers blocks.
    let step = if h.is_session() { h.n_data.max(1) } else { 1 };
    h.sequence = a.seq.wrapping_add((i * step) as u16);
    Ok(h)
}

fn tx(a: TxArgs) -> Result<(), String> {
    let timing = a.timing.timing()?;
    let spec = a.frame.spec()?;
    if a.count == 0 {
        return Err("--count must be at least 1".into());
    }
    let mut items = Vec::new();
    if let Some(t) = a.tone {
        items.push(Item { label: format!("level tone {t:.1} s"), samples: level_tone(t), frame: None });
    }
    let mut transmitter = Transmitter::new();
    for i in 0..a.count {
        let seed = a.seed * 1000 + i as u64;
        if a.random_header {
            let cfg = spec.config()?;
            let f = build_frame(&mut transmitter, &cfg, seed);
            let samples = f.output();
            let label = format!(
                "random header, MCS {} x{} {} prefix, {} blocks, {:.3} s",
                cfg.mcs,
                cfg.width,
                spec.prefix_name(),
                cfg.n_blocks,
                samples.len() as f64 / FS as f64
            );
            items.push(Item { label, samples, frame: Some((spec.clone(), cfg.clone(), seed, None)) });
            continue;
        }
        let hs = header_spec(&a, &spec, i)?;
        let (f, payloads) = headers::build_header_frame(&mut transmitter, &hs, seed)?;
        let h = f.header.clone().expect("built from a header");
        let samples = f.output();
        let cfg = f.config.clone();
        let label = format!(
            "{} > {} {}{} seq {}, MCS {} x{} {} prefix, {} blocks, {} control, {:.3} s",
            h.source,
            h.destination,
            headers::type_name(h.frame_type),
            headers::flag_text(&h),
            h.sequence,
            cfg.mcs,
            cfg.width,
            spec.prefix_name(),
            cfg.n_blocks,
            cfg.n_control,
            samples.len() as f64 / FS as f64
        );
        let placed = PlacedHeader {
            header_hex: bits_hex(&f.header_bits[..cairn_phy::params::HEADER_INFO_BITS - cairn_phy::params::CRC_BITS]),
            control_hex: f.control_bits.iter().map(|b| bits_hex(&b[..b.len() - cairn_phy::params::CRC_BITS])).collect(),
            payload_hex: hs.message.as_ref().map(|_| payloads.iter().map(|p| bits_hex(p)).collect()),
            source: h.source.to_string(),
            destination: h.destination.to_string(),
            frame_type: headers::type_name(h.frame_type),
            sequence: h.sequence,
        };
        let fspec = FrameSpec { blocks: Some(cfg.n_blocks), ..spec.clone() };
        items.push(Item { label, samples, frame: Some((fspec, cfg, seed, Some(placed))) });
    }
    if let Some(path) = &a.wav {
        return write_wav(path, &items, a.lead, a.gap, a.seed, a.tone.is_some());
    }
    for it in &items {
        check_keyed(txseq::keyed_seconds(it.samples.len(), &timing), a.force)?;
    }
    play(&items, &a.audio, &a.ptt, &timing, a.gap)
}

// --- listen -------------------------------------------------------------------------

/// The sound card in [`CHUNK`]s.
struct AudioSource {
    input: InputStream,
    buf: Vec<f32>,
    limit: Option<usize>,
    taken: usize,
    error: Option<String>,
    record: Option<Vec<f64>>,
}

/// Longest `--record`, samples.
const MAX_RECORD: usize = 600 * FS as usize;

impl ChunkSource for AudioSource {
    fn next_chunk(&mut self, out: &mut Vec<f64>) -> Option<()> {
        out.clear();
        loop {
            if stopped() || self.limit.is_some_and(|l| self.taken >= l) {
                return None;
            }
            let want = CHUNK - out.len();
            match self.input.read(&mut self.buf[..want]) {
                Ok(n) => out.extend(self.buf[..n].iter().map(|&v| v as f64)),
                Err(e) => {
                    self.error = Some(e.to_string());
                    return None;
                }
            }
            if out.len() == CHUNK {
                self.taken += CHUNK;
                if let Some(r) = self.record.as_mut().filter(|r| r.len() < MAX_RECORD) {
                    r.extend_from_slice(out);
                }
                return Some(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn input_stats(&self) -> Option<InputStats> {
        Some(self.input.stats())
    }
    fn backlog(&self) -> usize {
        self.input.available()
    }
}

fn listen_cmd(a: ListenArgs) -> Result<(), String> {
    let spec = a.frame.spec()?;
    let force_cp = a.frame.prefix.as_ref().map(|_| spec.cp);
    let prefix = match force_cp {
        Some(CP_LONG) => "long",
        Some(_) => "short",
        None => "from the detector",
    };
    let rx_opts = RxOptions { threads: a.threads.clamp(1, 4), ..Default::default() };
    let config: Box<dyn ConfigSource> = if a.fixed {
        let n_blocks = spec.n_blocks()?;
        println!(
            "config (fixed): MCS {} x{}, {} blocks, prefix {prefix}{}",
            spec.mcs,
            spec.width,
            n_blocks,
            if spec.silenced.is_empty() { String::new() } else { format!(", tones {:?} silenced", spec.silenced) }
        );
        Box::new(FixedConfig::new(spec.width, spec.mcs, n_blocks, &spec.silenced, force_cp)?)
    } else {
        let link = LinkState {
            width: spec.width,
            usable: headers::usable_from_silenced(spec.width, &spec.silenced),
            cp: force_cp,
            cap: a.peer_cap,
        };
        // Validate the link state once.
        cairn_phy::FrameConfig::new(link.width, 0, 1)
            .and_then(|c| c.with_mask(link.usable.clone()))
            .and_then(|c| c.with_cap(link.probe_extent()))
            .map_err(|e| format!("link state: {e}"))?;
        println!(
            "config: from each header; session frames at x{}{} probe to {} tones; broadcasts at the width of their probe (ADR 0007); prefix {prefix}",
            link.width,
            if spec.silenced.is_empty() { String::new() } else { format!(" (tones {:?} masked)", spec.silenced) },
            link.probe_extent(),
        );
        Box::new(HeaderFirst::new(link, rx_opts.clone()))
    };
    let detector = DetectorConfig { xcorr_threshold: a.rho, ..Default::default() };
    let opts = ListenOptions {
        detector,
        rx: rx_opts,
        stats_every: (a.stats * FS as f64).round() as usize,
        boost: !a.no_boost,
    };
    let limit = (a.seconds > 0.0).then_some((a.seconds * FS as f64) as usize);

    let live = a.wav.is_none();
    let t_open = SystemTime::now();
    let mut audio_src: Option<AudioSource> = None;
    let mut file_src: Option<BufferSource> = None;
    let mut paced_src: Option<PacedSource> = None;
    let source: &mut dyn ChunkSource = match &a.wav {
        Some(p) => {
            let mut x = cairn_phy::wav::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
            if let Some(l) = limit {
                x.truncate(l);
            }
            println!(
                "reading {} ({:.2} s){}",
                p.display(),
                x.len() as f64 / FS as f64,
                if a.realtime { " at real-time pace" } else { "" }
            );
            if a.realtime { paced_src.insert(PacedSource::new(x)) } else { file_src.insert(BufferSource::new(x)) }
        }
        None => {
            let input = InputStream::open(&a.audio.device, &a.audio.stream()).map_err(|e| e.to_string())?;
            println!("listening on {} ({}); Ctrl-C to stop", input.id, input.format);
            let record = a.record.as_ref().map(|_| Vec::new());
            audio_src.insert(AudioSource { input, buf: vec![0.0; CHUNK], limit, taken: 0, error: None, record })
        }
    };
    let t0 = Instant::now();
    let stamp = |pos: usize| -> String {
        let rel = pos as f64 / FS as f64;
        if live {
            format!("[{} {:8.3} s]", utc_hms(t_open + Duration::from_secs_f64(rel)), rel)
        } else {
            format!("[{rel:8.3} s]")
        }
    };
    let mut max_latency = 0.0f64;
    let mut turnarounds: Vec<f64> = Vec::new();
    let totals = listen::run(source, config, opts, &mut |o| match o {
        Output::Frame(f) => {
            max_latency = max_latency.max(f.latency_ms);
            if let Some(t) = f.turnaround_ms {
                turnarounds.push(t);
            }
            println!("{} {}", stamp(f.det.start), f.line());
            for l in f.control_lines() {
                println!("    {l}");
            }
            if a.text && f.blocks_ok() > 0 {
                let bytes = headers::payload_bytes(&f.rx.block_bits);
                let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
                let text: String =
                    bytes[..end].iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
                println!("    text: {text:?}{}", if f.complete() { "" } else { " (blocks missing)" });
            }
        }
        Output::Unresolved(d, why) => {
            println!("{} detection (rho {:.2}) not decoded: {why}", stamp(d.start), d.quality)
        }
        Output::Stats(s) => {
            let input = s.input.map(|i| format!(" | in: {}", stats_text(&i))).unwrap_or_default();
            let backlog =
                if live { format!(" | queue {:.0} ms", s.backlog as f64 * 1e3 / FS as f64) } else { String::new() };
            println!(
                "{} level {}{input}{backlog} | detections {} frames {} headers ok {}",
                stamp(s.position),
                levels_text(&s.levels),
                s.detections,
                s.frames,
                s.headers_ok
            );
        }
    });
    let secs = totals.samples as f64 / FS as f64;
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "done: {:.1} s of audio in {:.1} s ({:.0}x real time); detections {}, frames {}, headers ok {}, blocks {}/{}, worst decode latency {:.0} ms",
        secs,
        wall,
        secs / wall.max(1e-9),
        totals.detections,
        totals.frames,
        totals.headers_ok,
        totals.blocks_ok,
        totals.blocks,
        max_latency
    );
    if !turnarounds.is_empty() {
        turnarounds.sort_by(f64::total_cmp);
        let pct = |p: f64| turnarounds[((turnarounds.len() - 1) as f64 * p).round() as usize];
        println!(
            "receive turnaround (last sample to result, SPEC 7.3 budget 100 ms): p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms over {} frames",
            pct(0.5),
            pct(0.99),
            turnarounds[turnarounds.len() - 1],
            turnarounds.len()
        );
    }
    if let Some(src) = audio_src.as_mut() {
        println!("input: {}", stats_text(&src.input.stats()));
        if let (Some(path), Some(r)) = (&a.record, &src.record) {
            cairn_phy::wav::write(path, r).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("recorded {:.1} s to {}", r.len() as f64 / FS as f64, path.display());
        }
        if let Some(e) = src.error.take() {
            return Err(e);
        }
    }
    if let Some(n) = a.expect.filter(|&n| totals.complete != n || totals.detections != n) {
        return Err(format!(
            "expected {n} frames with every block OK and no other detections; got {} complete of {} detections",
            totals.complete, totals.detections
        ));
    }
    Ok(())
}
