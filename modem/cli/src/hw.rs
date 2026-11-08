//! The terminal and hardware seen by `setup` and `selftest`, as traits so
//! both run end to end against mocks in tests (the binary supplies the real
//! sound card, PTT and terminal).
//!
//! Keying is gated by type: every [`Hardware`] method that keys the PTT or
//! plays audio takes a [`Confirmed`], and the only way to get one is
//! [`confirm_keying`], which states what will be sent, for how long, and the
//! station-identification requirement, and needs the operator to type `yes`
//! at run time.

use std::path::{Path, PathBuf};

use cairn_audio::RtGrant;
use cairn_ptt::SerialLineSpec;

use crate::detect::PttCandidate;
use crate::txseq::TxTiming;

/// The operator's terminal: plain lines out and in.
pub trait Terminal {
    /// Print text (one or more lines).
    fn say(&mut self, text: &str);
    /// Print `prompt` and read one line (trimmed); None at end of input.
    fn ask(&mut self, prompt: &str) -> Option<String>;
}

/// The operator ended the input (Ctrl-D): stop without writing anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndOfInput;

/// Ask with a default shown in brackets; an empty answer takes it.
pub fn ask_default(t: &mut dyn Terminal, prompt: &str, default: &str) -> Result<String, EndOfInput> {
    let p = if default.is_empty() { format!("{prompt}: ") } else { format!("{prompt} [{default}]: ") };
    let a = t.ask(&p).ok_or(EndOfInput)?;
    Ok(if a.is_empty() { default.to_string() } else { a })
}

/// A yes/no question; an empty answer takes `default`.
pub fn ask_yes_no(t: &mut dyn Terminal, prompt: &str, default: bool) -> Result<bool, EndOfInput> {
    loop {
        let a = t.ask(&format!("{prompt} [{}]: ", if default { "Y/n" } else { "y/N" })).ok_or(EndOfInput)?;
        match a.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => t.say("Please answer y or n."),
        }
    }
}

/// Proof that the operator confirmed a transmission at run time.
#[derive(Debug)]
pub struct Confirmed {
    _private: (),
}

/// State what is about to go out and ask for an explicit `yes`. Anything
/// else (including `y`, an empty line or end of input) declines.
pub fn confirm_keying(
    t: &mut dyn Terminal,
    what: &str,
    keyed_s: f64,
    callsign: &str,
    identifies: bool,
) -> Option<Confirmed> {
    let id = if identifies {
        format!("Every frame carries your callsign {callsign} in its header and the ASCII ID block (SPEC 8.3, B.6).")
    } else {
        format!(
            "This transmission does not carry your callsign. You must identify ({callsign}) at the end of your \
             transmissions and at least every 10 minutes (US: Part 97.119; check your administration's rules): \
             setup offers an ID frame when the transmit steps are done, or identify by voice."
        )
    };
    t.say(&format!(
        "\n*** TRANSMIT: {what}, keyed about {keyed_s:.1} s in total. ***\n\
         Use a dummy load, or a frequency you are licensed to use and that is clear. {id}"
    ));
    match t.ask("Type 'yes' to transmit, anything else to skip: ") {
        Some(a) if a == "yes" => Some(Confirmed { _private: () }),
        _ => {
            t.say("Skipped: nothing transmitted.");
            None
        }
    }
}

/// A PTT as the CLI describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PttChoice {
    /// The `--ptt` spec (`none`, `vox`, `serial:PATH`, `cm108:PATH`, …).
    pub spec: String,
    /// Serial line that keys.
    pub line: SerialLineSpec,
    /// Active low.
    pub invert: bool,
    /// CM108 GPIO pin.
    pub gpio: u8,
}

impl PttChoice {
    /// From the config's `[ptt]` section (defaults: none, DTR, active
    /// high, GPIO 3).
    pub fn from_section(p: &crate::config::PttSection) -> Result<PttChoice, String> {
        Ok(PttChoice {
            spec: p.method.clone().unwrap_or_else(|| "none".into()),
            line: p.line.as_deref().unwrap_or("dtr").parse()?,
            invert: p.invert.unwrap_or(false),
            gpio: p.gpio.unwrap_or(cairn_ptt::CM108_DEFAULT_GPIO),
        })
    }

    /// True for `none` and `vox` (no keying line).
    pub fn is_null(&self) -> bool {
        matches!(self.spec.to_ascii_lowercase().as_str(), "none" | "vox")
    }

    /// One line.
    pub fn describe(&self) -> String {
        match self.spec.split(':').next().unwrap_or("") {
            "serial" => format!(
                "{} ({} keys{})",
                self.spec,
                if self.line == SerialLineSpec::Dtr { "DTR" } else { "RTS" },
                if self.invert { ", active low" } else { "" }
            ),
            "cm108" => format!("{} (GPIO {})", self.spec, self.gpio),
            _ => self.spec.clone(),
        }
    }
}

/// An audio device as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDev {
    /// Backend id (ALSA PCM name).
    pub id: String,
    /// Description.
    pub description: String,
    /// Captures at 48 kHz.
    pub input: bool,
    /// Plays at 48 kHz.
    pub output: bool,
}

/// Input levels over a measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelReport {
    /// RMS without DC, dBFS.
    pub rms_dbfs: f64,
    /// Peak, dBFS.
    pub peak_dbfs: f64,
    /// Samples at full scale.
    pub clipped: u64,
}

/// A PTT test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyReport {
    /// What the line read while keyed (None: no read-back).
    pub keyed_read: Option<bool>,
    /// What it read after release.
    pub after_read: Option<bool>,
}

/// One keyed transmission as measured at the DAC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxMeasure {
    /// PTT on time, ms.
    pub keyed_ms: f64,
    /// Measured TXDelay, ms.
    pub txdelay_ms: f64,
    /// Measured TXTail, ms.
    pub txtail_ms: f64,
    /// What the PTT line read while keyed (None: no read-back).
    pub keyed_read: Option<bool>,
    /// What it read after release.
    pub after_read: Option<bool>,
}

/// The sound card, the PTT and the host, for setup and the self-test.
/// Methods that only look never key; methods that key take a [`Confirmed`].
pub trait Hardware {
    /// Audio devices (opening each briefly to read its formats).
    fn audio_devices(&mut self) -> Result<Vec<AudioDev>, String>;
    /// Serial and HID PTT candidates (sysfs only).
    fn ptt_candidates(&mut self) -> Vec<PttCandidate>;
    /// Open the input, see it run, close it; a one-line description.
    fn open_input(&mut self, device: &str) -> Result<String, String>;
    /// Open the output, see it run, close it; a one-line description.
    fn open_output(&mut self, device: &str) -> Result<String, String>;
    /// Measure the input for `seconds`.
    fn measure_input(&mut self, device: &str, seconds: f64) -> Result<LevelReport, String>;
    /// Open the PTT unkeyed, read the line back, close it: (description,
    /// read-back). Never keys.
    fn ptt_idle(&mut self, ptt: &PttChoice) -> Result<(String, Option<bool>), String>;
    /// What a real-time request from an audio thread gets.
    fn rt_check(&mut self) -> RtGrant;
    /// The input's capture-gain mixer control: (name, gain dB).
    fn capture_control(&mut self, device: &str, control: Option<&str>) -> Result<(String, f64), String>;
    /// True when the TCP address can be bound (nothing else listens there).
    fn port_free(&mut self, addr: &str) -> Result<(), String>;
    /// True when `path` names an executable file.
    fn executable(&mut self, path: &str) -> bool;
    /// Write a file (keeping a `.bak` of the old one); the backup's path.
    fn write_file(&mut self, path: &Path, text: &str) -> Result<Option<PathBuf>, String>;
    /// Key the PTT for `seconds` without audio.
    fn key_ptt(&mut self, ok: &Confirmed, ptt: &PttChoice, seconds: f64) -> Result<KeyReport, String>;
    /// Key and play `audio` (48 kHz) with TXDelay and TXTail.
    fn transmit(
        &mut self,
        ok: &Confirmed,
        device: &str,
        ptt: &PttChoice,
        timing: TxTiming,
        audio: &[f32],
    ) -> Result<TxMeasure, String>;
    /// As [`Hardware::transmit`], capturing the input from before keying
    /// until `listen_s` after release; returns the capture too.
    fn transmit_and_listen(
        &mut self,
        ok: &Confirmed,
        device: &str,
        ptt: &PttChoice,
        timing: TxTiming,
        audio: &[f32],
        listen_s: f64,
    ) -> Result<(TxMeasure, Vec<f32>), String>;
}

/// The real-time instructions printed when a request was refused.
pub const RT_HELP: &str = "\
Real-time priority keeps the audio threads from losing samples under load. Either:
  - allow it for your user (any distribution):
      echo '@audio - rtprio 95' | sudo tee /etc/security/limits.d/95-cairn-audio.conf
      sudo usermod -aG audio \"$USER\"     # then log out and back in
  - or run rtkit (desktops with PipeWire usually do): systemctl status rtkit-daemon
The modem asks on its own every run; nothing here changes your system.
See docs/bench-testing.md, \"Real-time priority\".";

#[cfg(test)]
pub(crate) mod mock {
    //! A scripted terminal and a recording mock of the hardware.

    use super::*;
    use std::collections::VecDeque;

    /// Answers from a script; every line said and asked is recorded.
    #[derive(Default)]
    pub struct ScriptTerm {
        pub answers: VecDeque<String>,
        pub transcript: Vec<String>,
    }

    impl ScriptTerm {
        pub fn new(answers: &[&str]) -> Self {
            ScriptTerm { answers: answers.iter().map(|s| s.to_string()).collect(), transcript: Vec::new() }
        }
        pub fn text(&self) -> String {
            self.transcript.join("\n")
        }
    }

    impl Terminal for ScriptTerm {
        fn say(&mut self, text: &str) {
            self.transcript.push(text.to_string());
        }
        fn ask(&mut self, prompt: &str) -> Option<String> {
            let a = self.answers.pop_front();
            self.transcript.push(format!("{prompt}{}", a.as_deref().unwrap_or("<EOF>")));
            a
        }
    }

    /// Records every call; keying calls go to `keyed`.
    pub struct MockHw {
        pub devices: Vec<AudioDev>,
        pub candidates: Vec<PttCandidate>,
        pub levels: VecDeque<LevelReport>,
        pub idle_read: Option<bool>,
        pub rt: RtGrant,
        pub control: Result<(String, f64), String>,
        pub busy_ports: Vec<String>,
        pub fail_open: bool,
        /// Audio returned by transmit_and_listen (None: the transmitted audio, as a loopback).
        pub heard: Option<Vec<f32>>,
        pub calls: Vec<String>,
        pub keyed: Vec<String>,
        pub written: Vec<(PathBuf, String)>,
    }

    impl Default for MockHw {
        fn default() -> Self {
            MockHw {
                devices: vec![
                    AudioDev {
                        id: "front:CARD=AllInOneCable,DEV=0".into(),
                        description: "All-In-One-Cable, USB Audio".into(),
                        input: true,
                        output: true,
                    },
                    AudioDev {
                        id: "default".into(),
                        description: "Default ALSA device".into(),
                        input: true,
                        output: true,
                    },
                ],
                candidates: Vec::new(),
                levels: VecDeque::new(),
                idle_read: Some(false),
                rt: RtGrant::Realtime { fifo: true, priority: 60, via: cairn_audio::rt::Via::Direct },
                control: Ok(("AIOC Audio In".into(), 0.0)),
                busy_ports: Vec::new(),
                fail_open: false,
                heard: None,
                calls: Vec::new(),
                keyed: Vec::new(),
                written: Vec::new(),
            }
        }
    }

    impl Hardware for MockHw {
        fn audio_devices(&mut self) -> Result<Vec<AudioDev>, String> {
            self.calls.push("audio_devices".into());
            Ok(self.devices.clone())
        }
        fn ptt_candidates(&mut self) -> Vec<PttCandidate> {
            self.calls.push("ptt_candidates".into());
            self.candidates.clone()
        }
        fn open_input(&mut self, device: &str) -> Result<String, String> {
            self.calls.push(format!("open_input {device}"));
            if self.fail_open { Err("device busy".into()) } else { Ok(format!("input {device} (mono i16)")) }
        }
        fn open_output(&mut self, device: &str) -> Result<String, String> {
            self.calls.push(format!("open_output {device}"));
            if self.fail_open { Err("device busy".into()) } else { Ok(format!("output {device} (mono i16)")) }
        }
        fn measure_input(&mut self, device: &str, seconds: f64) -> Result<LevelReport, String> {
            self.calls.push(format!("measure_input {device} {seconds}"));
            Ok(self.levels.pop_front().unwrap_or(LevelReport { rms_dbfs: -21.0, peak_dbfs: -9.0, clipped: 0 }))
        }
        fn ptt_idle(&mut self, ptt: &PttChoice) -> Result<(String, Option<bool>), String> {
            self.calls.push(format!("ptt_idle {}", ptt.spec));
            Ok((ptt.describe(), self.idle_read))
        }
        fn rt_check(&mut self) -> RtGrant {
            self.calls.push("rt_check".into());
            self.rt.clone()
        }
        fn capture_control(&mut self, device: &str, _control: Option<&str>) -> Result<(String, f64), String> {
            self.calls.push(format!("capture_control {device}"));
            self.control.clone()
        }
        fn port_free(&mut self, addr: &str) -> Result<(), String> {
            self.calls.push(format!("port_free {addr}"));
            if self.busy_ports.iter().any(|p| p == addr) { Err("address in use".into()) } else { Ok(()) }
        }
        fn executable(&mut self, path: &str) -> bool {
            !path.contains("missing")
        }
        fn write_file(&mut self, path: &Path, text: &str) -> Result<Option<PathBuf>, String> {
            self.calls.push(format!("write_file {}", path.display()));
            self.written.push((path.to_path_buf(), text.to_string()));
            Ok(None)
        }
        fn key_ptt(&mut self, _ok: &Confirmed, ptt: &PttChoice, seconds: f64) -> Result<KeyReport, String> {
            self.keyed.push(format!("key_ptt {} {seconds}", ptt.spec));
            Ok(KeyReport { keyed_read: Some(true), after_read: Some(false) })
        }
        fn transmit(
            &mut self,
            _ok: &Confirmed,
            device: &str,
            ptt: &PttChoice,
            timing: TxTiming,
            audio: &[f32],
        ) -> Result<TxMeasure, String> {
            self.keyed.push(format!(
                "transmit {device} {} txdelay {:.0} ms {:.2} s",
                ptt.spec,
                timing.txdelay_s * 1e3,
                audio.len() as f64 / 48_000.0
            ));
            Ok(TxMeasure {
                keyed_ms: (timing.txdelay_s + timing.txtail_s) * 1e3 + audio.len() as f64 / 48.0,
                txdelay_ms: timing.txdelay_s * 1e3,
                txtail_ms: timing.txtail_s * 1e3,
                keyed_read: Some(true),
                after_read: Some(false),
            })
        }
        fn transmit_and_listen(
            &mut self,
            ok: &Confirmed,
            device: &str,
            ptt: &PttChoice,
            timing: TxTiming,
            audio: &[f32],
            listen_s: f64,
        ) -> Result<(TxMeasure, Vec<f32>), String> {
            let m = self.transmit(ok, device, ptt, timing, audio)?;
            let heard = self.heard.clone().unwrap_or_else(|| {
                // A loopback: the frame at half level in a little noise, with
                // silence around it.
                let mut x = vec![0f32; 24_000];
                x.extend(audio.iter().map(|v| v * 0.5));
                x.extend(std::iter::repeat_n(0f32, (listen_s * 48_000.0) as usize));
                let mut r = cairn_phy::channel::Rng::new(5);
                x.iter().map(|v| v + 1e-3 * r.normal() as f32).collect()
            });
            Ok((m, heard))
        }
    }
}
