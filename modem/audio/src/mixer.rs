//! Receive-gain control (SPEC 13.5): the capture gain of the interface the
//! modem opened, behind [`MixerControl`] so the AGC runs against a mock in
//! tests. The ALSA implementation (the `alsa` feature) only ever touches a
//! capture volume; there is no way to reach a playback (transmit) control
//! through this trait.

use std::sync::{Arc, Mutex};

/// A capture-gain control.
pub trait MixerControl: Send {
    /// What it is, e.g. `hw:CARD=AllInOneCable 'AIOC Audio In'`.
    fn name(&self) -> String;
    /// Gain range, dB (min, max).
    fn range_db(&self) -> (f64, f64);
    /// The current gain, dB.
    fn get_db(&mut self) -> Result<f64, String>;
    /// Set the gain (clamped to the range); returns the gain now in effect.
    fn set_db(&mut self, db: f64) -> Result<f64, String>;
}

/// A mixer in memory, with a shared handle to see and change its gain from
/// a test (or a simulator that applies the gain to the audio).
#[derive(Clone, Debug)]
pub struct MockMixer {
    state: Arc<Mutex<MockState>>,
}

#[derive(Debug)]
struct MockState {
    gain_db: f64,
    min_db: f64,
    max_db: f64,
    step_db: f64,
    sets: usize,
}

impl MockMixer {
    /// A control from `min_db` to `max_db` in steps of `step_db` (0: any
    /// value), now at `gain_db`.
    pub fn new(gain_db: f64, min_db: f64, max_db: f64, step_db: f64) -> Self {
        MockMixer { state: Arc::new(Mutex::new(MockState { gain_db, min_db, max_db, step_db, sets: 0 })) }
    }

    /// The gain now, dB.
    pub fn gain_db(&self) -> f64 {
        self.state.lock().expect("mock mixer").gain_db
    }

    /// Times the gain was set.
    pub fn sets(&self) -> usize {
        self.state.lock().expect("mock mixer").sets
    }
}

impl MixerControl for MockMixer {
    fn name(&self) -> String {
        "mock capture gain".into()
    }
    fn range_db(&self) -> (f64, f64) {
        let s = self.state.lock().expect("mock mixer");
        (s.min_db, s.max_db)
    }
    fn get_db(&mut self) -> Result<f64, String> {
        Ok(self.gain_db())
    }
    fn set_db(&mut self, db: f64) -> Result<f64, String> {
        let mut s = self.state.lock().expect("mock mixer");
        let mut g = db.clamp(s.min_db, s.max_db);
        if s.step_db > 0.0 {
            g = s.min_db + ((g - s.min_db) / s.step_db).round() * s.step_db;
        }
        s.gain_db = g;
        s.sets += 1;
        Ok(g)
    }
}

/// The ALSA card behind a PCM id: `plughw:CARD=AllInOneCable,DEV=0` ->
/// `hw:CARD=AllInOneCable`, `hw:2,0` -> `hw:2`; None for PCMs that are not
/// a card (`default`, `pipewire`, `null`).
pub fn card_of(pcm_id: &str) -> Option<String> {
    let (_, rest) = pcm_id.split_once(':')?;
    let card = rest.split(',').next()?.trim();
    let card = card.strip_prefix("CARD=").unwrap_or(card);
    if card.is_empty() {
        None
    } else if card.parse::<u32>().is_ok() {
        Some(format!("hw:{card}"))
    } else {
        Some(format!("hw:CARD={card}"))
    }
}

/// Preference among capture controls when none is named: the interface's
/// own input control first (`AIOC Audio In`, `Mic`, `Capture`), then any
/// other capture volume.
pub fn control_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n.contains("capture") || n.ends_with(" in") || n.contains("audio in") {
        0
    } else if n.contains("mic") {
        1
    } else {
        2
    }
}

#[cfg(all(target_os = "linux", feature = "alsa"))]
pub use alsa_mixer::AlsaCaptureGain;

#[cfg(all(target_os = "linux", feature = "alsa"))]
mod alsa_mixer {
    use super::{MixerControl, control_rank};
    use alsa::Round;
    use alsa::mixer::{Mixer, SelemChannelId, SelemId};

    /// The capture volume of an ALSA simple mixer element.
    pub struct AlsaCaptureGain {
        mixer: Mixer,
        card: String,
        sid: SelemId,
        name: String,
        /// The element reports dB; else the raw range is mapped linearly
        /// onto 0..`RAW_SPAN_DB`.
        db: bool,
        raw: (i64, i64),
        range: (f64, f64),
    }

    /// Without dB information the raw range counts as this many dB.
    const RAW_SPAN_DB: f64 = 48.0;

    impl AlsaCaptureGain {
        /// Open the capture-volume control `control` (None: the best
        /// candidate, see [`control_rank`]) on ALSA card `card` (e.g.
        /// `hw:CARD=AllInOneCable`).
        pub fn open(card: &str, control: Option<&str>) -> Result<Self, String> {
            let mixer = Mixer::new(card, false).map_err(|e| format!("mixer {card}: {e}"))?;
            let mut cands: Vec<(u8, SelemId, String)> = Vec::new();
            for elem in mixer.iter() {
                let Some(se) = alsa::mixer::Selem::new(elem) else { continue };
                if !se.has_capture_volume() {
                    continue;
                }
                let id = se.get_id();
                let name = id.get_name().unwrap_or("").to_string();
                if control.is_some_and(|c| !c.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                cands.push((control_rank(&name), id, name));
            }
            cands.sort_by_key(|c| c.0);
            let Some((_, sid, name)) = cands.into_iter().next() else {
                return Err(match control {
                    Some(c) => format!("{card} has no capture volume control {c:?}"),
                    None => format!("{card} has no capture volume control"),
                });
            };
            let (raw, range, db) = {
                let se = mixer.find_selem(&sid).ok_or("control vanished")?;
                let raw = se.get_capture_volume_range();
                let (lo, hi) = se.get_capture_db_range();
                if hi.0 > lo.0 {
                    // A "mute" minimum reads as a huge negative dB value: the AGC
                    // never goes more than 60 dB below the top anyway.
                    let hi = hi.to_db() as f64;
                    (raw, ((lo.to_db() as f64).max(hi - 60.0), hi), true)
                } else {
                    (raw, (0.0, RAW_SPAN_DB), false)
                }
            };
            Ok(AlsaCaptureGain { mixer, card: card.to_string(), sid, name, db, raw, range })
        }
    }

    impl MixerControl for AlsaCaptureGain {
        fn name(&self) -> String {
            format!("{} '{}'{}", self.card, self.name, if self.db { "" } else { " (no dB scale; raw steps)" })
        }

        fn range_db(&self) -> (f64, f64) {
            self.range
        }

        fn get_db(&mut self) -> Result<f64, String> {
            let _ = self.mixer.handle_events();
            let se = self.mixer.find_selem(&self.sid).ok_or("control vanished")?;
            if self.db {
                se.get_capture_vol_db(SelemChannelId::mono()).map(|m| m.to_db() as f64).map_err(|e| e.to_string())
            } else {
                let v = se.get_capture_volume(SelemChannelId::mono()).map_err(|e| e.to_string())?;
                let (lo, hi) = self.raw;
                Ok((v - lo) as f64 / (hi - lo).max(1) as f64 * RAW_SPAN_DB)
            }
        }

        fn set_db(&mut self, db: f64) -> Result<f64, String> {
            let db = db.clamp(self.range.0, self.range.1);
            {
                let se = self.mixer.find_selem(&self.sid).ok_or("control vanished")?;
                if self.db {
                    se.set_capture_db_all(alsa::mixer::MilliBel::from_db(db as f32), Round::Floor)
                        .map_err(|e| e.to_string())?;
                } else {
                    let (lo, hi) = self.raw;
                    let v = lo + ((db / RAW_SPAN_DB) * (hi - lo) as f64).round() as i64;
                    se.set_capture_volume_all(v.clamp(lo, hi)).map_err(|e| e.to_string())?;
                }
            }
            self.get_db()
        }
    }
}

/// Open the capture-gain control of the card behind PCM `pcm_id`.
pub fn open_capture_gain(pcm_id: &str, control: Option<&str>) -> Result<Box<dyn MixerControl>, String> {
    #[cfg(all(target_os = "linux", feature = "alsa"))]
    {
        let card = card_of(pcm_id).ok_or_else(|| format!("{pcm_id} is not an ALSA card (use its hw:/plughw: name)"))?;
        Ok(Box::new(AlsaCaptureGain::open(&card, control)?))
    }
    #[cfg(not(all(target_os = "linux", feature = "alsa")))]
    {
        let _ = (pcm_id, control);
        Err("no mixer support in this build (the alsa feature)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_from_pcm_ids() {
        assert_eq!(card_of("plughw:CARD=AllInOneCable,DEV=0").as_deref(), Some("hw:CARD=AllInOneCable"));
        assert_eq!(card_of("hw:CARD=AllInOneCable,DEV=0").as_deref(), Some("hw:CARD=AllInOneCable"));
        assert_eq!(card_of("sysdefault:CARD=Device").as_deref(), Some("hw:CARD=Device"));
        assert_eq!(card_of("hw:2,0").as_deref(), Some("hw:2"));
        assert_eq!(card_of("default"), None);
        assert_eq!(card_of("pipewire"), None);
        assert!(control_rank("AIOC Audio In") < control_rank("Auto Gain Control"));
        assert!(control_rank("Capture") < control_rank("Mic Boost"));
    }

    #[test]
    fn mock_clamps_and_steps() {
        let mut m = MockMixer::new(0.0, -24.0, 6.0, 0.5);
        assert_eq!(m.set_db(-3.2).unwrap(), -3.0);
        assert_eq!(m.set_db(-40.0).unwrap(), -24.0);
        assert_eq!(m.set_db(10.0).unwrap(), 6.0);
        let h = m.clone();
        assert_eq!(h.gain_db(), 6.0);
        assert_eq!(h.sets(), 3);
    }
}
