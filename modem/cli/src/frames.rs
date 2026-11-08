//! Test frames and the level-setting tone (SPEC 7.1), and the
//! `cairn-bench/1` manifest that lets `cairn-bench rx` score a WAV of them.

use std::fmt::Write as _;

use cairn_phy::params::{self, CP_LONG, CP_SHORT, FS};
use cairn_phy::{FrameConfig, Transmitter, TxFrame};

use crate::nprng::Pcg64;

/// Level-setting tone frequency (SPEC 7.1).
pub const LEVEL_TONE_HZ: f64 = 1000.0;

/// The data waveform's peak amplitude (SPEC 7.1: -3 dBFS).
pub fn peak_amplitude() -> f64 {
    10f64.powf(params::PEAK_DBFS / 20.0)
}

/// What to send.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSpec {
    /// Rung.
    pub mcs: u8,
    /// Width, tones.
    pub width: usize,
    /// Prefix, samples (60 or 120).
    pub cp: usize,
    /// Tones silenced in data symbols (SPEC 5.4).
    pub silenced: Vec<usize>,
    /// Block count; `None`: as many as fit `frame_s`.
    pub blocks: Option<usize>,
    /// Target frame time, s; `None`: SPEC 10.2 (2 s at MCS 0-1, else 4 s).
    pub frame_s: Option<f64>,
}

impl FrameSpec {
    /// The default frame time of SPEC 10.2 / Appendix A.
    pub fn default_frame_s(mcs: u8) -> f64 {
        if mcs <= 1 { 2.0 } else { 4.0 }
    }

    fn base(&self, n_blocks: usize) -> Result<FrameConfig, String> {
        let mask = if self.silenced.is_empty() {
            None
        } else {
            Some((0..self.width).filter(|k| !self.silenced.contains(k)).collect())
        };
        FrameConfig::new(self.width, self.mcs, n_blocks)
            .and_then(|c| c.with_cp(self.cp))
            .and_then(|c| c.with_mask(mask))
            .map_err(|e| e.to_string())
    }

    /// Blocks per frame, as `cairn-bench tx` counts them (`link_sim.Budget`
    /// without the ACK block: the frame time less Appendix A's TXDelay and
    /// TXTail, preamble, header and, when the data uses tones above 23, the
    /// probe of ADR 0007), less `control_symbols` for control blocks sent.
    pub fn n_blocks_with(&self, control_symbols: usize) -> Result<usize, String> {
        if let Some(b) = self.blocks {
            return Ok(b);
        }
        let cfg = self.base(1)?;
        let frame_s = self.frame_s.unwrap_or(Self::default_frame_s(self.mcs));
        let avail = params::block_symbols_available(frame_s, self.cp)
            .saturating_sub(cfg.probe_symbols() + control_symbols)
            .max(1);
        let rung = cfg.rung();
        let per_symbol = rung.bits_per_point * cfg.data_tones.len();
        Ok((avail * per_symbol / rung.coded_bits).max(1))
    }

    /// [`FrameSpec::n_blocks_with`] for a frame without control blocks.
    pub fn n_blocks(&self) -> Result<usize, String> {
        self.n_blocks_with(0)
    }

    /// The frame configuration.
    pub fn config(&self) -> Result<FrameConfig, String> {
        self.base(self.n_blocks()?)
    }

    /// `short` or `long`.
    pub fn prefix_name(&self) -> &'static str {
        if self.cp == CP_LONG { "long" } else { "short" }
    }
}

/// Parse `8-11,30` into tone indices.
pub fn parse_tones(text: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (lo, hi) = part.split_once('-').unwrap_or((part, part));
        let lo: usize = lo.trim().parse().map_err(|_| format!("bad tone list {text:?}"))?;
        let hi: usize = hi.trim().parse().map_err(|_| format!("bad tone list {text:?}"))?;
        out.extend(lo..=hi);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Parse `short`/`long`/`60`/`120`.
pub fn parse_prefix(s: &str) -> Result<usize, String> {
    match s {
        "short" | "60" => Ok(CP_SHORT),
        "long" | "120" => Ok(CP_LONG),
        _ => Err(format!("prefix must be short or long, not {s:?}")),
    }
}

/// Build a frame with the simulator's random header and payloads for
/// `seed` (`frame.build(cfg, np.random.default_rng(seed))`).
///
/// The header bits are random until the link layer assembles real header
/// fields (SPEC 8.2); a header-first receiver will need those.
pub fn build_frame(tx: &mut Transmitter, cfg: &FrameConfig, seed: u64) -> TxFrame {
    let mut rng = Pcg64::new(seed);
    let header = rng.bits(params::HEADER_INFO_BITS - params::CRC_BITS);
    let flat = rng.bits(cfg.n_blocks * params::DATA_PAYLOAD_BITS);
    let payloads: Vec<Vec<u8>> = flat.chunks(params::DATA_PAYLOAD_BITS).map(<[u8]>::to_vec).collect();
    tx.build(cfg, &header, &payloads)
}

/// SPEC 7.1 level-setting tone: 1000 Hz at the data's peak amplitude, with
/// 20 ms raised-cosine ends (as `cairn-bench tx`).
pub fn level_tone(seconds: f64) -> Vec<f64> {
    let n = (seconds * FS as f64).round() as usize;
    let peak = peak_amplitude();
    let w = 2.0 * std::f64::consts::PI * LEVEL_TONE_HZ / FS as f64;
    let mut x: Vec<f64> = (0..n).map(|i| peak * (w * i as f64).sin()).collect();
    let r = ((0.02 * FS as f64) as usize).min(n / 2);
    for i in 0..r {
        let g = 0.5 - 0.5 * (std::f64::consts::PI * (i as f64 + 0.5) / r as f64).cos();
        x[i] *= g;
        x[n - 1 - i] *= g;
    }
    x
}

/// One frame placed in a WAV, for the manifest.
#[derive(Clone, Debug)]
pub struct PlacedFrame {
    /// Order sent.
    pub index: usize,
    /// What it was.
    pub spec: FrameSpec,
    /// Its configuration.
    pub cfg: FrameConfig,
    /// Payload seed.
    pub seed: u64,
    /// First sample in the WAV.
    pub start: usize,
    /// Length.
    pub n_samples: usize,
    /// Peak and RMS of the scaled frame, dBFS.
    pub peak_dbfs: f64,
    /// RMS, dBFS.
    pub rms_dbfs: f64,
}

/// A level tone placed in a WAV.
#[derive(Clone, Copy, Debug)]
pub struct PlacedTone {
    /// First sample.
    pub start: usize,
    /// Length, s.
    pub seconds: f64,
}

fn num(x: f64) -> String {
    if x.is_finite() { format!("{x}") } else { "null".into() }
}

/// A `cairn-bench/1` manifest (the format `cairn-bench tx` writes).
pub fn manifest_json(
    frames: &[PlacedFrame],
    tone: Option<PlacedTone>,
    total_samples: usize,
    seed: u64,
    lead_s: f64,
    gap_s: f64,
) -> String {
    let fs = FS as f64;
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, " \"format\": \"cairn-bench/1\",\n \"fs\": {FS},\n \"peak_dbfs\": {},", num(params::PEAK_DBFS));
    let _ = writeln!(s, " \"seed\": {seed},\n \"lead_s\": {},\n \"gap_s\": {},", num(lead_s), num(gap_s));
    let _ = writeln!(s, " \"callsign\": null,\n \"generator\": \"cairn-modem tx\",");
    let _ = writeln!(s, " \"n_samples\": {total_samples},\n \"duration_s\": {},", num(total_samples as f64 / fs));
    match tone {
        Some(t) => {
            let _ = writeln!(
                s,
                " \"level_tone\": {{\"start_s\": {}, \"duration_s\": {}, \"freq_hz\": {}, \"peak_dbfs\": {}}},",
                num(t.start as f64 / fs),
                num(t.seconds),
                num(LEVEL_TONE_HZ),
                num(params::PEAK_DBFS)
            );
        }
        None => s.push_str(" \"level_tone\": null,\n"),
    }
    s.push_str(" \"cw_ids\": [],\n \"frames\": [\n");
    for (i, f) in frames.iter().enumerate() {
        let silenced = f.spec.silenced.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ");
        let _ = write!(
            s,
            "  {{\"index\": {}, \"mcs\": {}, \"width\": {}, \"prefix\": \"{}\", \"cp\": {}, \"silenced\": [{}], \
             \"n_blocks\": {}, \"seed\": {}, \"start_s\": {}, \"start_sample\": {}, \"n_samples\": {}, \
             \"duration_s\": {}, \"clip_db\": {}, \"peak_dbfs\": {}, \"rms_dbfs\": {}}}",
            f.index,
            f.spec.mcs,
            f.spec.width,
            f.spec.prefix_name(),
            f.cfg.cp,
            silenced,
            f.cfg.n_blocks,
            f.seed,
            num(f.start as f64 / fs),
            f.start,
            f.n_samples,
            num(f.n_samples as f64 / fs),
            num(f.cfg.clip_db),
            num(f.peak_dbfs),
            num(f.rms_dbfs),
        );
        s.push_str(if i + 1 < frames.len() { ",\n" } else { "\n" });
    }
    s.push_str(" ]\n}\n");
    s
}

/// Peak and RMS of `x`, dBFS.
pub fn peak_rms_dbfs(x: &[f64]) -> (f64, f64) {
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let rms = (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt();
    (cairn_audio::dbfs(peak), cairn_audio::dbfs(rms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_counts_match_appendix_a() {
        // SPEC Appendix A.2, 4 s frames at 24 tones: MCS 2 carries 14 blocks, MCS 6 42.
        let s = |mcs, width| FrameSpec { mcs, width, cp: CP_SHORT, silenced: vec![], blocks: None, frame_s: Some(4.0) };
        assert_eq!(s(2, 24).n_blocks().unwrap(), 14);
        assert_eq!(s(6, 24).n_blocks().unwrap(), 42);
        // 56 tones: the probe of every wide frame (ADR 0007) costs 2 symbols, as in link_sim.Budget.
        assert_eq!(s(6, 56).n_blocks().unwrap(), 99);
        assert_eq!(s(3, 56).n_blocks().unwrap(), 49);
        assert_eq!(parse_tones("8-11, 30").unwrap(), vec![8, 9, 10, 11, 30]);
    }

    #[test]
    fn tone_peak() {
        let t = level_tone(0.5);
        let peak = t.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((peak - peak_amplitude()).abs() < 1e-3);
        assert!(t[0].abs() < 1e-3 && t[t.len() - 1].abs() < 1e-3);
    }
}
