//! Frame geometry (SPEC 5.4, 6.4, 8.1; ADR 0004, ADR 0005
//! `frame-symbol-content`): which symbols carry what, on which tones, at
//! which scale. Mirrors `FrameConfig` of `sim/cairn_sim/frame.py`.
//!
//! A frame is `preamble (4) | header (7) | [probe (2)] | control blocks |
//! data blocks`. Control blocks (0-3) are MCS 0 blocks in their own run of
//! symbols on the base data tones less masked ones, with pilots 3 and 20,
//! every active tone scaled by sqrt(24 / active tones) (ADR 0005).
//! [`FrameConfig::from_header`] gives the frame a header describes at the
//! link's width and mask ([`LinkState`]), for header-first reception.

use std::fmt;

use crate::fields::Header;
use crate::params::{
    self, BASE_TONES, CONTROL_CODED_BITS, CP_LONG, CP_SHORT, MAX_TONES, N_USEFUL, PREAMBLE_SYMBOLS, PROBE_SYMBOLS, Rung,
};

/// Why a frame configuration is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No such rung.
    Mcs(u8),
    /// Width outside 24-56.
    Width(usize),
    /// Prefix is neither 60 nor 120 samples.
    Prefix(usize),
    /// Tones above the base set need the width probe.
    NeedsProbe,
    /// A mask tone outside 0-55.
    MaskTone(usize),
    /// Pilot 3 or 20 masked.
    MaskedBasePilot,
    /// The mask leaves no data tone.
    NoDataTones,
    /// Data block count above 128.
    Blocks(usize),
    /// Control block count above 3.
    Controls(usize),
    /// The mask leaves no control tone.
    NoControlTones,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Mcs(m) => write!(f, "no rung MCS {m}"),
            ConfigError::Width(w) => write!(f, "width {w} outside {BASE_TONES}-{MAX_TONES}"),
            ConfigError::Prefix(c) => write!(f, "prefix {c} is neither {CP_SHORT} nor {CP_LONG}"),
            ConfigError::NeedsProbe => write!(f, "tones above the base set need the width probe"),
            ConfigError::MaskTone(k) => write!(f, "mask tone {k} outside 0-{}", MAX_TONES - 1),
            ConfigError::MaskedBasePilot => write!(f, "pilot tones 3 and 20 cannot be masked (SPEC 5.4)"),
            ConfigError::NoDataTones => write!(f, "the mask leaves no data tones"),
            ConfigError::Blocks(n) => write!(f, "data block count {n} above {}", params::MAX_DATA_BLOCKS),
            ConfigError::Controls(n) => write!(f, "control block count {n} above {}", params::MAX_CONTROL_BLOCKS),
            ConfigError::NoControlTones => write!(f, "the mask leaves no control tones"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// What a receiver must know that the header does not say (SPEC 5.4, 8.2):
/// the link width and tone mask, and optionally the prefix (None: detect it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkState {
    /// Link width, 24-56 tones.
    pub width: usize,
    /// Usable-tone mask report, or None for every tone usable.
    pub usable: Option<Vec<usize>>,
    /// Prefix, 60 or 120 samples, if known.
    pub cp: Option<usize>,
}

impl Default for LinkState {
    fn default() -> Self {
        LinkState { width: BASE_TONES, usable: None, cp: None }
    }
}

/// What a frame carries and how. Build with [`FrameConfig::new`] and the
/// `with_*` methods; every derived quantity is computed once.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameConfig {
    /// Link width W (24-56).
    pub width: usize,
    /// Data rung.
    pub mcs: u8,
    /// Data blocks.
    pub n_blocks: usize,
    /// Control blocks (0-3).
    pub n_control: usize,
    /// Prefix length, 60 or 120 samples.
    pub cp: usize,
    /// Apply clip-and-filter.
    pub clip: bool,
    /// Clip level in dB above RMS (default: the rung's, ADR 0003).
    pub clip_db: f64,
    /// Width probe after the header.
    pub has_probe: bool,
    /// Usable-tone mask report (SPEC 5.4), or None for every tone usable.
    pub usable: Option<Vec<usize>>,
    /// Tones below the width that data symbols leave silent.
    pub masked: Vec<usize>,
    /// Tones carrying coded bits in data symbols, lowest first.
    pub data_tones: Vec<usize>,
    /// Pilot tones present in data symbols.
    pub data_pilots: Vec<usize>,
    /// Data tones and data pilots, sorted.
    pub data_active_tones: Vec<usize>,
    /// Header data tones: base tones less pilots 3 and 20.
    pub base_data_tones: Vec<usize>,
    /// Tones carrying coded bits in control symbols: base data tones less masked ones.
    pub control_tones: Vec<usize>,
    /// Control tones and pilots 3 and 20, sorted.
    pub control_active_tones: Vec<usize>,
}

impl FrameConfig {
    /// A frame at `width` tones, rung `mcs`, `n_blocks` data blocks, short
    /// prefix, clipped at the rung's level, probe when wider than 24, no mask.
    pub fn new(width: usize, mcs: u8, n_blocks: usize) -> Result<Self, ConfigError> {
        let rung = params::rung(mcs).ok_or(ConfigError::Mcs(mcs))?;
        let mut cfg = FrameConfig {
            width,
            mcs,
            n_blocks,
            n_control: 0,
            cp: CP_SHORT,
            clip: true,
            // SPEC 7.1: a frame with no data blocks uses 7 dB.
            clip_db: if n_blocks == 0 { params::CLIP_DB_REFERENCE } else { rung.clip_db },
            has_probe: width > BASE_TONES,
            usable: None,
            masked: Vec::new(),
            data_tones: Vec::new(),
            data_pilots: Vec::new(),
            data_active_tones: Vec::new(),
            base_data_tones: Vec::new(),
            control_tones: Vec::new(),
            control_active_tones: Vec::new(),
        };
        cfg.derive()?;
        Ok(cfg)
    }

    /// The frame `header` describes at the link's width and mask (SPEC 8.2):
    /// the header's rung when it has data blocks (MCS 0 otherwise; the field
    /// is then ignored), its block counts and probe flag, prefix `cp`.
    pub fn from_header(header: &Header, link: &LinkState, cp: usize) -> Result<Self, ConfigError> {
        let mcs = if header.n_data > 0 { header.mcs } else { 0 };
        FrameConfig::new(link.width, mcs, header.n_data)?
            .with_controls(header.n_control)?
            .with_cp(cp)?
            .with_probe(header.probe)?
            .with_mask(link.usable.clone())
    }

    /// Use `n` control blocks (0-3).
    pub fn with_controls(mut self, n: usize) -> Result<Self, ConfigError> {
        self.n_control = n;
        self.derive()?;
        Ok(self)
    }

    /// Use prefix `cp` (60 or 120).
    pub fn with_cp(mut self, cp: usize) -> Result<Self, ConfigError> {
        self.cp = cp;
        self.derive()?;
        Ok(self)
    }

    /// Use the usable-tone mask `usable` (tones 0-55; at or above the width ignored).
    pub fn with_mask(mut self, usable: Option<Vec<usize>>) -> Result<Self, ConfigError> {
        self.usable = usable;
        self.derive()?;
        Ok(self)
    }

    /// Switch clip-and-filter on or off.
    pub fn with_clip(mut self, clip: bool) -> Self {
        self.clip = clip;
        self
    }

    /// Override the clip level.
    pub fn with_clip_db(mut self, clip_db: f64) -> Self {
        self.clip_db = clip_db;
        self
    }

    /// Force the width probe on or off.
    pub fn with_probe(mut self, probe: bool) -> Result<Self, ConfigError> {
        self.has_probe = probe;
        self.derive()?;
        Ok(self)
    }

    fn derive(&mut self) -> Result<(), ConfigError> {
        if !(BASE_TONES..=MAX_TONES).contains(&self.width) {
            return Err(ConfigError::Width(self.width));
        }
        if self.cp != CP_SHORT && self.cp != CP_LONG {
            return Err(ConfigError::Prefix(self.cp));
        }
        if self.width > BASE_TONES && !self.has_probe {
            return Err(ConfigError::NeedsProbe);
        }
        if self.n_blocks > params::MAX_DATA_BLOCKS {
            return Err(ConfigError::Blocks(self.n_blocks));
        }
        if self.n_control > params::MAX_CONTROL_BLOCKS {
            return Err(ConfigError::Controls(self.n_control));
        }
        let w = self.width;
        self.masked = match &self.usable {
            None => Vec::new(),
            Some(u) => {
                if let Some(&k) = u.iter().find(|&&k| k >= MAX_TONES) {
                    return Err(ConfigError::MaskTone(k));
                }
                (0..w).filter(|k| !u.contains(k)).collect()
            }
        };
        if self.masked.iter().any(|k| params::UNMASKABLE_TONES.contains(k)) {
            return Err(ConfigError::MaskedBasePilot);
        }
        let pilots = params::pilots_for(w);
        self.data_tones = (0..w).filter(|k| !pilots.contains(k) && !self.masked.contains(k)).collect();
        if self.data_tones.is_empty() {
            return Err(ConfigError::NoDataTones);
        }
        self.data_pilots = pilots.iter().copied().filter(|p| !self.masked.contains(p)).collect();
        let mut act: Vec<usize> = self.data_tones.iter().chain(&self.data_pilots).copied().collect();
        act.sort_unstable();
        self.data_active_tones = act;
        let base_pilots = params::pilots_for(BASE_TONES);
        self.base_data_tones = (0..BASE_TONES).filter(|k| !base_pilots.contains(k)).collect();
        self.control_tones = self.base_data_tones.iter().copied().filter(|k| !self.masked.contains(k)).collect();
        if self.n_control > 0 && self.control_tones.is_empty() {
            return Err(ConfigError::NoControlTones);
        }
        let mut act: Vec<usize> = self.control_tones.iter().chain(&base_pilots).copied().collect();
        act.sort_unstable();
        self.control_active_tones = act;
        Ok(())
    }

    /// The data rung.
    pub fn rung(&self) -> &'static Rung {
        &params::RUNGS[self.mcs as usize]
    }

    /// Samples per symbol, prefix included.
    pub fn stride(&self) -> usize {
        N_USEFUL + self.cp
    }

    /// Probe symbols (0 or 2).
    pub fn probe_symbols(&self) -> usize {
        if self.has_probe { PROBE_SYMBOLS } else { 0 }
    }

    /// Amplitude factor on every active tone of a data symbol: total power
    /// stays that of `width` unit-power tones whatever the mask (SPEC 5.4).
    pub fn data_gain(&self) -> f64 {
        (self.width as f64 / self.data_active_tones.len() as f64).sqrt()
    }

    /// Header symbols (7).
    pub fn header_symbols(&self) -> usize {
        params::HEADER_CODED_BITS.div_ceil(2 * self.base_data_tones.len())
    }

    /// Pilot tones of control symbols (3 and 20, which cannot be masked).
    pub fn control_pilots(&self) -> Vec<usize> {
        params::pilots_for(BASE_TONES)
    }

    /// Amplitude factor on every active tone of a control symbol (ADR 0005):
    /// a masked control symbol keeps the power of 24 unit-power tones.
    pub fn control_gain(&self) -> f64 {
        (BASE_TONES as f64 / self.control_active_tones.len() as f64).sqrt()
    }

    /// Control symbols, the last one padded.
    pub fn control_symbols(&self) -> usize {
        if self.n_control == 0 {
            0
        } else {
            (self.n_control * CONTROL_CODED_BITS).div_ceil(2 * self.control_tones.len())
        }
    }

    /// Coded data bits in the frame.
    pub fn data_bits(&self) -> usize {
        self.n_blocks * self.rung().coded_bits
    }

    /// Data symbols, the last one padded.
    pub fn data_symbols(&self) -> usize {
        self.data_bits().div_ceil(self.rung().bits_per_point * self.data_tones.len())
    }

    /// Index of the first header symbol.
    pub fn first_header(&self) -> usize {
        PREAMBLE_SYMBOLS
    }

    /// Index of the first probe symbol (or of the first data symbol without a probe).
    pub fn first_probe(&self) -> usize {
        PREAMBLE_SYMBOLS + self.header_symbols()
    }

    /// Index of the first control symbol (or of the first data symbol without control blocks).
    pub fn first_control(&self) -> usize {
        self.first_probe() + self.probe_symbols()
    }

    /// Index of the first data symbol.
    pub fn first_data(&self) -> usize {
        self.first_control() + self.control_symbols()
    }

    /// Symbols in the frame.
    pub fn total_symbols(&self) -> usize {
        self.first_data() + self.data_symbols()
    }

    /// Samples in the frame.
    pub fn n_samples(&self) -> usize {
        self.total_symbols() * self.stride()
    }
}
