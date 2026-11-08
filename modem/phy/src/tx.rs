//! Frame builder (SPEC 6.4, 7.1, 8.1), mirroring `frame.build` of the Stage 1
//! simulator: header, control and data blocks through CRC, LDPC, rate
//! matching, scrambling, interleaving and mapping; the tone grid; the
//! time-domain frame with clip-and-filter at the rung's level (7 dB without
//! data blocks). [`Transmitter::build_frame`] builds the frame a SPEC 8.2
//! header describes, with its Appendix B control blocks.

use std::fmt;

use num_complex::Complex64;

use crate::fields::{ControlBlock, FieldError, Header};
use crate::frame::{ConfigError, FrameConfig, LinkState};
use crate::ofdm::{Grid, Ofdm};
use crate::params::{
    self, BASE_TONES, CONTROL_CODED_BITS, CP_SHORT, HEADER_CODED_BITS, HEADER_POSITION, PREAMBLE_SYMBOLS, PROBE_SYMBOLS,
};
use crate::{crc, gold, interleave, ldpc, mapping, preamble};

const QPSK: usize = 2;

/// One block's bits through the transmit chain (SPEC 6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockStages {
    /// Information bits with CRC.
    pub info: Vec<u8>,
    /// After LDPC encoding and rate matching (E bits).
    pub rate_matched: Vec<u8>,
    /// The scrambling sequence c(0 .. E-1).
    pub scrambler: Vec<u8>,
    /// Rate-matched XOR scrambler.
    pub scrambled: Vec<u8>,
    /// Scrambled bits permuted by the block interleaver.
    pub interleaved: Vec<u8>,
}

/// Encode a block at scrambler `position`: CRC is appended to `message`.
pub fn block_stages(code: &ldpc::LiftedCode, message: &[u8], e: usize, position: u32) -> BlockStages {
    let info = crc::append_crc(message);
    let rate_matched = code.encode_rate_matched(&info, e);
    let scrambler = gold::scramble_sequence(position, e);
    let scrambled: Vec<u8> = rate_matched.iter().zip(&scrambler).map(|(a, b)| a ^ b).collect();
    let interleaved = interleave::interleave(&scrambled);
    BlockStages { info, rate_matched, scrambler, scrambled, interleaved }
}

/// The header's interleaved bits and padding for a frame (QPSK on the 22 base data tones).
pub fn header_stream(cfg: &FrameConfig, header_info: &[u8]) -> Vec<u8> {
    let code = ldpc::header_code();
    let tx = code.encode_rate_matched(header_info, HEADER_CODED_BITS);
    let mut bits: Vec<u8> =
        tx.iter().zip(gold::scramble_sequence(HEADER_POSITION, tx.len())).map(|(a, b)| a ^ b).collect();
    bits = interleave::interleave(&bits);
    let res = cfg.header_symbols() * cfg.base_data_tones.len() * QPSK;
    bits.extend(gold::pad_bits(res - bits.len()));
    bits
}

/// The header's QPSK points (padding included) for header information bits (CRC included).
pub fn header_points(cfg: &FrameConfig, header_info: &[u8]) -> Vec<Complex64> {
    mapping::modulate(&header_stream(cfg, header_info), QPSK)
}

/// The control segment's interleaved bits and padding: blocks (information
/// bits, CRC included) at scrambler positions 1 .. n_control, QPSK on the
/// control tones.
pub fn control_stream(cfg: &FrameConfig, control_infos: &[Vec<u8>]) -> Vec<u8> {
    let code = ldpc::control_code();
    let mut bits = Vec::with_capacity(cfg.control_symbols() * cfg.control_tones.len() * QPSK);
    for (i, info) in control_infos.iter().enumerate() {
        let tx = code.encode_rate_matched(info, CONTROL_CODED_BITS);
        let scrambled: Vec<u8> =
            tx.iter().zip(gold::scramble_sequence(1 + i as u32, tx.len())).map(|(a, b)| a ^ b).collect();
        bits.extend(interleave::interleave(&scrambled));
    }
    let res = cfg.control_symbols() * cfg.control_tones.len() * QPSK;
    bits.extend(gold::pad_bits(res - bits.len()));
    bits
}

/// Why a frame could not be built from a header.
#[derive(Clone, Debug, PartialEq)]
pub enum BuildError {
    /// A header or control block field is out of range.
    Field(FieldError),
    /// The header describes no frame at this link state.
    Config(ConfigError),
    /// Control blocks or payloads disagree with the header's counts.
    Count {
        /// What disagrees.
        what: &'static str,
        /// Count in the header.
        header: usize,
        /// Count given.
        given: usize,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Field(e) => write!(f, "{e}"),
            BuildError::Config(e) => write!(f, "{e}"),
            BuildError::Count { what, header, given } => write!(f, "{given} {what} for a header of {header}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<FieldError> for BuildError {
    fn from(e: FieldError) -> Self {
        BuildError::Field(e)
    }
}

impl From<ConfigError> for BuildError {
    fn from(e: ConfigError) -> Self {
        BuildError::Config(e)
    }
}

/// Pilot and data tones of one run of symbols.
fn fill_symbols(
    grid: &mut Grid,
    first_row: usize,
    points: &[Complex64],
    data_tones: &[usize],
    pilots: &[usize],
    pilot_table: &[[f64; 4]],
    gain: f64,
) {
    let d = data_tones.len();
    let n_sym = points.len() / d;
    for s in 0..n_sym {
        let row = first_row + s;
        for (slot, &k) in data_tones.iter().enumerate() {
            let p = points[s * d + slot];
            *grid.at_mut(row, k) = Complex64::new(p.re * gain, p.im * gain);
        }
        for &p in pilots {
            *grid.at_mut(row, p) = Complex64::new(gold::pilot_value(pilot_table, row, p) * gain, 0.0);
        }
    }
}

/// Width-probe symbols (SPEC 5.4, 6.4) as rows `first_symbol` and `first_symbol + 1`
/// of `grid`: 1 - 2 c(56 r + k) on every tone, pilots on the pilot tones.
pub fn fill_probe(grid: &mut Grid, first_symbol: usize, pilot_table: &[[f64; 4]]) {
    let width = grid.cols;
    let values = gold::probe_values();
    let pilots = params::pilots_for(width);
    for r in 0..PROBE_SYMBOLS {
        let row = first_symbol + r;
        for k in 0..width {
            *grid.at_mut(row, k) = Complex64::new(values[params::MAX_TONES * r + k], 0.0);
        }
        for &p in &pilots {
            *grid.at_mut(row, p) = Complex64::new(gold::pilot_value(pilot_table, row, p), 0.0);
        }
    }
}

/// The probe's known values (2, width), pilots included.
pub fn probe_grid(width: usize, first_symbol: usize) -> Grid {
    let table = gold::pilot_table(first_symbol + PROBE_SYMBOLS);
    let mut g = Grid::zeros(first_symbol + PROBE_SYMBOLS, width);
    fill_probe(&mut g, first_symbol, &table);
    g.slice_rows(first_symbol, first_symbol + PROBE_SYMBOLS)
}

/// A frame's tone grid and the CRC-appended bits of its blocks.
#[derive(Clone, Debug)]
pub struct FrameGrid {
    /// Tone values of every symbol.
    pub grid: Grid,
    /// Header information bits, CRC included (144).
    pub header_bits: Vec<u8>,
    /// Control block information bits, CRC included (n_control x 256).
    pub control_bits: Vec<Vec<u8>>,
    /// Data block information bits, CRC included (n_blocks x 1024).
    pub block_bits: Vec<Vec<u8>>,
}

/// A built frame.
#[derive(Clone, Debug)]
pub struct TxFrame {
    /// The configuration it was built for.
    pub config: FrameConfig,
    /// Time-domain samples at 48 kHz, the simulator's scale (unit mean power
    /// before clip-and-filter); see [`TxFrame::output`] for the -3 dBFS output.
    pub samples: Vec<f64>,
    /// Header information bits, CRC included (144).
    pub header_bits: Vec<u8>,
    /// Control block information bits, CRC included (n_control x 256).
    pub control_bits: Vec<Vec<u8>>,
    /// Data block information bits, CRC included (n_blocks x 1024).
    pub block_bits: Vec<Vec<u8>>,
    /// Tone values of every symbol before clip-and-filter.
    pub grid: Grid,
    /// The header's fields, when built from them ([`Transmitter::build_frame`]).
    pub header: Option<Header>,
}

impl TxFrame {
    /// The frame scaled so its peak sample is at `PEAK_DBFS` (SPEC 7.1).
    pub fn output(&self) -> Vec<f64> {
        let mut out = self.samples.clone();
        crate::ofdm::scale_to_peak(&mut out, params::PEAK_DBFS);
        out
    }
}

/// Frame builder with reusable FFT plans.
pub struct Transmitter {
    ofdm: Ofdm,
}

impl Default for Transmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Transmitter {
    /// A transmitter.
    pub fn new() -> Self {
        Transmitter { ofdm: Ofdm::new() }
    }

    /// The tone grid of a frame without control blocks (header message 128
    /// bits; payloads 1008 bits each), with the CRC-appended header and block bits.
    pub fn grid(&self, cfg: &FrameConfig, header_msg: &[u8], payloads: &[Vec<u8>]) -> (Grid, Vec<u8>, Vec<Vec<u8>>) {
        let g = self.grid_with_controls(cfg, header_msg, &[], payloads);
        (g.grid, g.header_bits, g.block_bits)
    }

    /// The tone grid of a frame (header message 128 bits, control messages
    /// 240 bits each, payloads 1008 bits each), with the CRC-appended header,
    /// control and block bits.
    pub fn grid_with_controls(
        &self,
        cfg: &FrameConfig,
        header_msg: &[u8],
        control_msgs: &[Vec<u8>],
        payloads: &[Vec<u8>],
    ) -> FrameGrid {
        assert_eq!(header_msg.len(), params::HEADER_INFO_BITS - params::CRC_BITS);
        assert_eq!(control_msgs.len(), cfg.n_control, "control block count");
        assert_eq!(payloads.len(), cfg.n_blocks);
        let header_bits = crc::append_crc(header_msg);
        let hpoints = header_points(cfg, &header_bits);
        let control_bits: Vec<Vec<u8>> = control_msgs
            .iter()
            .map(|m| {
                assert_eq!(m.len(), params::CONTROL_INFO_BITS - params::CRC_BITS);
                crc::append_crc(m)
            })
            .collect();
        let cpoints = mapping::modulate(&control_stream(cfg, &control_bits), QPSK);

        let rung = cfg.rung();
        let dcode = ldpc::data_code();
        let mut block_bits = Vec::with_capacity(cfg.n_blocks);
        let mut stream = Vec::with_capacity(cfg.data_symbols() * cfg.data_tones.len() * rung.bits_per_point);
        for (i, p) in payloads.iter().enumerate() {
            assert_eq!(p.len(), params::DATA_PAYLOAD_BITS);
            let st = block_stages(dcode, p, rung.coded_bits, (1 + cfg.n_control + i) as u32);
            stream.extend_from_slice(&st.interleaved);
            block_bits.push(st.info);
        }
        let d_res = cfg.data_symbols() * cfg.data_tones.len() * rung.bits_per_point;
        stream.extend(gold::pad_bits(d_res - stream.len()));
        let dpoints = mapping::modulate(&stream, rung.bits_per_point);

        let total = cfg.total_symbols();
        let table = gold::pilot_table(total);
        // The probe may reach above the width, to the sender's cap (SPEC 5.4).
        let mut grid = Grid::zeros(total, cfg.n_tones());
        let pre = preamble::grid(cfg.n_tones());
        grid.data[..pre.data.len()].copy_from_slice(&pre.data);
        let base_pilots = params::pilots_for(BASE_TONES);
        fill_symbols(&mut grid, PREAMBLE_SYMBOLS, &hpoints, &cfg.base_data_tones, &base_pilots, &table, 1.0);
        if cfg.has_probe {
            fill_probe(&mut grid, cfg.first_probe(), &table);
        }
        let cpilots = cfg.control_pilots();
        fill_symbols(
            &mut grid,
            cfg.first_control(),
            &cpoints,
            &cfg.control_tones,
            &cpilots,
            &table,
            cfg.control_gain(),
        );
        fill_symbols(&mut grid, cfg.first_data(), &dpoints, &cfg.data_tones, &cfg.data_pilots, &table, cfg.data_gain());
        FrameGrid { grid, header_bits, control_bits, block_bits }
    }

    /// Build a frame without control blocks: grid, then samples with
    /// clip-and-filter per segment (see [`Transmitter::build_with_controls`]).
    pub fn build(&mut self, cfg: &FrameConfig, header_msg: &[u8], payloads: &[Vec<u8>]) -> TxFrame {
        self.build_with_controls(cfg, header_msg, &[], payloads)
    }

    /// Build the frame a SPEC 8.2 header describes at the link's width, the
    /// mask its Mask ID names (ADR 0011), the probe's extent and the prefix
    /// (short unless `link.cp` says otherwise), with its control blocks and
    /// payloads (1008 bits each; [`crate::pdu::block_bits`]). The k-th
    /// payload is the data block the block map names k-th
    /// ([`crate::fields::plan_blocks`] gives the header's Sequence and the
    /// map; the map must agree with the header,
    /// [`crate::fields::check_block_map`]). Header flag bit 4 is set when the
    /// data uses tones above 23 (ADR 0007, ADR 0011); [`TxFrame::header`]
    /// holds the header as sent.
    pub fn build_frame(
        &mut self,
        header: &Header,
        controls: &[ControlBlock],
        payloads: &[Vec<u8>],
        link: &LinkState,
    ) -> Result<TxFrame, BuildError> {
        crate::fields::check_block_map(header, controls)?;
        let mut header = header.clone();
        if !header.probe {
            // ADR 0007: a frame whose data uses tones above 23 carries the probe.
            let probed = Header { probe: true, ..header.clone() };
            if FrameConfig::from_header(&probed, link, link.cp.unwrap_or(CP_SHORT))?.needs_probe() {
                header = probed;
            }
        }
        let header = &header;
        let cfg = FrameConfig::from_header(header, link, link.cp.unwrap_or(CP_SHORT))?;
        if controls.len() != header.n_control {
            return Err(BuildError::Count { what: "control blocks", header: header.n_control, given: controls.len() });
        }
        if payloads.len() != header.n_data {
            return Err(BuildError::Count { what: "data blocks", header: header.n_data, given: payloads.len() });
        }
        let msgs = controls.iter().map(ControlBlock::message_bits).collect::<Result<Vec<_>, _>>()?;
        let mut frame = self.build_with_controls(&cfg, &header.message_bits()?, &msgs, payloads);
        frame.header = Some(header.clone());
        Ok(frame)
    }

    /// Build a frame: grid, then samples with clip-and-filter per segment
    /// (preamble and header on the base tones, the probe on every tone up to
    /// the cap, control blocks on the active base tones, data on the active
    /// tones), all at the width's scale.
    pub fn build_with_controls(
        &mut self,
        cfg: &FrameConfig,
        header_msg: &[u8],
        control_msgs: &[Vec<u8>],
        payloads: &[Vec<u8>],
    ) -> TxFrame {
        let FrameGrid { grid, header_bits, control_bits, block_bits } =
            self.grid_with_controls(cfg, header_msg, control_msgs, payloads);
        let scale = cfg.width;
        let (fp, fc, fd) = (cfg.first_probe(), cfg.first_control(), cfg.first_data());
        let base: Vec<usize> = (0..BASE_TONES).collect();
        let all: Vec<usize> = (0..cfg.cap).collect();
        let segments: [(usize, usize, &[usize]); 4] = [
            (0, fp, &base),
            (fp, fc, &all),
            (fc, fd, &cfg.control_active_tones),
            (fd, grid.rows, &cfg.data_active_tones),
        ];
        let mut samples = Vec::with_capacity(cfg.n_samples());
        for (a, b, tones) in segments {
            if a == b {
                continue;
            }
            let x = self.ofdm.modulate(&grid.slice_rows(a, b), cfg.cp, scale);
            if cfg.clip {
                samples.extend(self.ofdm.clip_and_filter(&x, cfg.n_tones(), tones, cfg.cp, cfg.clip_db, scale));
            } else {
                samples.extend(x);
            }
        }
        TxFrame { config: cfg.clone(), samples, header_bits, control_bits, block_bits, grid, header: None }
    }
}
