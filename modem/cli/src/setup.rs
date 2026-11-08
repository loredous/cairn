//! `cairn-modem setup`: the guided setup (SPEC 13.6), a state machine over
//! a [`Terminal`] and the [`Hardware`], so it runs end to end on mocks.
//!
//! Steps, in order: station (callsign, grid) → audio device → rig profile
//! (suggested from the device; pick, create or none) → PTT (serial and HID
//! candidates found by USB ID) → real-time priority → receive level
//! (open-squelch noise near -20 dBFS, peaks under -3 dBFS) → PTT test →
//! transmit level (the level-setting tone for 70% deviation) → TXDelay
//! (measured with a listening station, or entered) → squelch and carrier
//! tail → identification → review and write.
//!
//! Every step that keys the PTT or plays audio goes through
//! [`confirm_keying`]: it says what will be sent, for how long, and what
//! identification it carries, and needs a typed `yes`. Declined, the step is
//! skipped and the profile's values stand. End of input at any prompt stops
//! the wizard without writing anything.

use std::path::PathBuf;

use crate::config::{FileConfig, ProfileMeta, Rig, RigProfile};
use crate::hw::{
    EndOfInput, Hardware, LevelReport, PttChoice, RT_HELP, Terminal, ask_default, ask_yes_no, confirm_keying,
};
use crate::txseq::TxTiming;

/// TXDelay values tried by the measurement, ms, longest first.
pub const TXDELAY_SWEEP_MS: [f64; 8] = [500.0, 400.0, 300.0, 250.0, 200.0, 150.0, 100.0, 50.0];
/// Margin added to the shortest TXDelay that decoded, ms.
pub const TXDELAY_MARGIN_MS: f64 = 50.0;
/// Level-setting tone length, s.
pub const TONE_S: f64 = 5.0;

/// A wizard step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Callsign and grid.
    Station,
    /// The sound card.
    Audio,
    /// The rig profile.
    Profile,
    /// The keying line.
    Ptt,
    /// Real-time priority.
    Realtime,
    /// Receive level.
    RxLevel,
    /// Key the PTT briefly.
    PttTest,
    /// Transmit drive with the level-setting tone.
    TxLevel,
    /// TXDelay and TXTail.
    TxDelay,
    /// Squelch and carrier tail.
    Squelch,
    /// An ID frame after unidentified test transmissions.
    Identify,
    /// Show and write the result.
    Review,
    /// Finished.
    Done,
}

/// What the wizard needs to know about the host.
#[derive(Clone, Debug)]
pub struct SetupEnv {
    /// Where the config goes.
    pub config_path: PathBuf,
    /// Where a new rig profile goes.
    pub user_rigs_dir: Option<PathBuf>,
    /// The known profiles.
    pub rigs: Vec<Rig>,
    /// The current config file (its values are the defaults offered).
    pub existing: FileConfig,
}

/// How the wizard ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Files written.
    Written {
        /// The config file.
        config: PathBuf,
        /// A new rig profile.
        profile: Option<PathBuf>,
    },
    /// The operator chose not to write.
    NotWritten,
    /// End of input: nothing written.
    Aborted,
}

/// The wizard.
pub struct Wizard {
    /// The next step.
    pub step: Step,
    env: SetupEnv,
    /// What the operator chose in this run.
    chosen: FileConfig,
    rig: Option<RigProfile>,
    new_rig: bool,
    unidentified: bool,
    outcome: Option<Outcome>,
}

fn list_choice(t: &mut dyn Terminal, prompt: &str, n: usize, default: &str) -> Result<Choice, EndOfInput> {
    let a = ask_default(t, prompt, default)?;
    Ok(match a.parse::<usize>() {
        Ok(k) if (1..=n).contains(&k) => Choice::Item(k - 1),
        _ => Choice::Text(a),
    })
}

enum Choice {
    Item(usize),
    Text(String),
}

fn level_advice(l: &LevelReport, signal: bool) -> String {
    let mut s = format!("RMS {:.1} dBFS, peak {:.1} dBFS", l.rms_dbfs, l.peak_dbfs);
    if l.clipped > 0 {
        s +=
            &format!(": CLIPPING ({} samples at full scale). Lower the radio's volume or the capture gain.", l.clipped);
    } else if signal {
        s += &match l.peak_dbfs {
            p if p > -3.0 => ": too hot (peaks above -3 dBFS). Lower the volume.".to_string(),
            p if p < -20.0 => ": weak. Raise the volume until peaks reach about -10 dBFS.".to_string(),
            _ => ": good (peaks near -10 dBFS, under -3).".to_string(),
        };
    } else {
        s += &match l.rms_dbfs {
            r if r < -35.0 => {
                ": very low. Is the squelch open? Raise the radio's volume (or the capture gain) toward -20 dBFS."
                    .to_string()
            }
            r if r < -26.0 => ": a little low; about -20 dBFS is the target.".to_string(),
            r if r > -14.0 || l.peak_dbfs > -3.0 => ": hot. Lower the volume toward -20 dBFS RMS.".to_string(),
            _ => ": good (target about -20 dBFS RMS).".to_string(),
        };
    }
    s
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl Wizard {
    /// A wizard at its first step.
    pub fn new(env: SetupEnv) -> Wizard {
        let rig = env
            .existing
            .station
            .rig
            .as_ref()
            .and_then(|n| env.rigs.iter().find(|r| &r.profile.profile.name == n))
            .map(|r| r.profile.clone());
        Wizard {
            step: Step::Station,
            env,
            chosen: FileConfig::default(),
            rig,
            new_rig: false,
            unidentified: false,
            outcome: None,
        }
    }

    /// The settings as they stand: profile, then the existing file, then
    /// this run's choices.
    pub fn draft(&self) -> FileConfig {
        let base = self.rig.as_ref().filter(|_| !self.new_rig).map(|r| r.overlay()).unwrap_or_default();
        let mut existing = self.env.existing.clone();
        if self.rig.as_ref().map(|r| r.profile.name.clone()) != existing.station.rig {
            // Another profile: the old file's interface settings no longer apply.
            existing.audio = Default::default();
            existing.ptt = Default::default();
            existing.timing = Default::default();
            existing.link.cap = None;
        }
        base.layer(&existing).layer(&self.chosen)
    }

    fn callsign(&self) -> String {
        self.draft().station.callsign.unwrap_or_default()
    }

    fn ptt(&self) -> PttChoice {
        PttChoice::from_section(&self.draft().ptt).unwrap_or(PttChoice {
            spec: "none".into(),
            line: cairn_ptt::SerialLineSpec::Dtr,
            invert: false,
            gpio: 3,
        })
    }

    fn device(&self) -> String {
        self.draft().audio.device.unwrap_or_else(|| "default".into())
    }

    fn timing(&self) -> TxTiming {
        let d = self.draft();
        TxTiming {
            txdelay_s: d.timing.txdelay_ms.unwrap_or(150.0) / 1e3,
            txtail_s: d.timing.txtail_ms.unwrap_or(10.0) / 1e3,
        }
    }

    /// Run the whole wizard.
    pub fn run(mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Outcome {
        while self.step != Step::Done {
            match self.advance(t, hw) {
                Ok(next) => self.step = next,
                Err(EndOfInput) => {
                    t.say("\nSetup stopped (end of input); nothing written.");
                    return Outcome::Aborted;
                }
            }
        }
        self.outcome.unwrap_or(Outcome::NotWritten)
    }

    /// Run the current step; the next one.
    pub fn advance(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        Ok(match self.step {
            Step::Station => self.station(t)?,
            Step::Audio => self.audio(t, hw)?,
            Step::Profile => self.profile(t, hw)?,
            Step::Ptt => self.ptt_step(t, hw)?,
            Step::Realtime => {
                t.say("\n== Real-time priority ==");
                let g = hw.rt_check();
                if g.is_realtime() {
                    t.say(&format!("Audio threads get {g}. Good."));
                } else {
                    t.say(&format!("Audio threads get {g}.\n{RT_HELP}"));
                }
                Step::RxLevel
            }
            Step::RxLevel => self.rx_level(t, hw)?,
            Step::PttTest => self.ptt_test(t, hw)?,
            Step::TxLevel => self.tx_level(t, hw)?,
            Step::TxDelay => self.txdelay(t, hw)?,
            Step::Squelch => self.squelch(t)?,
            Step::Identify => self.identify(t, hw)?,
            Step::Review => self.review(t, hw)?,
            Step::Done => Step::Done,
        })
    }

    fn station(&mut self, t: &mut dyn Terminal) -> Result<Step, EndOfInput> {
        t.say(
            "Cairn guided setup.\n\
             This picks the sound card, the PTT and a rig profile, checks the receive level, \
             helps set the transmit level and TXDelay, and writes the configuration.\n\
             Nothing is transmitted unless you type 'yes' when asked. Press Enter to take the value in brackets; \
             Ctrl-D stops without writing anything.\n\n== Station ==",
        );
        let old = self.env.existing.station.callsign.clone().unwrap_or_default();
        loop {
            let c = ask_default(t, "Your callsign and SSID (e.g. N0CALL-1)", &old)?.to_ascii_uppercase();
            let ok = crate::control::parse_call(&c).is_ok()
                && c.rsplit_once('-')
                    .map_or(&c[..], |(a, _)| a)
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '/');
            if ok && !c.is_empty() {
                self.chosen.station.callsign = Some(c);
                break;
            }
            t.say("That is not a callsign (CALL or CALL-SSID, SSID 0-15). Every header and ID block carries it.");
        }
        let old = self.env.existing.station.grid.clone().unwrap_or_default();
        loop {
            let g = ask_default(t, "Grid locator for the ID block (4 or 6 characters, Enter for none)", &old)?;
            if g.is_empty() || ((g.len() == 4 || g.len() == 6) && g.chars().all(|c| c.is_ascii_alphanumeric())) {
                self.chosen.station.grid = (!g.is_empty()).then_some(g);
                break;
            }
            t.say("4 or 6 characters, e.g. EM10 or EM10ab.");
        }
        Ok(Step::Audio)
    }

    fn audio(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say("\n== Audio device ==");
        let devs = match hw.audio_devices() {
            Ok(d) => d,
            Err(e) => {
                t.say(&format!("Could not list audio devices: {e}"));
                Vec::new()
            }
        };
        let usable: Vec<_> = devs.iter().filter(|d| d.input && d.output).collect();
        let current = self.draft().audio.device;
        let mut default = 0usize;
        for (i, d) in usable.iter().enumerate() {
            let rigs: Vec<&str> = self
                .env
                .rigs
                .iter()
                .filter(|r| r.profile.matches_device(&d.id) || r.profile.matches_device(&d.description))
                .map(|r| r.profile.profile.name.as_str())
                .collect();
            if default == 0 && (current.as_deref() == Some(d.id.as_str()) || (current.is_none() && !rigs.is_empty())) {
                default = i + 1;
            }
            let hint = if rigs.is_empty() { String::new() } else { format!("  [profile: {}]", rigs.join(", ")) };
            t.say(&format!("  {}) {}  {}{hint}", i + 1, d.id, d.description));
        }
        if usable.is_empty() {
            t.say("No device that both captures and plays at 48 kHz was found. Type a device name instead.");
        }
        let def = if default > 0 { default.to_string() } else { current.unwrap_or_else(|| "1".into()) };
        let dev = loop {
            match list_choice(t, "Device (number, or a name)", usable.len(), &def)? {
                Choice::Item(k) => break usable[k].id.clone(),
                Choice::Text(s) if !s.is_empty() && s.parse::<usize>().is_err() => break s,
                Choice::Text(_) => t.say("Pick one of the numbers, or type a device name."),
            }
        };
        t.say(&format!("Audio device: {dev}"));
        self.chosen.audio.device = Some(dev);
        Ok(Step::Profile)
    }

    fn profile(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say(
            "\n== Rig profile ==\nA rig profile holds an interface and radio's defaults (PTT, TXDelay, levels, width).",
        );
        let dev = self.device();
        let desc = hw
            .audio_devices()
            .ok()
            .and_then(|v| v.into_iter().find(|d| d.id == dev))
            .map(|d| d.description)
            .unwrap_or_default();
        let usb: Vec<String> = hw.ptt_candidates().iter().filter_map(|c| c.usb_text()).collect();
        let n = self.env.rigs.len();
        let mut suggested = None;
        for (i, r) in self.env.rigs.iter().enumerate() {
            let p = &r.profile;
            let hit = p.matches_device(&dev) || p.matches_device(&desc);
            let usb_hit = usb.iter().any(|u| p.matches_usb(u));
            if suggested.is_none() && hit {
                suggested = Some(i + 1);
            }
            t.say(&format!(
                "  {}) {:<12} {} ({}){}",
                i + 1,
                p.profile.name,
                p.profile.description,
                p.profile.status,
                if hit {
                    "  <- matches the device"
                } else if usb_hit {
                    "  <- its USB ID is plugged in"
                } else {
                    ""
                }
            ));
        }
        t.say(&format!("  {}) a new profile for this interface and radio\n  0) none", n + 1));
        let current = self
            .rig
            .as_ref()
            .and_then(|r| self.env.rigs.iter().position(|x| x.profile.profile.name == r.profile.name))
            .map(|i| i + 1);
        let def = current.or(suggested).map_or("0".to_string(), |i| i.to_string());
        loop {
            let a = ask_default(t, "Profile", &def)?;
            match a.parse::<usize>() {
                Ok(0) => {
                    self.rig = None;
                    self.new_rig = false;
                    break;
                }
                Ok(k) if k <= n => {
                    let r = self.env.rigs[k - 1].profile.clone();
                    t.say(&format!(
                        "{}: {}{}",
                        r.profile.name,
                        if r.profile.status == "measured" {
                            format!("measured on {}", r.profile.measured_on.as_deref().unwrap_or("?"))
                        } else {
                            "UNVERIFIED defaults: check every value".to_string()
                        },
                        r.profile.notes.as_deref().map(|n| format!("\n{}", n.trim())).unwrap_or_default()
                    ));
                    self.rig = Some(r);
                    self.new_rig = false;
                    break;
                }
                Ok(k) if k == n + 1 => {
                    let name = loop {
                        let s = ask_default(t, "Name for the new profile (lower case, digits, '-')", "my-rig")?;
                        if valid_name(&s) {
                            break s;
                        }
                        t.say("Use lower-case letters, digits and '-'.");
                    };
                    let description = ask_default(t, "One line describing it (interface and radio)", "")?;
                    self.rig = Some(RigProfile {
                        profile: ProfileMeta {
                            name,
                            description,
                            status: "unverified".into(),
                            device_match: vec![dev.clone()],
                            squelch: Some("open".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    self.new_rig = true;
                    break;
                }
                _ => t.say("Pick a number from the list."),
            }
        }
        Ok(Step::Ptt)
    }

    fn ptt_step(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say("\n== PTT ==\nHow the modem keys the transmitter (SPEC 13.3). Found on this host (nothing opened):");
        let cands = hw.ptt_candidates();
        for (i, c) in cands.iter().enumerate() {
            t.say(&format!(
                "  {}) {}  {}{}",
                i + 1,
                c.spec(),
                c.known().map_or(c.label.clone(), |k| k.name.to_string()),
                c.usb_text().map(|u| format!(" [{u}]")).unwrap_or_default()
            ));
        }
        if cands.is_empty() {
            t.say("  (no USB serial port or CM108-class HID device)");
        }
        t.say("  or type: vox, none, rigctld[:HOST:PORT], serial:PATH, cm108:PATH (cm108:auto)");
        let cur = self.draft().ptt.method;
        let def = cur
            .clone()
            .or_else(|| {
                let r = self.rig.as_ref()?;
                cands.iter().position(|c| c.usb_text().is_some_and(|u| r.matches_usb(&u))).map(|i| (i + 1).to_string())
            })
            .unwrap_or_else(|| "none".into());
        let spec = loop {
            let s = match list_choice(t, "PTT", cands.len(), &def)? {
                Choice::Item(k) => cands[k].spec(),
                Choice::Text(s) => s,
            };
            let probe = if s.ends_with(":auto") { "cm108:/dev/null".to_string() } else { s.clone() };
            match cairn_ptt::PttSpec::parse(&probe, cairn_ptt::SerialLineSpec::Dtr, false, 3) {
                Ok(_) => break s,
                Err(e) => t.say(&e),
            }
        };
        let d = self.draft().ptt;
        self.chosen.ptt.method = Some(spec.clone());
        match spec.split(':').next().unwrap_or("").to_ascii_lowercase().as_str() {
            "serial" => {
                let line = loop {
                    let l = ask_default(t, "Line that keys: dtr or rts", d.line.as_deref().unwrap_or("dtr"))?;
                    if matches!(l.to_ascii_lowercase().as_str(), "dtr" | "rts") {
                        break l.to_ascii_lowercase();
                    }
                };
                let invert = ask_yes_no(t, "Active low (keys with the line low)?", d.invert.unwrap_or(false))?;
                self.chosen.ptt.line = Some(line);
                self.chosen.ptt.invert = Some(invert);
            }
            "cm108" => {
                let g = loop {
                    let g = ask_default(t, "GPIO pin (1-8)", &d.gpio.unwrap_or(3).to_string())?;
                    match g.parse::<u8>() {
                        Ok(g) if (1..=8).contains(&g) => break g,
                        _ => t.say("1-8 (3 on most boards)."),
                    }
                };
                self.chosen.ptt.gpio = Some(g);
            }
            "vox" => {
                t.say(
                    "VOX keys on the audio: broadcast mode only; its delay and hang time break sessions (SPEC 13.3).",
                );
                let refuse = ask_yes_no(t, "Refuse incoming sessions (recommended with VOX)?", true)?;
                self.chosen.link.refuse = Some(refuse);
            }
            "none" => t.say("No PTT: receive only (frames would play unkeyed)."),
            _ => {}
        }
        t.say(&format!("PTT: {}", self.ptt().describe()));
        Ok(Step::Realtime)
    }

    fn rx_level(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say(
            "\n== Receive level ==\nOpen the radio's squelch fully, on a quiet frequency with no signal. Carrier \
             detect watches the receiver's noise above the signal band, so the squelch stays open in operation too.\n\
             Target (SPEC 13.5): open-squelch noise near -20 dBFS RMS, signal peaks never above -3 dBFS. \
             `run --rx-agc` holds it there by stepping the capture gain between frames; set the radio's volume \
             so the AGC starts near the middle of its range.",
        );
        let dev = self.device();
        let mut tries = 0;
        while tries < 6
            && ask_yes_no(t, if tries == 0 { "Measure the input for 3 s now?" } else { "Measure again?" }, tries == 0)?
        {
            tries += 1;
            match hw.measure_input(&dev, 3.0) {
                Ok(l) => t.say(&format!("Noise: {}", level_advice(&l, false))),
                Err(e) => {
                    t.say(&format!("Could not capture: {e}"));
                    break;
                }
            }
        }
        if ask_yes_no(
            t,
            "Can another station send data or the level-setting tone now (`cairn-modem tone` at their end) to measure a signal?",
            false,
        )? {
            match hw.measure_input(&dev, 5.0) {
                Ok(l) => t.say(&format!("Signal: {}", level_advice(&l, true))),
                Err(e) => t.say(&format!("Could not capture: {e}")),
            }
        }
        let control = self.draft().audio.rx_agc_control;
        match hw.capture_control(&dev, control.as_deref()) {
            Ok((name, g)) => {
                t.say(&format!("Capture gain control: '{name}' at {g:+.1} dB."));
                let on =
                    ask_yes_no(t, "Use receive AGC on it (run --rx-agc)?", self.draft().audio.rx_agc.unwrap_or(true))?;
                self.chosen.audio.rx_agc = Some(on);
                if on && control.is_none() {
                    self.chosen.audio.rx_agc_control = Some(name);
                }
            }
            Err(e) => t.say(&format!(
                "No capture gain control ({e}): the AGC cannot run; set the level by hand (`cairn-modem levels`)."
            )),
        }
        Ok(Step::PttTest)
    }

    fn ptt_test(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say("\n== PTT test ==");
        let ptt = self.ptt();
        if ptt.is_null() {
            t.say("No keying line to test.");
            return Ok(Step::TxLevel);
        }
        t.say("Keys the transmitter for 1 s with no audio, to check the wiring: watch the radio's TX indicator.");
        if let Some(ok) = confirm_keying(t, "PTT test (carrier only, no audio)", 1.0, &self.callsign(), false) {
            match hw.key_ptt(&ok, &ptt, 1.0) {
                Ok(r) => {
                    self.unidentified = true;
                    let rb = |v: Option<bool>| match v {
                        Some(true) => "keyed",
                        Some(false) => "unkeyed",
                        None => "no read-back",
                    };
                    t.say(&format!("Line read {} while keyed, {} after.", rb(r.keyed_read), rb(r.after_read)));
                    if !ask_yes_no(t, "Did the radio transmit (TX indicator on)?", true)? {
                        t.say("Check the line (dtr/rts), polarity (active low) or GPIO pin, and the cable; then rerun setup.");
                    }
                }
                Err(e) => t.say(&format!("PTT test failed: {e} (released)")),
            }
        }
        Ok(Step::TxLevel)
    }

    fn tx_level(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        let d = self.draft();
        t.say(&format!(
            "\n== Transmit level ==\nSPEC 7.1: set the drive so the level-setting tone (1000 Hz at the data's peak level) \
             gives 70% of full deviation: 3.5 kHz on a 25 kHz channel, 1.75 kHz on 12.5 kHz. The modem never \
             changes the output volume; you set it{}.\n\
             With a deviation meter or service monitor, read it directly. Without one:\n\
             - Listen on a second receiver (or have a peer run `cairn-modem levels`): raise the output a step at a \
             time while the tone plays. The received tone grows about 1 dB per dB until the transmitter's \
             limiter, then stops growing and distorts. Set the output about 3 dB below that knee: about 70%. \
             An SDR with an FM deviation display reads it too.\n\
             - The empirical fallback is the drive sweep: with a peer, run link tests (`probe CALL` in `run`) \
             while lowering the output 1-2 dB at a time. The SNR the peer reports improves while you were \
             overdriving the radio and stops improving once you are below its limiter: keep that level.",
            d.audio
                .output_control
                .as_ref()
                .map(|c| format!(
                    " on '{c}' (alsamixer){}",
                    d.audio.output_db.map(|v| format!("; the profile starts at {v:+.1} dB")).unwrap_or_default()
                ))
                .unwrap_or_default()
        ));
        let ptt = self.ptt();
        let timing = self.timing();
        let tone: Vec<f32> = crate::frames::level_tone(TONE_S).iter().map(|&v| v as f32).collect();
        let keyed = crate::txseq::keyed_seconds(tone.len(), &timing);
        let mut first = true;
        while ask_yes_no(t, if first { "Send the level-setting tone now?" } else { "Send it again?" }, false)? {
            first = false;
            let Some(ok) = confirm_keying(t, "the level-setting tone (1000 Hz)", keyed, &self.callsign(), false) else {
                break;
            };
            match hw.transmit(&ok, &self.device(), &ptt, timing, &tone) {
                Ok(m) => {
                    self.unidentified = true;
                    t.say(&format!("Tone sent: keyed {:.1} s.", m.keyed_ms / 1e3));
                }
                Err(e) => {
                    t.say(&format!("Transmit failed: {e} (PTT released)"));
                    break;
                }
            }
        }
        let cur = d.audio.output_db.map(|v| format!("{v}")).unwrap_or_default();
        loop {
            let a = ask_default(t, "Output (drive) control setting to record, dB (Enter to keep)", &cur)?;
            if a.is_empty() {
                break;
            }
            match a.parse::<f64>() {
                Ok(v) if (-100.0..=30.0).contains(&v) => {
                    self.chosen.audio.output_db = Some(v);
                    break;
                }
                _ => t.say("A number in dB, e.g. -9.5."),
            }
        }
        Ok(Step::TxDelay)
    }

    fn txdelay(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        let d = self.draft();
        let cur = d.timing.txdelay_ms.unwrap_or(150.0);
        t.say(&format!(
            "\n== TXDelay ==\nTXDelay is PTT to the first audio sample (SPEC 7.3, 20-500 ms): it must cover your \
             transmitter's rise and the far receiver opening up. Too short loses the preamble and every frame; \
             too long wastes air time on every turn. Now {cur:.0} ms{}.",
            self.rig.as_ref().map(|r| format!(" (profile {})", r.profile.name)).unwrap_or_default()
        ));
        let ptt = self.ptt();
        let mut value = cur;
        let call = self.callsign();
        if !ptt.is_null()
            && ask_yes_no(
                t,
                "Measure it with a second station listening (it runs `cairn-modem listen` and tells you which frames decoded)?",
                false,
            )?
        {
            let frames: Vec<(f64, Vec<f32>)> = TXDELAY_SWEEP_MS
                .iter()
                .enumerate()
                .filter_map(|(i, &v)| {
                    crate::selftest::text_frame(&call, &format!("TXDELAY {v:.0}"), i as u16).ok().map(|a| (v, a))
                })
                .collect();
            let tail = d.timing.txtail_ms.unwrap_or(10.0) / 1e3;
            let keyed: f64 = frames
                .iter()
                .map(|(v, a)| crate::txseq::keyed_seconds(a.len(), &TxTiming { txdelay_s: v / 1e3, txtail_s: tail }))
                .sum();
            t.say(&format!(
                "{} UI frames, TXDelay {} ms in turn, each saying \"TXDELAY n\". The listening station notes which decode.",
                frames.len(),
                TXDELAY_SWEEP_MS.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(", ")
            ));
            if let Some(ok) = confirm_keying(t, &format!("{} test frames", frames.len()), keyed, &call, true) {
                for (v, a) in &frames {
                    match hw.transmit(&ok, &self.device(), &ptt, TxTiming { txdelay_s: v / 1e3, txtail_s: tail }, a) {
                        Ok(m) => {
                            t.say(&format!("sent TXDELAY {v:.0} (measured TXDelay {:.0} ms at the DAC)", m.txdelay_ms))
                        }
                        Err(e) => {
                            t.say(&format!("Transmit failed: {e} (PTT released)"));
                            break;
                        }
                    }
                }
                loop {
                    let a = ask_default(
                        t,
                        "Shortest TXDelay whose frame decoded, with every longer one decoded too (ms; Enter if none)",
                        "",
                    )?;
                    if a.is_empty() {
                        t.say("None decoded: check levels and the PTT first; TXDelay stays as it was.");
                        break;
                    }
                    match a.parse::<f64>() {
                        Ok(v) if (0.0..=1000.0).contains(&v) => {
                            value = (v + TXDELAY_MARGIN_MS).min(1000.0);
                            t.say(&format!("Recommended: {v:.0} + {TXDELAY_MARGIN_MS:.0} ms margin = {value:.0} ms."));
                            break;
                        }
                        _ => t.say("A number of milliseconds."),
                    }
                }
            }
        }
        loop {
            let a = ask_default(t, "TXDelay to use, ms", &format!("{value:.0}"))?;
            match a.parse::<f64>() {
                Ok(v) if (0.0..=1000.0).contains(&v) => {
                    if !(20.0..=500.0).contains(&v) {
                        t.say("Note: SPEC 7.3's range is 20-500 ms.");
                    }
                    self.chosen.timing.txdelay_ms = Some(v);
                    break;
                }
                _ => t.say("0-1000 ms."),
            }
        }
        let tail = d.timing.txtail_ms.unwrap_or(10.0);
        loop {
            let a = ask_default(t, "TXTail (last sample to PTT release), ms", &format!("{tail:.0}"))?;
            match a.parse::<f64>() {
                Ok(v) if (0.0..=200.0).contains(&v) => {
                    self.chosen.timing.txtail_ms = Some(v);
                    break;
                }
                _ => t.say("0-200 ms (SPEC 7.3: 0-50)."),
            }
        }
        Ok(Step::Squelch)
    }

    fn squelch(&mut self, t: &mut dyn Terminal) -> Result<Step, EndOfInput> {
        t.say(
            "\n== Squelch and carrier tail ==\nCarrier detect (SPEC 7.2) uses the receiver's noise: with the squelch \
             closed the modem hears silence, cannot tell a busy channel from a free one, and the squelch's own \
             opening delay eats the start of every frame.",
        );
        if !ask_yes_no(t, "Will the squelch stay open in operation?", true)? {
            t.say("WARNING: carrier detect will not work and frames may lose their start. Open the squelch (volume low is fine).");
        }
        match self.rig.as_ref().and_then(|r| r.profile.carrier_tail_ms) {
            Some(ms) => t.say(&format!(
                "This profile's radios kept a carrier tail of about {ms:.0} ms after PTT release. The far end sees \
                 that as a busy channel and waits it out; it lengthens every turnaround, so a radio with a shorter \
                 tail (or a squelch-tail-elimination setting, where the radio has one) is faster."
            )),
            None => t.say(
                "Some radios keep transmitting a carrier for a fraction of a second after PTT release (a carrier \
                 or squelch tail; about 0.7 s on the handhelds of the aioc-ht profile). The far end waits it out on \
                 every turn: a shorter tail is faster.",
            ),
        }
        Ok(if self.unidentified { Step::Identify } else { Step::Review })
    }

    fn identify(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        t.say("\n== Identification ==\nThe PTT test and the tone did not carry your callsign.");
        let call = self.callsign();
        let d = self.draft();
        match crate::selftest::id_frame(&call, d.station.grid.as_deref()) {
            Ok(audio) => {
                let keyed = crate::txseq::keyed_seconds(audio.len(), &self.timing());
                match confirm_keying(t, "an ID frame (a BEACON with the ID block)", keyed, &call, true) {
                    Some(ok) => match hw.transmit(&ok, &self.device(), &self.ptt(), self.timing(), &audio) {
                        Ok(_) => {
                            self.unidentified = false;
                            t.say("ID frame sent.");
                        }
                        Err(e) => t.say(&format!("Transmit failed: {e} (PTT released). Identify by voice.")),
                    },
                    None => t.say("Identify by voice (or CW) now, as your licence requires."),
                }
            }
            Err(e) => t.say(&format!("Could not build an ID frame: {e}. Identify by voice.")),
        }
        Ok(Step::Review)
    }

    /// The config file to write (only what differs from the profile) and
    /// the new profile, if one was made.
    pub fn result(&self) -> (FileConfig, Option<RigProfile>) {
        let draft = self.draft();
        let mut new = None;
        let base = match (&self.rig, self.new_rig) {
            (Some(r), false) => r.overlay(),
            (Some(r), true) => {
                let mut p = r.clone();
                p.audio = draft.audio.clone();
                p.ptt = draft.ptt.clone();
                p.timing = draft.timing.clone();
                p.link.cap = draft.link.cap;
                p.link.refuse = draft.link.refuse;
                let base = p.overlay();
                new = Some(p);
                base
            }
            (None, _) => FileConfig::default(),
        };
        let mut cfg = draft.minus(&base);
        cfg.station = draft.station.clone();
        cfg.station.rig = self.rig.as_ref().map(|r| r.profile.name.clone());
        (cfg, new)
    }

    fn review(&mut self, t: &mut dyn Terminal, hw: &mut dyn Hardware) -> Result<Step, EndOfInput> {
        let (cfg, new) = self.result();
        let path = self.env.config_path.clone();
        t.say("\n== Review ==");
        let profile_path = new
            .as_ref()
            .and_then(|p| self.env.user_rigs_dir.as_ref().map(|d| d.join(format!("{}.toml", p.profile.name))));
        if let (Some(p), Some(pp)) = (&new, &profile_path) {
            t.say(&format!("New rig profile {}:\n{}", pp.display(), p.to_toml()));
        }
        t.say(&format!("Configuration {}:\n{}", path.display(), cfg.to_toml()));
        if !ask_yes_no(t, "Write it?", true)? {
            t.say("Not written.");
            self.outcome = Some(Outcome::NotWritten);
            return Ok(Step::Done);
        }
        let mut written_profile = None;
        if let (Some(p), Some(pp)) = (&new, &profile_path) {
            match hw.write_file(pp, &p.to_toml()) {
                Ok(_) => written_profile = Some(pp.clone()),
                Err(e) => t.say(&format!("Could not write {}: {e}", pp.display())),
            }
        }
        match hw.write_file(&path, &cfg.to_toml()) {
            Ok(backup) => {
                if let Some(b) = backup {
                    t.say(&format!("Kept the previous file as {}.", b.display()));
                }
                t.say(&format!(
                    "Wrote {}.\nNext: `cairn-modem selftest` checks it all without transmitting; then `cairn-modem run`. \
                     See docs/operator-guide.md for a first contact and unattended operation.",
                    path.display()
                ));
                self.outcome = Some(Outcome::Written { config: path, profile: written_profile });
            }
            Err(e) => {
                t.say(&format!("Could not write {}: {e}", path.display()));
                self.outcome = Some(Outcome::NotWritten);
            }
        }
        Ok(Step::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{PttCandidate, PttKind};
    use crate::hw::mock::{MockHw, ScriptTerm};
    use std::path::Path;

    fn env(existing: FileConfig) -> SetupEnv {
        SetupEnv {
            config_path: "/home/op/.config/cairn/cairn.toml".into(),
            user_rigs_dir: Some("/home/op/.config/cairn/rigs".into()),
            rigs: crate::config::all_rigs(None).0,
            existing,
        }
    }

    fn aioc_hw() -> MockHw {
        MockHw {
            candidates: vec![PttCandidate {
                kind: PttKind::Serial,
                path: "/dev/serial/by-id/usb-AIOC_All-In-One-Cable_1234-if00".into(),
                usb: Some((0x1209, 0x7388)),
                label: "usb-AIOC_All-In-One-Cable_1234-if00".into(),
            }],
            ..Default::default()
        }
    }

    fn run(answers: &[&str], hw: &mut MockHw, existing: FileConfig) -> (Outcome, ScriptTerm) {
        let mut t = ScriptTerm::new(answers);
        let o = Wizard::new(env(existing)).run(&mut t, hw);
        (o, t)
    }

    fn written(hw: &MockHw) -> FileConfig {
        let (p, text) = hw.written.last().expect("a file written");
        assert_eq!(p, Path::new("/home/op/.config/cairn/cairn.toml"));
        FileConfig::parse(text).unwrap()
    }

    #[test]
    fn full_run_with_every_transmission_confirmed() {
        let mut hw = aioc_hw();
        let answers = [
            "n0call-1", // callsign
            "EM10",     // grid
            "",         // device: the AIOC (matches a profile)
            "",         // profile: aioc-ht (suggested)
            "",         // PTT: the profile's serial:All-In-One-Cable
            "",         // line dtr
            "",         // active low? no
            "",         // measure noise? yes
            "",         // measure again? no
            "",         // measure a signal? no
            "",         // AGC? yes
            "yes",      // PTT test
            "",         // did it transmit? yes
            "y",        // send the tone?
            "yes",      // confirm
            "",         // again? no
            "-10",      // output dB
            "y",        // measure TXDelay?
            "yes",      // confirm the sweep
            "200",      // shortest that decoded
            "",         // TXDelay: the recommendation (250)
            "",         // TXTail: 20
            "",         // squelch open? yes
            "yes",      // ID frame
            "",         // write? yes
        ];
        let (o, t) = run(&answers, &mut hw, FileConfig::default());
        let text = t.text();
        assert_eq!(o, Outcome::Written { config: "/home/op/.config/cairn/cairn.toml".into(), profile: None }, "{text}");
        // Keyed: the PTT test, the tone, eight sweep frames, the ID frame.
        assert_eq!(hw.keyed.len(), 1 + 1 + 8 + 1, "{:?}", hw.keyed);
        assert!(hw.keyed[0].starts_with("key_ptt serial:All-In-One-Cable"));
        assert!(hw.keyed[1].contains("txdelay 300 ms") && hw.keyed[1].contains("5.00 s"), "{}", hw.keyed[1]);
        assert!(hw.keyed[2].contains("txdelay 500 ms") && hw.keyed[9].contains("txdelay 50 ms"));
        assert!(text.contains("Recommended: 200 + 50 ms margin = 250 ms"));
        assert!(text.contains("70% of full deviation") && text.contains("drive sweep"));
        assert!(text.contains("'AIOC Audio Out Volume'"));
        assert!(text.contains("about 700 ms"), "the profile's carrier tail");
        let cfg = written(&hw);
        assert_eq!(cfg.station.callsign.as_deref(), Some("N0CALL-1"));
        assert_eq!(cfg.station.grid.as_deref(), Some("EM10"));
        assert_eq!(cfg.station.rig.as_deref(), Some("aioc-ht"));
        // Only what differs from the profile.
        assert_eq!(cfg.timing.txdelay_ms, Some(250.0));
        assert_eq!(cfg.timing.txtail_ms, None, "20 ms is the profile's");
        assert_eq!(cfg.audio.output_db, Some(-10.0));
        assert_eq!(cfg.audio.rx_agc, Some(true));
        assert_eq!(cfg.audio.device, None, "the profile's device");
        assert_eq!(cfg.ptt, Default::default());
        // The effective result has the profile's values.
        let e = crate::config::effective(&cfg, None, None).unwrap();
        assert_eq!(e.ptt.method.as_deref(), Some("serial:All-In-One-Cable"));
        assert_eq!(e.link.cap, Some(32));
    }

    #[test]
    fn declining_every_transmission_keys_nothing_and_still_writes() {
        let mut hw = aioc_hw();
        let answers = [
            "N0CALL-1", "", "", "", "", "", "",   // station, audio, profile, PTT
            "n",  // no noise measurement
            "",   // no signal
            "n",  // no AGC
            "y",  // PTT test prompt: 'y' is not 'yes'
            "y",  // send the tone? yes...
            "no", // ...but decline at the confirmation
            "",   // output dB: keep
            "y",  // measure TXDelay?
            "",   // decline the sweep (empty line)
            "",   // TXDelay: keep 300
            "",   // TXTail
            "",   // squelch
            "",   // write
        ];
        let (o, t) = run(&answers, &mut hw, FileConfig::default());
        let text = t.text();
        assert!(matches!(o, Outcome::Written { .. }), "{text}");
        assert!(hw.keyed.is_empty(), "keyed: {:?}", hw.keyed);
        assert_eq!(text.matches("Skipped: nothing transmitted.").count(), 3);
        assert!(text.contains("does not carry your callsign"), "the ID requirement is stated");
        assert!(!text.contains("== Identification =="), "nothing to identify");
        let cfg = written(&hw);
        assert_eq!(cfg.station.rig.as_deref(), Some("aioc-ht"));
        assert_eq!(cfg.timing, Default::default(), "the profile's timing stands");
        assert_eq!(cfg.audio.rx_agc, Some(false));
        assert!(!hw.calls.iter().any(|c| c.starts_with("measure_input")));
    }

    #[test]
    fn end_of_input_writes_nothing() {
        for cut in [0, 3, 12] {
            let mut hw = aioc_hw();
            let all = ["N0CALL-1", "", "", "", "", "", "", "n", "", "n", "yes", ""];
            let (o, t) = run(&all[..cut.min(all.len())], &mut hw, FileConfig::default());
            assert_eq!(o, Outcome::Aborted, "cut at {cut}: {}", t.text());
            assert!(hw.written.is_empty());
        }
    }

    #[test]
    fn bad_answers_are_asked_again() {
        let mut hw = aioc_hw();
        let answers = [
            "not a call!",
            "N0CALL-99",
            "N0CALL-2", // callsign twice wrong
            "EM1",
            "", // grid wrong then none
            "9",
            "2", // device out of range is taken as a name? no: "9" is a number, asked again
            "7",
            "0", // profile out of range, then none
            "serial:",
            "vox",
            "", // bad PTT spec, then VOX; refuse sessions
            "n",
            "",  // no measurement, no signal; no capture control question (AGC ok -> asked)
            "n", // AGC
            "n", // tone
            "",  // output dB
            "abc",
            "120",
            "",  // TXDelay: bad, then 120, TXTail default
            "n", // squelch closed
            "n", // do not write
        ];
        let (o, t) = run(&answers, &mut hw, FileConfig::default());
        let text = t.text();
        assert_eq!(o, Outcome::NotWritten, "{text}");
        assert!(text.contains("That is not a callsign"));
        assert!(text.contains("4 or 6 characters"));
        assert!(text.contains("Pick one of the numbers"));
        assert!(text.contains("Pick a number from the list"));
        assert!(text.contains("serial PTT needs a path"));
        assert!(text.contains("broadcast mode only"));
        assert!(text.contains("No keying line to test"));
        assert!(text.contains("WARNING: carrier detect will not work"));
        assert!(!text.contains("Measure it with a second station"), "no sweep without a PTT line");
        assert!(hw.keyed.is_empty() && hw.written.is_empty());
    }

    #[test]
    fn a_new_profile_is_written_with_the_choices() {
        let mut hw = MockHw::default();
        let answers = [
            "N0CALL-3",
            "",               // station
            "2",              // device: default
            "5",              // a new profile
            "bench-ht",       // its name
            "bench handheld", // description
            "cm108:auto",
            "", // PTT, GPIO 3
            "n",
            "",
            "", // levels; AGC yes
            "", // PTT test: decline
            "n",
            "", // no tone; output
            "",
            "180",
            "", // no sweep; TXDelay 180; TXTail
            "",
            "", // squelch; write
        ];
        let (o, t) = run(&answers, &mut hw, FileConfig::default());
        let text = t.text();
        assert_eq!(
            o,
            Outcome::Written {
                config: "/home/op/.config/cairn/cairn.toml".into(),
                profile: Some("/home/op/.config/cairn/rigs/bench-ht.toml".into())
            },
            "{text}"
        );
        let p = RigProfile::parse(&hw.written[0].1).unwrap();
        assert_eq!(p.profile.name, "bench-ht");
        assert_eq!(p.profile.status, "unverified");
        assert_eq!(p.audio.device.as_deref(), Some("default"));
        assert_eq!(p.ptt.method.as_deref(), Some("cm108:auto"));
        assert_eq!(p.ptt.gpio, Some(3));
        assert_eq!(p.timing.txdelay_ms, Some(180.0));
        let cfg = written(&hw);
        assert_eq!(cfg.station.rig.as_deref(), Some("bench-ht"));
        assert_eq!(cfg.timing, Default::default(), "in the profile");
        assert!(hw.keyed.is_empty());
    }

    #[test]
    fn existing_values_are_the_defaults() {
        let existing = FileConfig::parse(
            "[station]\ncallsign = \"N0OLD-4\"\ngrid = \"FN31\"\nrig = \"aioc-ht\"\n[timing]\ntxdelay_ms = 220.0\n",
        )
        .unwrap();
        let mut hw = aioc_hw();
        // Enter everywhere: keep everything; decline every transmission.
        let answers = ["", "", "", "", "", "", "", "n", "", "", "", "n", "", "", "", "", "", ""];
        let (o, t) = run(&answers, &mut hw, existing);
        assert!(matches!(o, Outcome::Written { .. }), "{}", t.text());
        let cfg = written(&hw);
        assert_eq!(cfg.station.callsign.as_deref(), Some("N0OLD-4"));
        assert_eq!(cfg.station.grid.as_deref(), Some("FN31"));
        assert_eq!(cfg.timing.txdelay_ms, Some(220.0), "kept from the old file");
        assert!(hw.keyed.is_empty());
    }
}
