//! `cairn-modem selftest`: checks that need no transmission, each PASS, WARN
//! or FAIL with a hint, and with `--transmit` (explicit, confirmed at run
//! time) one short keyed ID frame for a loopback check (SPEC 13.6).
//!
//! The built-in reference recording (a header-first UI broadcast through
//! the simulator's mic-path channel at 14 dB, from `modem/fixtures/rx`) goes
//! through the same receive path as live audio: the capture ring with its
//! gap handling ([`cairn_audio::MockCapture`]), the detector, carrier detect,
//! the PHY receiver and the link layer of a [`Modem`].

use std::path::PathBuf;

use cairn_audio::{InputStream, MockCapture, RtGrant};
use cairn_link::Instant;
use cairn_phy::fields::frame_type;

use crate::config::FileConfig;
use crate::headers::{self, HeaderSpec};
use crate::hw::{Hardware, PttChoice, Terminal, confirm_keying};
use crate::runtime::{FrameInfo, Modem, ModemConfig, Output};
use crate::txseq::TxTiming;
use crate::unattended::Policy;

/// The reference recording.
pub const REFERENCE_WAV: &[u8] = include_bytes!("../../fixtures/rx/hdr_mic_w27_ui_broadcast_snr14.wav");
/// Who sent it.
pub const REFERENCE_SOURCE: &str = "N0CALL-2";

/// A check's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Fine.
    Pass,
    /// Works, but worth a look.
    Warn,
    /// Will not work as configured.
    Fail,
    /// Not checked (nothing to check, or a check it depends on failed).
    Skip,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

/// One check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// The outcome.
    pub status: Status,
    /// What was found.
    pub detail: String,
    /// How to fix it.
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
        Check { name, status, detail: detail.into(), hint: None }
    }

    fn hint(mut self, h: impl Into<String>) -> Check {
        self.hint = Some(h.into());
        self
    }

    /// The report line(s).
    pub fn line(&self) -> String {
        let mut s = format!("{} {:<22} {}", self.status.tag(), self.name, self.detail);
        if let Some(h) = &self.hint {
            s += &format!("\n     {:<22} fix: {h}", "");
        }
        s
    }
}

/// How the config file was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigState {
    /// Read from this path.
    Loaded(PathBuf),
    /// No file there (built-in defaults and flags only).
    Missing(Option<PathBuf>),
    /// Not read on request (`--no-config`): flags and built-in defaults only.
    Skipped,
    /// Unreadable or invalid.
    Error(String),
}

/// The audio of a frame built from a header spec (f32, peaks at -3 dBFS).
pub fn frame_audio(spec: &HeaderSpec) -> Result<Vec<f32>, String> {
    let mut tx = cairn_phy::Transmitter::new();
    let (f, _) = headers::build_header_frame(&mut tx, spec, 1)?;
    Ok(f.output().iter().map(|&v| v as f32).collect())
}

fn station_spec(callsign: &str, ftype: u8, seq: u16) -> Result<HeaderSpec, String> {
    let (source, text) = headers::parse_station(callsign)?;
    if source == cairn_phy::fields::Address::Broadcast {
        return Err("the callsign cannot be the broadcast address".into());
    }
    let mut h = HeaderSpec::broadcast(source, ftype, seq, 24, 0, 0);
    h.callsign = text;
    h.id = true;
    Ok(h)
}

/// A BEACON with the ID control block: the station's identification frame.
pub fn id_frame(callsign: &str, grid: Option<&str>) -> Result<Vec<f32>, String> {
    let mut h = station_spec(callsign, frame_type::BEACON, 0)?;
    h.grid = grid.map(str::to_string);
    frame_audio(&h)
}

/// A UI broadcast with the ID block carrying `text` (MCS 0, 24 tones).
pub fn text_frame(callsign: &str, text: &str, seq: u16) -> Result<Vec<f32>, String> {
    let mut h = station_spec(callsign, frame_type::UI, seq)?;
    h.message = Some(text.as_bytes().to_vec());
    h.n_data = text.len().div_ceil(126).max(1);
    frame_audio(&h)
}

/// Decode `samples` through the live receive path (a mock capture device
/// feeding a [`Modem`]); every decoded header.
pub fn decode(samples: Vec<f32>, me: &str) -> Result<Vec<FrameInfo>, String> {
    let n = samples.len();
    let mut input = InputStream::mock(MockCapture::new(samples)).map_err(|e| e.to_string())?;
    let (call, ssid) = me.rsplit_once('-').map_or((me, 0), |(c, s)| (c, s.parse().unwrap_or(0)));
    let mut c = cairn_link::Config::for_callsign(call, ssid);
    c.seed = 1;
    let mut cfg = ModemConfig::new(c);
    cfg.rx.threads = 1;
    cfg.ring_seconds = 0.0;
    let mut modem = Modem::new(cfg);
    let mut buf = vec![0f32; 480];
    let mut chunk: Vec<f64> = Vec::with_capacity(480);
    let mut pos = 0usize;
    let mut heard = Vec::new();
    let t0 = std::time::Instant::now();
    while pos + 480 <= n && t0.elapsed().as_secs() < 30 {
        let got = input.read(&mut buf[..480 - chunk.len()]).map_err(|e| e.to_string())?;
        chunk.extend(buf[..got].iter().map(|&v| v as f64));
        for g in input.take_gaps() {
            modem.note_gap(g.at as usize, g.samples as usize);
        }
        if chunk.len() == 480 {
            let t = Instant((pos as u64 * 1_000_000) / 48_000);
            modem.push_audio(&chunk, t, t);
            pos += 480;
            chunk.clear();
            heard.extend(modem.drain().into_iter().filter_map(|o| match o {
                Output::Heard(f) => Some(f),
                _ => None,
            }));
        } else if got == 0 {
            if input.ended() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    Ok(heard)
}

/// The reference recording through the receive path.
pub fn reference_check() -> Check {
    let x = match cairn_phy::wav::parse(REFERENCE_WAV) {
        Ok(x) => x,
        Err(e) => return Check::new("reference decode", Status::Fail, format!("built-in WAV: {e}")),
    };
    match decode(x.iter().map(|&v| v as f32).collect(), "N0TEST-9") {
        Ok(h) => match h.as_slice() {
            [f] if f.blocks_ok == f.blocks && f.header.src.to_string() == REFERENCE_SOURCE => Check::new(
                "reference decode",
                Status::Pass,
                format!(
                    "{} > {} {:?}, {} tones, {}/{} blocks, SNR {:.1} dB, decode {:.0} ms",
                    f.header.src, f.header.dst, f.header.ftype, f.width, f.blocks_ok, f.blocks, f.snr_db, f.decode_ms
                ),
            ),
            other => Check::new(
                "reference decode",
                Status::Fail,
                format!("expected 1 frame from {REFERENCE_SOURCE} with every block; got {}", other.len()),
            )
            .hint("a build or CPU problem: run `cargo test --release` in modem/ and report it"),
        },
        Err(e) => Check::new("reference decode", Status::Fail, e),
    }
}

/// Options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Run the keyed loopback check (asks for confirmation first).
    pub transmit: bool,
    /// Seconds of input to measure for the receive level (0: skip).
    pub level_seconds: f64,
}

fn valid_call(c: &str) -> Result<(), String> {
    crate::control::parse_call(c)?;
    let (call, ssid) = c.rsplit_once('-').map_or((c, 0u8), |(a, s)| (a, s.parse().unwrap_or(0)));
    cairn_link::IdBlock::new(&call.to_ascii_uppercase(), ssid, None).map_err(|e| format!("{c}: {e:?}"))?;
    Ok(())
}

fn rt_check(g: RtGrant) -> Check {
    match &g {
        RtGrant::Realtime { .. } => Check::new("real-time priority", Status::Pass, format!("audio threads get {g}")),
        RtGrant::Nice { .. } => Check::new("real-time priority", Status::Warn, format!("only {g}"))
            .hint("allow rtprio for your user or run rtkit (setup prints how; docs/bench-testing.md)"),
        RtGrant::Denied(_) => Check::new("real-time priority", Status::Warn, format!("{g}")).hint(
            "audio may be lost under load: add '@audio - rtprio 95' to /etc/security/limits.d/ and join the audio group, or run rtkit",
        ),
    }
}

/// Run every check. `cfg` is the effective configuration (profile, file and
/// flags merged).
pub fn run(
    state: &ConfigState,
    cfg: &FileConfig,
    hw: &mut dyn Hardware,
    term: &mut dyn Terminal,
    opts: &Options,
) -> Vec<Check> {
    let mut out = Vec::new();
    // Configuration.
    out.push(match state {
        ConfigState::Loaded(p) => Check::new("config file", Status::Pass, p.display().to_string()),
        ConfigState::Missing(p) => Check::new(
            "config file",
            Status::Warn,
            format!("none at {}", p.as_ref().map_or("(no home directory)".into(), |p| p.display().to_string())),
        )
        .hint("run `cairn-modem setup` to write one (flags and built-in defaults are used meanwhile)"),
        ConfigState::Skipped => {
            Check::new("config file", Status::Skip, "skipped (--no-config); using flags and built-in defaults")
        }
        ConfigState::Error(e) => {
            Check::new("config file", Status::Fail, e.clone()).hint("fix the file, or rerun setup")
        }
    });
    out.push(match &cfg.station.rig {
        Some(r) => match crate::config::find_rig(r, crate::config::user_rigs_dir().as_deref()) {
            Ok(rig) if rig.profile.profile.status == "measured" => Check::new(
                "rig profile",
                Status::Pass,
                format!("{r}: measured on {}", rig.profile.profile.measured_on.as_deref().unwrap_or("?")),
            ),
            Ok(_) => Check::new("rig profile", Status::Pass, format!("{r} (unverified defaults; setup can measure)")),
            Err(e) => Check::new("rig profile", Status::Fail, e).hint("`cairn-modem rigs` lists the profiles"),
        },
        None => Check::new("rig profile", Status::Skip, "none named (station.rig or --rig)"),
    });
    let callsign = cfg.station.callsign.clone();
    out.push(match &callsign {
        Some(c) => match valid_call(c) {
            Ok(()) => Check::new("callsign", Status::Pass, c.clone()),
            Err(e) => Check::new("callsign", Status::Fail, e).hint("CALL or CALL-SSID (SSID 0-15)"),
        },
        None => Check::new("callsign", Status::Fail, "not set").hint(
            "give --callsign, or set station.callsign (every header and ID block carries it; needed to transmit)",
        ),
    });
    if let Some(g) = &cfg.station.grid {
        if !(g.len() == 4 || g.len() == 6) {
            out.push(Check::new("grid", Status::Fail, format!("{g:?}")).hint("4 or 6 characters, e.g. EM10 or EM10ab"));
        }
    }
    let ptt = PttChoice::from_section(&cfg.ptt);
    let mut problems = Vec::new();
    let t = &cfg.timing;
    if t.txdelay_ms.is_some_and(|d| !(0.0..=1000.0).contains(&d)) {
        problems.push("timing.txdelay_ms must be 0-1000".to_string());
    }
    if t.txtail_ms.is_some_and(|d| !(0.0..=200.0).contains(&d)) {
        problems.push("timing.txtail_ms must be 0-200".to_string());
    }
    if cfg.link.cap.is_some_and(|c| !(24..=56).contains(&c)) {
        problems.push("link.cap must be 24-56".to_string());
    }
    if cfg.link.max_frame.is_some_and(|f| !(0.5..=4.0).contains(&f)) {
        problems.push("link.max_frame must be 0.5-4.0".to_string());
    }
    if let Err(e) = &ptt {
        problems.push(format!("ptt.line: {e}"));
    }
    if let Ok(p) = &ptt {
        let spec = if p.spec.ends_with(":auto") { "cm108:/dev/null" } else { p.spec.as_str() };
        if let Err(e) = cairn_ptt::PttSpec::parse(spec, p.line, p.invert, p.gpio) {
            problems.push(format!("ptt.method: {e}"));
        }
    }
    let unattended = cfg.unattended.enabled == Some(true);
    let policy = Policy::from_section(&cfg.unattended);
    if let Err(e) = &policy {
        problems.push(e.clone());
    }
    out.push(if problems.is_empty() {
        Check::new("settings", Status::Pass, "timing, width, PTT and unattended settings valid")
    } else {
        Check::new("settings", Status::Fail, problems.join("; "))
    });

    // Audio.
    let device = cfg.audio.device.clone().unwrap_or_else(|| "default".into());
    let mut audio_ok = false;
    match hw.audio_devices() {
        Ok(list) => {
            let q = cairn_audio::normalize(&device);
            let found: Vec<_> = list
                .iter()
                .filter(|d| {
                    d.id == device
                        || cairn_audio::normalize(&d.id).contains(&q)
                        || cairn_audio::normalize(&d.description).contains(&q)
                })
                .collect();
            out.push(match found.as_slice() {
                [] => Check::new("audio device", Status::Fail, format!("no device matches {device:?}"))
                    .hint("plug the interface in; `cairn-modem devices` lists names to use as audio.device"),
                [d, ..] if !(d.input && d.output) => Check::new(
                    "audio device",
                    Status::Fail,
                    format!("{} lacks 48 kHz {}", d.id, if d.input { "playback" } else { "capture" }),
                )
                .hint("pick the interface's own PCM (e.g. front:CARD=...,DEV=0 or plughw:...)"),
                [d, ..] => {
                    audio_ok = true;
                    Check::new("audio device", Status::Pass, format!("{} ({})", d.id, d.description))
                }
            });
        }
        Err(e) => out.push(Check::new("audio device", Status::Fail, e)),
    }
    for (name, r) in [("audio input", hw.open_input(&device)), ("audio output", hw.open_output(&device))] {
        out.push(match r {
            Ok(d) => Check::new(name, Status::Pass, d),
            Err(e) if audio_ok => Check::new(name, Status::Fail, e)
                .hint("another program may hold the device (stop it, or use plughw:/front:)"),
            Err(e) => Check::new(name, Status::Skip, e),
        });
    }
    if opts.level_seconds > 0.0 && out.iter().any(|c| c.name == "audio input" && c.status == Status::Pass) {
        out.push(match hw.measure_input(&device, opts.level_seconds) {
            Ok(l) if l.clipped > 0 => Check::new(
                "receive level",
                Status::Fail,
                format!("clipping ({} samples at full scale), peak {:.1} dBFS", l.clipped, l.peak_dbfs),
            )
            .hint("lower the radio's volume or the capture gain (run --rx-agc does it between frames)"),
            Ok(l) if l.rms_dbfs < -35.0 => {
                Check::new("receive level", Status::Warn, format!("noise {:.1} dBFS RMS, very low", l.rms_dbfs)).hint(
                    "open the squelch (carrier detect needs the noise) and raise the volume: target about -20 dBFS",
                )
            }
            Ok(l) if l.rms_dbfs > -10.0 || l.peak_dbfs > -3.0 => Check::new(
                "receive level",
                Status::Warn,
                format!("noise {:.1} dBFS RMS, peak {:.1} dBFS: hot", l.rms_dbfs, l.peak_dbfs),
            )
            .hint("lower the volume: open-squelch noise near -20 dBFS, signal peaks under -3 dBFS (SPEC 13.5)"),
            Ok(l) => Check::new(
                "receive level",
                Status::Pass,
                format!(
                    "noise {:.1} dBFS RMS, peak {:.1} dBFS (target about -20 RMS, peaks under -3)",
                    l.rms_dbfs, l.peak_dbfs
                ),
            ),
            Err(e) => Check::new("receive level", Status::Fail, e),
        });
    }

    // PTT, never keyed here.
    if let Ok(p) = &ptt {
        let kind = p.spec.split(':').next().unwrap_or("").to_ascii_lowercase();
        out.push(match kind.as_str() {
            "none" => Check::new("PTT", Status::Warn, "none: receive only (frames play unkeyed)")
                .hint("set ptt.method (serial:..., cm108:..., rigctld) to transmit"),
            "vox" => Check::new("PTT", Status::Warn, "VOX: broadcast only; sessions are not supported (SPEC 13.3)")
                .hint("a PTT line is needed for sessions"),
            _ => match hw.ptt_idle(p) {
                Ok((d, Some(false))) => Check::new("PTT", Status::Pass, format!("{d}: opens, line reads unkeyed")),
                Ok((d, None)) => Check::new("PTT", Status::Pass, format!("{d}: opens (no read-back)")),
                Ok((d, Some(true))) => Check::new("PTT", Status::Fail, format!("{d}: line reads KEYED at idle"))
                    .hint("check ptt.invert and ptt.line, then `cairn-modem ptt-test` into a dummy load"),
                Err(e) => Check::new("PTT", Status::Fail, e).hint(
                    "check the path (`cairn-modem devices` lists candidates) and permissions (dialout / udev rule)",
                ),
            },
        });
    }

    out.push(rt_check(hw.rt_check()));

    // Mixer.
    let control = cfg.audio.rx_agc_control.as_deref();
    if audio_ok {
        out.push(match hw.capture_control(&device, control) {
            Ok((n, g)) => Check::new("capture gain control", Status::Pass, format!("'{n}' at {g:+.1} dB")),
            Err(e) if cfg.audio.rx_agc == Some(true) => Check::new("capture gain control", Status::Fail, e)
                .hint("audio.rx_agc needs a capture control: set audio.rx_agc_control or turn rx_agc off"),
            Err(e) => Check::new("capture gain control", Status::Warn, e)
                .hint("no receive AGC: set the level by hand with `cairn-modem levels`"),
        });
    }

    out.push(reference_check());

    // Ports.
    for (name, addr, default) in
        [("KISS port", &cfg.host.kiss, "127.0.0.1:8001"), ("API port", &cfg.host.api, "127.0.0.1:8002")]
    {
        let a = addr.as_deref().unwrap_or(default);
        if a == "off" {
            continue;
        }
        out.push(match hw.port_free(a) {
            Ok(()) => Check::new(name, Status::Pass, format!("{a} free")),
            Err(e) => Check::new(name, Status::Fail, format!("{a}: {e}"))
                .hint("another cairn-modem (or program) listens there: stop it or change host.kiss / host.api"),
        });
    }

    // Unattended.
    if let Ok(p) = &policy {
        let missing: Vec<String> = p
            .hooks
            .programs()
            .into_iter()
            .filter(|(_, prog)| !hw.executable(prog))
            .map(|(e, prog)| format!("{} ({prog})", e.name()))
            .collect();
        if unattended || !missing.is_empty() {
            out.push(if !missing.is_empty() {
                Check::new("unattended", Status::Fail, format!("hook programs not executable: {}", missing.join(", ")))
                    .hint("give the full path of an executable file")
            } else if unattended && callsign.is_none() {
                Check::new("unattended", Status::Fail, "no callsign")
            } else if unattended && p.allow.is_empty() {
                Check::new("unattended", Status::Warn, p.describe())
                    .hint("anyone not denied may connect: consider unattended.allow (docs/operator-guide.md)")
            } else {
                Check::new("unattended", Status::Pass, p.describe())
            });
        }
    }

    // The keyed loopback.
    if opts.transmit {
        out.push(transmit_check(cfg, hw, term, &device, ptt.ok().as_ref(), callsign.as_deref()));
    }
    out
}

fn transmit_check(
    cfg: &FileConfig,
    hw: &mut dyn Hardware,
    term: &mut dyn Terminal,
    device: &str,
    ptt: Option<&PttChoice>,
    callsign: Option<&str>,
) -> Check {
    const NAME: &str = "transmit loopback";
    let (Some(ptt), Some(call)) = (ptt, callsign) else {
        return Check::new(NAME, Status::Skip, "needs a valid PTT and callsign");
    };
    let audio = match id_frame(call, cfg.station.grid.as_deref()) {
        Ok(a) => a,
        Err(e) => return Check::new(NAME, Status::Fail, e),
    };
    let timing = TxTiming {
        txdelay_s: cfg.timing.txdelay_ms.unwrap_or(150.0) / 1e3,
        txtail_s: cfg.timing.txtail_ms.unwrap_or(10.0) / 1e3,
    };
    let keyed = crate::txseq::keyed_seconds(audio.len(), &timing);
    let Some(ok) =
        confirm_keying(term, "one ID frame (a BEACON with the ID block) for a loopback check", keyed, call, true)
    else {
        return Check::new(NAME, Status::Skip, "declined at the prompt; nothing transmitted");
    };
    match hw.transmit_and_listen(&ok, device, ptt, timing, &audio, 1.0) {
        Err(e) if crate::hw::is_ptt_fault(&e) => Check::new(NAME, Status::Fail, e)
            .hint("the PTT interface failed: replug it and check the radio is not keyed, then rerun; nothing more was transmitted"),
        Err(e) => Check::new(NAME, Status::Fail, format!("{e} (PTT released)")),
        Ok((m, heard)) => {
            if m.after_read == Some(true) {
                return Check::new(NAME, Status::Fail, "PTT still reads keyed after release")
                    .hint("unplug the interface; check ptt.invert");
            }
            if !ptt.is_null() && m.keyed_read == Some(false) {
                return Check::new(NAME, Status::Fail, "the PTT line did not read keyed while transmitting")
                    .hint("check ptt.line and ptt.invert");
            }
            let me = format!("{}-15", call.split('-').next().unwrap_or(call));
            let frames = decode(heard, &me).unwrap_or_default();
            let own = frames.iter().find(|f| f.header.src.to_string().eq_ignore_ascii_case(call));
            let timing = format!(
                "keyed {:.0} ms, TXDelay {:.0} ms, TXTail {:.0} ms at the DAC",
                m.keyed_ms, m.txdelay_ms, m.txtail_ms
            );
            match own {
                Some(f) => {
                    Check::new(NAME, Status::Pass, format!("{timing}; own ID frame heard back, SNR {:.1} dB", f.snr_db))
                }
                None => Check::new(NAME, Status::Warn, format!("{timing}; own frame not heard on the input")).hint(
                    "expected unless the input hears the output (a second receiver on the frequency, or a loopback cable); \
                     a half-duplex radio cannot hear itself",
                ),
            }
        }
    }
}

/// The report: every line, then a summary; the worst status.
pub fn report(checks: &[Check]) -> (String, Status) {
    let mut s: String = checks.iter().map(|c| c.line() + "\n").collect();
    let count = |st| checks.iter().filter(|c| c.status == st).count();
    let worst = checks.iter().map(|c| c.status).filter(|s| *s != Status::Skip).max().unwrap_or(Status::Pass);
    s += &format!(
        "\n{} passed, {} warnings, {} failed, {} skipped",
        count(Status::Pass),
        count(Status::Warn),
        count(Status::Fail),
        count(Status::Skip)
    );
    (s, worst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::LevelReport;
    use crate::hw::mock::{MockHw, ScriptTerm};

    fn owner_like() -> FileConfig {
        let c = FileConfig::parse(
            "[station]\ncallsign = \"N0CALL-1\"\ngrid = \"EM10\"\nrig = \"aioc-ht\"\n\
             [ptt]\nmethod = \"serial:/dev/serial/by-id/usb-AIOC_All-In-One-Cable_1234-if00\"\n",
        )
        .unwrap();
        crate::config::effective(&c, None, None).unwrap()
    }

    fn by_name<'a>(c: &'a [Check], n: &str) -> &'a Check {
        c.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("no check {n}"))
    }

    #[test]
    fn reference_recording_decodes_through_the_receive_path() {
        let c = reference_check();
        assert_eq!(c.status, Status::Pass, "{}", c.line());
        assert!(c.detail.contains("N0CALL-2"));
    }

    #[test]
    fn a_good_station_passes_without_keying() {
        let mut hw = MockHw::default();
        let mut term = ScriptTerm::new(&[]);
        let cfg = owner_like();
        let checks = run(
            &ConfigState::Loaded("/home/op/.config/cairn/cairn.toml".into()),
            &cfg,
            &mut hw,
            &mut term,
            &Options { transmit: false, level_seconds: 2.0 },
        );
        let (text, worst) = report(&checks);
        assert_eq!(worst, Status::Pass, "{text}");
        assert!(hw.keyed.is_empty(), "nothing keyed: {:?}", hw.keyed);
        for n in ["config file", "rig profile", "callsign", "settings", "audio device", "audio input", "audio output"] {
            assert_eq!(by_name(&checks, n).status, Status::Pass, "{n}");
        }
        assert!(by_name(&checks, "PTT").detail.contains("reads unkeyed"));
        assert!(by_name(&checks, "receive level").detail.contains("-21.0"));
        assert!(hw.calls.contains(&"port_free 127.0.0.1:8001".to_string()));
        assert!(hw.calls.contains(&"port_free 127.0.0.1:8002".to_string()));
    }

    #[test]
    fn skipped_config_says_so_and_is_not_a_missing_home() {
        let mut hw = MockHw::default();
        let mut term = ScriptTerm::new(&[]);
        let checks = run(
            &ConfigState::Skipped,
            &owner_like(),
            &mut hw,
            &mut term,
            &Options { transmit: false, level_seconds: 0.0 },
        );
        let c = by_name(&checks, "config file");
        assert_eq!(c.status, Status::Skip);
        assert!(c.detail.contains("--no-config") && !c.detail.contains("home directory"), "{}", c.detail);
    }

    #[test]
    fn problems_fail_or_warn_with_hints() {
        let mut hw = MockHw {
            idle_read: Some(true),
            rt: RtGrant::Denied("no RLIMIT_RTPRIO, no rtkit".into()),
            control: Err("no capture control".into()),
            busy_ports: vec!["127.0.0.1:8002".into()],
            ..Default::default()
        };
        hw.levels.push_back(LevelReport { rms_dbfs: -50.0, peak_dbfs: -40.0, clipped: 0 });
        let mut cfg = owner_like();
        cfg.station.callsign = None;
        cfg.timing.txdelay_ms = Some(5000.0);
        cfg.unattended.enabled = Some(true);
        cfg.unattended.hooks.connected = vec!["/opt/missing/hook".into()];
        let mut term = ScriptTerm::new(&[]);
        let checks = run(
            &ConfigState::Missing(None),
            &cfg,
            &mut hw,
            &mut term,
            &Options { transmit: false, level_seconds: 1.0 },
        );
        let (text, worst) = report(&checks);
        assert_eq!(worst, Status::Fail, "{text}");
        assert_eq!(by_name(&checks, "config file").status, Status::Warn);
        assert_eq!(by_name(&checks, "callsign").status, Status::Fail);
        assert!(by_name(&checks, "settings").detail.contains("txdelay_ms"));
        assert_eq!(by_name(&checks, "PTT").status, Status::Fail);
        assert!(by_name(&checks, "PTT").hint.as_ref().unwrap().contains("invert"));
        assert_eq!(by_name(&checks, "real-time priority").status, Status::Warn);
        assert_eq!(by_name(&checks, "receive level").status, Status::Warn);
        assert_eq!(by_name(&checks, "capture gain control").status, Status::Warn);
        assert_eq!(by_name(&checks, "API port").status, Status::Fail);
        assert_eq!(by_name(&checks, "KISS port").status, Status::Pass);
        assert!(by_name(&checks, "unattended").detail.contains("connected (/opt/missing/hook)"));
        assert!(text.contains("fix:"));
        assert!(hw.keyed.is_empty());
    }

    #[test]
    fn missing_device_and_vox() {
        let mut hw = MockHw { devices: vec![], fail_open: true, ..Default::default() };
        let mut cfg = owner_like();
        cfg.ptt.method = Some("vox".into());
        let checks = run(
            &ConfigState::Loaded("c".into()),
            &cfg,
            &mut hw,
            &mut ScriptTerm::new(&[]),
            &Options { transmit: false, level_seconds: 1.0 },
        );
        assert_eq!(by_name(&checks, "audio device").status, Status::Fail);
        assert_eq!(by_name(&checks, "audio input").status, Status::Skip);
        assert!(checks.iter().all(|c| c.name != "receive level"), "no level without an input");
        assert!(by_name(&checks, "PTT").detail.contains("broadcast only"));
        assert!(!hw.calls.iter().any(|c| c.starts_with("ptt_idle")), "VOX has no line to open");
    }

    #[test]
    fn a_ptt_fault_in_the_transmit_check_fails_the_self_test_clearly() {
        let mut hw = MockHw { ptt_fault: Some("Broken pipe (os error 32)".into()), ..Default::default() };
        let mut term = ScriptTerm::new(&["yes"]);
        let checks = run(
            &ConfigState::Loaded("c".into()),
            &owner_like(),
            &mut hw,
            &mut term,
            &Options { transmit: true, level_seconds: 0.0 },
        );
        let c = by_name(&checks, "transmit loopback");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("MAY STILL BE KEYED") && c.detail.contains("Broken pipe"), "{}", c.detail);
        assert!(!c.detail.contains("(PTT released)"));
        assert!(c.hint.as_ref().unwrap().contains("replug"));
        let (text, worst) = report(&checks);
        assert_eq!(worst, Status::Fail, "{text}");
        assert_eq!(hw.keyed.len(), 1, "no second attempt");
    }

    #[test]
    fn transmit_check_needs_an_explicit_yes() {
        let cfg = owner_like();
        for answer in ["y", "", "YES", "no"] {
            let mut hw = MockHw::default();
            let mut term = ScriptTerm::new(&[answer]);
            let checks = run(
                &ConfigState::Loaded("c".into()),
                &cfg,
                &mut hw,
                &mut term,
                &Options { transmit: true, level_seconds: 0.0 },
            );
            assert_eq!(by_name(&checks, "transmit loopback").status, Status::Skip, "answer {answer:?}");
            assert!(hw.keyed.is_empty(), "answer {answer:?} keyed: {:?}", hw.keyed);
            assert!(term.text().contains("N0CALL-1 in its header"), "the ID requirement is stated");
        }
        // End of input declines too.
        let mut hw = MockHw::default();
        let checks = run(
            &ConfigState::Loaded("c".into()),
            &cfg,
            &mut hw,
            &mut ScriptTerm::new(&[]),
            &Options { transmit: true, level_seconds: 0.0 },
        );
        assert_eq!(by_name(&checks, "transmit loopback").status, Status::Skip);
        assert!(hw.keyed.is_empty());
    }

    #[test]
    fn transmit_check_keys_once_and_hears_its_own_id_frame_on_a_loopback() {
        let cfg = owner_like();
        let mut hw = MockHw::default();
        let mut term = ScriptTerm::new(&["yes"]);
        let checks = run(
            &ConfigState::Loaded("c".into()),
            &cfg,
            &mut hw,
            &mut term,
            &Options { transmit: true, level_seconds: 0.0 },
        );
        let c = by_name(&checks, "transmit loopback");
        assert_eq!(c.status, Status::Pass, "{}", c.line());
        assert!(c.detail.contains("own ID frame heard back"));
        assert_eq!(hw.keyed.len(), 1, "{:?}", hw.keyed);
        assert!(hw.keyed[0].contains("txdelay 300 ms"), "the profile's TXDelay: {}", hw.keyed[0]);
        // Nothing heard: a warning, not a failure.
        let mut hw = MockHw { heard: Some(vec![0.0; 96_000]), ..Default::default() };
        let checks = run(
            &ConfigState::Loaded("c".into()),
            &cfg,
            &mut hw,
            &mut ScriptTerm::new(&["yes"]),
            &Options { transmit: true, level_seconds: 0.0 },
        );
        assert_eq!(by_name(&checks, "transmit loopback").status, Status::Warn);
    }

    #[test]
    fn id_and_text_frames_decode() {
        let mut x = vec![0f32; 4800];
        x.extend(id_frame("N0CALL-1", Some("EM10")).unwrap());
        x.extend(vec![0f32; 9600]);
        x.extend(text_frame("N0CALL-1", "TXDELAY 300", 3).unwrap());
        x.extend(vec![0f32; 9600]);
        let f = decode(x, "N0RX-1").unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].header.ftype, cairn_link::FrameType::Beacon);
        assert_eq!(f[1].header.ftype, cairn_link::FrameType::Ui);
        assert!(f.iter().all(|f| f.header.src.to_string() == "N0CALL-1" && f.controls_ok == f.controls));
        assert_eq!(f[1].header.seq.0, 3);
    }
}
