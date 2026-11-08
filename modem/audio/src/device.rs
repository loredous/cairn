//! Device listing and selection by name.

use cpal::traits::{DeviceTrait, HostTrait};

use crate::{AudioError, SAMPLE_RATE};

/// Stream direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Capture.
    Input,
    /// Playback.
    Output,
}

/// One device as listed.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Backend id (ALSA PCM name, e.g. `plughw:CARD=AllInOneCable,DEV=0`).
    pub id: String,
    /// Description lines (e.g. `All-In-One-Cable, USB Audio`).
    pub description: Vec<String>,
    /// Can capture at 48 kHz.
    pub input_48k: bool,
    /// Can play at 48 kHz.
    pub output_48k: bool,
    /// Listed for capture at all.
    pub input: bool,
    /// Listed for playback at all.
    pub output: bool,
}

/// Lowercase ASCII letters and digits only, so `All-In-One-Cable` matches
/// `AllInOneCable`.
pub fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn info(d: &cpal::Device) -> DeviceInfo {
    let id = d.id().map(|i| i.id().to_string()).unwrap_or_else(|_| d.to_string());
    let description = d
        .description()
        .map(|desc| {
            let mut lines: Vec<String> = desc.extended().map(str::to_string).collect();
            if lines.is_empty() {
                lines.push(desc.name().to_string());
            }
            lines
        })
        .unwrap_or_default();
    let rate_ok = |r: Result<Vec<cpal::SupportedStreamConfigRange>, cpal::Error>| {
        r.map(|v| v.iter().any(|c| c.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= c.max_sample_rate()))
            .unwrap_or(false)
    };
    let ins = d.supported_input_configs().map(|i| i.collect::<Vec<_>>());
    let outs = d.supported_output_configs().map(|i| i.collect::<Vec<_>>());
    let input = ins.as_ref().is_ok_and(|v| !v.is_empty());
    let output = outs.as_ref().is_ok_and(|v| !v.is_empty());
    DeviceInfo { id, description, input_48k: rate_ok(ins), output_48k: rate_ok(outs), input, output }
}

/// Every device of the default host (ALSA on Linux), with its capabilities.
/// Opening each to query formats takes a moment and skips busy devices.
pub fn list_devices() -> Result<Vec<DeviceInfo>, AudioError> {
    let host = cpal::default_host();
    Ok(host.devices()?.map(|d| info(&d)).collect())
}

/// Preference among several devices whose names match: the ALSA plug layer
/// on the hardware first (exact format when native, conversion when not),
/// then the raw hardware, then everything else.
fn rank(id: &str) -> u8 {
    if id.starts_with("plughw:") {
        0
    } else if id.starts_with("hw:") {
        1
    } else if id.starts_with("sysdefault:") {
        2
    } else {
        3
    }
}

/// The device whose id or description contains `query` (normalized; see
/// [`normalize`]) and that supports `dir`. An exact id match wins; among
/// several matches the plug layer on the hardware is preferred, then
/// the raw hardware. `default` picks the host's default device.
pub fn find_device(query: &str, dir: Direction) -> Result<(cpal::Device, String), AudioError> {
    let host = cpal::default_host();
    if query.eq_ignore_ascii_case("default") {
        let d = match dir {
            Direction::Input => host.default_input_device(),
            Direction::Output => host.default_output_device(),
        };
        let d = d.ok_or_else(|| AudioError::Device("no default device".into()))?;
        let id = d.id().map(|i| i.id().to_string()).unwrap_or_else(|_| d.to_string());
        return Ok((d, id));
    }
    let want = normalize(query);
    if want.is_empty() {
        return Err(AudioError::Device("empty device name".into()));
    }
    let mut matches: Vec<(cpal::Device, String)> = Vec::new();
    for d in host.devices()? {
        let id = d.id().map(|i| i.id().to_string()).unwrap_or_else(|_| d.to_string());
        if id == query {
            matches.clear();
            matches.push((d, id));
            break;
        }
        let desc = d.description().map(|x| x.extended().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        if normalize(&id).contains(&want) || normalize(&desc).contains(&want) {
            matches.push((d, id));
        }
    }
    matches.retain(|(d, _)| match dir {
        Direction::Input => d.supports_input(),
        Direction::Output => d.supports_output(),
    });
    matches.sort_by_key(|(_, id)| rank(id));
    matches.into_iter().next().ok_or_else(|| {
        AudioError::Device(format!(
            "no {} device matches {query:?} (see `cairn-modem devices`; a busy device is not listed)",
            match dir {
                Direction::Input => "capture",
                Direction::Output => "playback",
            }
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_without_punctuation() {
        assert!(normalize("plughw:CARD=AllInOneCable,DEV=0").contains(&normalize("All-In-One-Cable")));
        assert!(rank("plughw:CARD=X,DEV=0") < rank("hw:CARD=X,DEV=0"));
        assert!(rank("hw:CARD=X,DEV=0") < rank("front:CARD=X,DEV=0"));
    }
}
