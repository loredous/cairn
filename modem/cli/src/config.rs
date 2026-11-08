//! The configuration file and rig profiles (SPEC 13.4, 13.6).
//!
//! `cairn-modem` reads `~/.config/cairn/cairn.toml` (or `$XDG_CONFIG_HOME`,
//! `$CAIRN_CONFIG`, `--config PATH`); command-line flags and environment
//! variables override it. A **rig profile** describes one interface and radio
//! combination (audio device, PTT method and lines, TXDelay and TXTail,
//! output level, squelch, carrier tail, passband and width cap) and supplies
//! defaults under the config file: built-in < profile < config file < flags.
//! Profiles ship in the repository's `docs/rigs/` (compiled in) and may be
//! added or overridden in `~/.config/cairn/rigs/NAME.toml`.
//!
//! Both formats are TOML with the same sections (`[audio]`, `[ptt]`,
//! `[timing]`, `[link]`); a profile adds `[profile]` (its name and what it
//! knows about the radio), the config file adds `[station]`, `[host]` and
//! `[unattended]`. Unknown keys are errors, so a typo does not pass silently.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    *t == T::default()
}

/// `[station]`: who this station is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationSection {
    /// Callsign and SSID, e.g. `N0CALL-1` (SPEC 8.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Maidenhead locator for the ID block (4 or 6 characters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<String>,
    /// Rig profile name (`cairn-modem rigs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rig: Option<String>,
}

/// `[audio]`: the sound card and its levels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSection {
    /// Device query: a substring of the ALSA name or description, or a PCM id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Device period, frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<u32>,
    /// Capture buffer, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<f64>,
    /// Capture backend: auto, alsa or cpal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Ask for real-time priority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rt: Option<bool>,
    /// Real-time priority of the audio threads, 1-99.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rt_prio: Option<u8>,
    /// Receive AGC on the capture-gain control (SPEC 13.5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_agc: Option<bool>,
    /// The capture-gain control for the AGC (e.g. `AIOC Audio In`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_agc_control: Option<String>,
    /// The playback-volume control that sets drive (informational: the
    /// modem never changes it; setup and self-test report it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_control: Option<String>,
    /// Its setting for 70% deviation, dB (from setup or the drive sweep).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_db: Option<f64>,
}

/// `[ptt]`: how the transmitter is keyed (SPEC 13.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PttSection {
    /// `none`, `vox`, `serial:PATH`, `cm108:/dev/hidrawN` (or `cm108:auto`),
    /// `rigctld[:HOST[:PORT]]`, as `--ptt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Serial line that keys: dtr or rts (the other is held low).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Serial PTT is active low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invert: Option<bool>,
    /// CM108 GPIO pin (1-8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpio: Option<u8>,
}

/// `[timing]`: SPEC 7.3.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingSection {
    /// PTT to first audio sample at the DAC, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txdelay_ms: Option<f64>,
    /// Last sample at the DAC to PTT release, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txtail_ms: Option<f64>,
}

/// `[link]`: the link layer's settings (SPEC 7.3, 10, 11).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSection {
    /// Width cap in tones, 24-56.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<u8>,
    /// Report the all-ones tone mask instead of tracking per-tone SNR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_mask: Option<bool>,
    /// p-persistence (SPEC 11.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persistence: Option<f64>,
    /// Slot time, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_ms: Option<u64>,
    /// Maximum frame on-air time, s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_frame: Option<f64>,
    /// Broadcast rung, MCS 0-3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast_mcs: Option<u8>,
    /// Quieting band for carrier detect, `LO-HI` Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quieting: Option<String>,
    /// Refuse incoming CONNECTs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refuse: Option<bool>,
    /// Add the ID block to UI frames on the 9-minute ID clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_broadcasts: Option<bool>,
    /// Dictionary directory (`*.zdict` files, SPEC 12.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dict_dir: Option<PathBuf>,
}

/// `[host]`: interfaces and files.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSection {
    /// KISS TCP address, or "off".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiss: Option<String>,
    /// Native API TCP address, or "off".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Native API Unix socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_socket: Option<PathBuf>,
    /// Control interface TCP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// Received files and messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_dir: Option<PathBuf>,
    /// Captures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_dir: Option<PathBuf>,
    /// The heard list across restarts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heard_file: Option<PathBuf>,
    /// Raw-audio ring, s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring_seconds: Option<f64>,
    /// Capture automatically after a failed frame from the session peer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_on_failure: Option<bool>,
    /// Log every output line to this file too (rotated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_file: Option<PathBuf>,
    /// Rotate the log at this size, KiB (default 1024).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_max_kb: Option<u64>,
    /// Rotated logs kept (default 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_keep: Option<u32>,
}

/// `[unattended.hooks]`: a command (program and arguments, no shell) per
/// event. Received content never goes into the arguments; it is passed in
/// environment variables and on standard input ([`crate::unattended::Hooks`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksSection {
    /// A session came up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connected: Vec<String>,
    /// A session ended (DISCONNECT or link loss).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disconnected: Vec<String>,
    /// A file arrived complete (needs `host.rx_dir`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub received_file: Vec<String>,
    /// A text message arrived (needs `host.rx_dir`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub received_message: Vec<String>,
    /// An incoming CONNECT was refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
    /// The watchdog or the TX budget stopped transmitting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alarm: Vec<String>,
    /// Kill a hook still running after this long, s (default 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<f64>,
}

/// `[unattended]`: automatic operation (SPEC 13.4, 13.6).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnattendedSection {
    /// Run unattended (as `run --unattended`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Accept CONNECTs only from these (CALL, CALL-N, CALL-*, *); empty: anyone not denied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Refuse CONNECTs from these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Disconnect a session after this many minutes (default 30; 0: no limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_min: Option<f64>,
    /// Keyed time allowed per UTC day, minutes (default 60; 0: no limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_tx_min: Option<f64>,
    /// Keep the day's keyed time here across restarts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_file: Option<PathBuf>,
    /// Watchdog: release PTT and stop when keyed this much longer than the
    /// transmission should take, s (default 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watchdog_margin_s: Option<f64>,
    /// Watchdog: stop when no audio arrives for this long, s (default 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stall_s: Option<f64>,
    /// Hooks.
    #[serde(default, skip_serializing_if = "is_default")]
    pub hooks: HooksSection,
}

/// The configuration file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// `[station]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub station: StationSection,
    /// `[audio]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub audio: AudioSection,
    /// `[ptt]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub ptt: PttSection,
    /// `[timing]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub timing: TimingSection,
    /// `[link]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub link: LinkSection,
    /// `[host]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub host: HostSection,
    /// `[unattended]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub unattended: UnattendedSection,
}

/// What a profile knows about its radio and interface.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMeta {
    /// Profile name (the file stem by convention).
    pub name: String,
    /// One line.
    pub description: String,
    /// `measured` (values from a bench or on-air test, say on what) or
    /// `unverified` (conservative defaults from documentation).
    pub status: String,
    /// What the measured values were measured on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_on: Option<String>,
    /// The interface (AIOC, Digirig, SignaLink, CM108 cable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// USB vendor:product IDs of the interface (hex), for detection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usb_ids: Vec<String>,
    /// Substrings that identify the audio device in `cairn-modem devices`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_match: Vec<String>,
    /// Substrings that identify its serial port under /dev/serial/by-id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serial_match: Vec<String>,
    /// `open` (carrier detect needs the receiver's noise) or `any`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squelch: Option<String>,
    /// The radio's carrier (squelch) tail after the far end releases PTT, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier_tail_ms: Option<f64>,
    /// `mic`, `data` or `unknown`: which audio path (docs/bench-testing.md).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Audio passband measured or expected, Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passband_hz: Option<[f64; 2]>,
    /// Tones the passband leaves weak (masks drop them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weak_tones: Vec<u8>,
    /// Free text: quirks, where values came from, what to check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A rig profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RigProfile {
    /// `[profile]`.
    pub profile: ProfileMeta,
    /// `[audio]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub audio: AudioSection,
    /// `[ptt]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub ptt: PttSection,
    /// `[timing]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub timing: TimingSection,
    /// `[link]`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub link: LinkSection,
}

impl RigProfile {
    /// Parse a profile.
    pub fn parse(text: &str) -> Result<RigProfile, String> {
        let p: RigProfile = toml::from_str(text).map_err(|e| e.to_string())?;
        if p.profile.name.is_empty() {
            return Err("[profile] name is empty".into());
        }
        if !matches!(p.profile.status.as_str(), "measured" | "unverified") {
            return Err(format!("[profile] status must be \"measured\" or \"unverified\", not {:?}", p.profile.status));
        }
        Ok(p)
    }

    /// The settings it supplies, as a config layer.
    pub fn overlay(&self) -> FileConfig {
        FileConfig {
            audio: self.audio.clone(),
            ptt: self.ptt.clone(),
            timing: self.timing.clone(),
            link: self.link.clone(),
            ..Default::default()
        }
    }

    /// TOML text.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a profile serializes")
    }

    /// True when the device description or id matches the profile.
    pub fn matches_device(&self, text: &str) -> bool {
        let t = cairn_audio::normalize(text);
        self.profile.device_match.iter().any(|m| !m.is_empty() && t.contains(&cairn_audio::normalize(m)))
    }

    /// True when the USB `vid:pid` matches one of the profile's IDs (a
    /// `vid:*` entry matches any product of that vendor).
    pub fn matches_usb(&self, vid_pid: &str) -> bool {
        let v = vid_pid.to_ascii_lowercase();
        self.profile.usb_ids.iter().any(|id| {
            let id = id.to_ascii_lowercase();
            match id.strip_suffix(":*") {
                Some(vendor) => v.split(':').next() == Some(vendor),
                None => id == v,
            }
        })
    }
}

impl FileConfig {
    /// Parse a config file.
    pub fn parse(text: &str) -> Result<FileConfig, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// TOML text.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a config serializes")
    }

    /// This layer with `upper` on top: every value `upper` sets wins, tables
    /// merge key by key, lists are replaced whole.
    pub fn layer(&self, upper: &FileConfig) -> FileConfig {
        fn merge(lo: &mut toml::Table, hi: &toml::Table) {
            for (k, v) in hi {
                match (lo.get_mut(k), v) {
                    (Some(toml::Value::Table(a)), toml::Value::Table(b)) => merge(a, b),
                    _ => {
                        lo.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        let mut lo = toml::Table::try_from(self).expect("a config is a table");
        let hi = toml::Table::try_from(upper).expect("a config is a table");
        merge(&mut lo, &hi);
        lo.try_into().expect("a merged config deserializes")
    }

    /// What this config sets beyond `base`: values equal to `base`'s are
    /// left out (a config written over a rig profile keeps only the
    /// operator's own changes).
    pub fn minus(&self, base: &FileConfig) -> FileConfig {
        fn diff(hi: &mut toml::Table, lo: &toml::Table) {
            hi.retain(|k, v| match (v, lo.get(k)) {
                (toml::Value::Table(a), Some(toml::Value::Table(b))) => {
                    diff(a, b);
                    !a.is_empty()
                }
                (v, Some(b)) => v != b,
                (_, None) => true,
            });
        }
        let mut hi = toml::Table::try_from(self).expect("a config is a table");
        let lo = toml::Table::try_from(base).expect("a config is a table");
        diff(&mut hi, &lo);
        hi.try_into().expect("a reduced config deserializes")
    }

    /// Load `path`; a missing file is an empty config when `optional`.
    pub fn load(path: &Path, optional: bool) -> Result<FileConfig, String> {
        match std::fs::read_to_string(path) {
            Ok(t) => FileConfig::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if optional && e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// The user's configuration directory: `$XDG_CONFIG_HOME/cairn` or
/// `~/.config/cairn`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        return Some(PathBuf::from(x).join("cairn"));
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".config").join("cairn"))
}

/// The default config file.
pub fn default_config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("cairn.toml"))
}

/// The user's rig profile directory.
pub fn user_rigs_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("rigs"))
}

/// `~/` at the start of a path becomes `$HOME/`.
pub fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => p.to_path_buf(),
    }
}

/// The profiles shipped in `docs/rigs/`, compiled in.
pub const BUILTIN_RIGS: &[(&str, &str)] = &[
    ("aioc-ht", include_str!("../../../docs/rigs/aioc-ht.toml")),
    ("digirig", include_str!("../../../docs/rigs/digirig.toml")),
    ("signalink", include_str!("../../../docs/rigs/signalink.toml")),
    ("cm108-gpio", include_str!("../../../docs/rigs/cm108-gpio.toml")),
];

/// Where a profile came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RigSource {
    /// Compiled in from `docs/rigs/`.
    Builtin,
    /// A file in the user's directory.
    User(PathBuf),
}

/// A profile and its source.
#[derive(Clone, Debug)]
pub struct Rig {
    /// The profile.
    pub profile: RigProfile,
    /// Where it came from.
    pub source: RigSource,
}

/// Every profile: the built-in ones, then `user_dir/*.toml`, a user profile
/// replacing a built-in one of the same name. Unreadable user files come
/// back as errors next to the list.
pub fn all_rigs(user_dir: Option<&Path>) -> (Vec<Rig>, Vec<String>) {
    let mut rigs: Vec<Rig> = BUILTIN_RIGS
        .iter()
        .map(|(n, t)| Rig {
            profile: RigProfile::parse(t).unwrap_or_else(|e| panic!("built-in rig {n}: {e}")),
            source: RigSource::Builtin,
        })
        .collect();
    let mut errors = Vec::new();
    if let Some(dir) = user_dir {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect())
            .unwrap_or_default();
        files.sort();
        for f in files {
            match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|t| RigProfile::parse(&t)) {
                Ok(p) => {
                    rigs.retain(|r| r.profile.profile.name != p.profile.name);
                    rigs.push(Rig { profile: p, source: RigSource::User(f) });
                }
                Err(e) => errors.push(format!("{}: {e}", f.display())),
            }
        }
    }
    (rigs, errors)
}

/// The profile called `name`.
pub fn find_rig(name: &str, user_dir: Option<&Path>) -> Result<Rig, String> {
    let (rigs, errors) = all_rigs(user_dir);
    let names: Vec<String> = rigs.iter().map(|r| r.profile.profile.name.clone()).collect();
    rigs.into_iter().find(|r| r.profile.profile.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        let mut m = format!("no rig profile {name:?}; known: {}", names.join(", "));
        if !errors.is_empty() {
            m += &format!(" (unreadable: {})", errors.join("; "));
        }
        m
    })
}

/// The effective settings: the rig profile named by `rig` (or by the
/// config's `station.rig`) under the config file.
pub fn effective(config: &FileConfig, rig: Option<&str>, user_dir: Option<&Path>) -> Result<FileConfig, String> {
    let name = rig.map(str::to_string).or_else(|| config.station.rig.clone());
    let mut cfg = match &name {
        Some(n) => find_rig(n, user_dir)?.profile.overlay().layer(config),
        None => config.clone(),
    };
    cfg.station.rig = name;
    Ok(cfg)
}

/// Write `text` to `path` (creating its directory), keeping any previous
/// file as `PATH.bak`. Returns the backup's path when there was one.
pub fn write_with_backup(path: &Path, text: &str) -> std::io::Result<Option<PathBuf>> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    let backup = if path.exists() {
        let mut b = path.as_os_str().to_owned();
        b.push(".bak");
        let b = PathBuf::from(b);
        std::fs::copy(path, &b)?;
        Some(b)
    } else {
        None
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rigs_parse_and_name_themselves() {
        let (rigs, errors) = all_rigs(None);
        assert!(errors.is_empty());
        assert_eq!(rigs.len(), BUILTIN_RIGS.len());
        for (r, (name, _)) in rigs.iter().zip(BUILTIN_RIGS) {
            assert_eq!(&r.profile.profile.name, name);
            assert!(r.profile.ptt.method.is_some(), "{name}: a PTT method");
            assert!(r.profile.timing.txdelay_ms.is_some(), "{name}: a TXDelay");
            // Round trip.
            assert_eq!(RigProfile::parse(&r.profile.to_toml()).unwrap(), r.profile);
        }
        // No real callsigns in shipped profiles.
        for (_, t) in BUILTIN_RIGS {
            assert!(!t.contains("K0JLB"));
        }
    }

    #[test]
    fn the_aioc_profile_holds_the_field_test_values() {
        let r = find_rig("aioc-ht", None).unwrap().profile;
        assert_eq!(r.profile.status, "measured");
        assert_eq!(r.timing.txdelay_ms, Some(300.0));
        assert_eq!(r.timing.txtail_ms, Some(20.0));
        assert_eq!(r.link.cap, Some(32));
        assert_eq!(r.profile.weak_tones, vec![0, 1, 2]);
        assert_eq!(r.ptt.line.as_deref(), Some("dtr"));
        assert!(r.matches_device("plughw:CARD=AllInOneCable,DEV=0"));
        assert!(r.matches_device("All-In-One-Cable, USB Audio"));
        assert!(!r.matches_device("HDA Intel PCH"));
        assert!(r.matches_usb("1209:7388"));
        let cm = find_rig("cm108-gpio", None).unwrap().profile;
        assert!(cm.matches_usb("0D8C:013C"), "vendor wildcard");
        assert!(!cm.matches_usb("1209:7388"));
    }

    #[test]
    fn layers_override_key_by_key() {
        let lo = FileConfig::parse(
            "[audio]\ndevice = \"AIOC\"\nperiod = 480\n[timing]\ntxdelay_ms = 300\ntxtail_ms = 20\n\
             [unattended]\nallow = [\"N0AAA-*\"]\n",
        )
        .unwrap();
        let hi = FileConfig::parse("[timing]\ntxdelay_ms = 250\n[unattended]\nallow = [\"N0BBB\"]\n").unwrap();
        let m = lo.layer(&hi);
        assert_eq!(m.audio.device.as_deref(), Some("AIOC"));
        assert_eq!(m.timing.txdelay_ms, Some(250.0));
        assert_eq!(m.timing.txtail_ms, Some(20.0));
        assert_eq!(m.unattended.allow, vec!["N0BBB".to_string()], "lists are replaced");
        assert_eq!(FileConfig::default().layer(&m), m);
        assert_eq!(m.layer(&FileConfig::default()), m);
        // minus is layer's inverse for what the upper layer changed.
        let d = m.minus(&lo);
        assert_eq!(d.timing.txdelay_ms, Some(250.0));
        assert_eq!(d.timing.txtail_ms, None);
        assert_eq!(d.audio, AudioSection::default());
        assert_eq!(lo.layer(&d), m);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(FileConfig::parse("[timing]\ntxdelay = 300\n").unwrap_err().contains("txdelay"));
        assert!(FileConfig::parse("[nope]\n").is_err());
        assert!(RigProfile::parse("[profile]\nname = \"x\"\ndescription = \"\"\nstatus = \"guess\"\n").is_err());
    }

    #[test]
    fn profile_under_config_and_user_override() {
        let dir = std::env::temp_dir().join(format!("cairn-rigs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut mine = find_rig("aioc-ht", None).unwrap().profile;
        mine.timing.txdelay_ms = Some(220.0);
        std::fs::write(dir.join("aioc-ht.toml"), mine.to_toml()).unwrap();
        std::fs::write(dir.join("broken.toml"), "[profile]\n").unwrap();
        let (rigs, errors) = all_rigs(Some(&dir));
        assert_eq!(errors.len(), 1);
        let r = rigs.iter().find(|r| r.profile.profile.name == "aioc-ht").unwrap();
        assert!(matches!(r.source, RigSource::User(_)));
        assert_eq!(r.profile.timing.txdelay_ms, Some(220.0));

        let cfg =
            FileConfig::parse("[station]\ncallsign = \"N0CALL-1\"\nrig = \"aioc-ht\"\n[timing]\ntxtail_ms = 30\n")
                .unwrap();
        let e = effective(&cfg, None, Some(&dir)).unwrap();
        assert_eq!(e.timing.txdelay_ms, Some(220.0), "from the user's profile");
        assert_eq!(e.timing.txtail_ms, Some(30.0), "the config file wins");
        assert_eq!(e.station.callsign.as_deref(), Some("N0CALL-1"));
        let e = effective(&cfg, Some("digirig"), Some(&dir)).unwrap();
        assert_eq!(e.station.rig.as_deref(), Some("digirig"), "--rig wins");
        assert!(effective(&cfg, Some("nonesuch"), None).unwrap_err().contains("known: aioc-ht"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_keeps_a_backup() {
        let dir = std::env::temp_dir().join(format!("cairn-cfg-{}", std::process::id()));
        let p = dir.join("sub").join("cairn.toml");
        assert_eq!(write_with_backup(&p, "a = 1\n").unwrap(), None);
        let b = write_with_backup(&p, "b = 2\n").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "a = 1\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b = 2\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
