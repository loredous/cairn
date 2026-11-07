//! Frame builder (SPEC 6.4, 7.1, 8.1), mirroring `frame.build` of the Stage 1
//! simulator: header and data blocks through CRC, LDPC, rate matching,
//! scrambling, interleaving and mapping; the tone grid; the time-domain
//! frame with clip-and-filter at the rung's level.

use num_complex::Complex64;

use crate::frame::FrameConfig;
use crate::ofdm::{Grid, Ofdm};
use crate::params::{self, BASE_TONES, HEADER_CODED_BITS, HEADER_POSITION, PREAMBLE_SYMBOLS, PROBE_SYMBOLS};
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
    /// Data block information bits, CRC included (n_blocks x 1024).
    pub block_bits: Vec<Vec<u8>>,
    /// Tone values of every symbol before clip-and-filter.
    pub grid: Grid,
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

    /// The tone grid of a frame (header message 128 bits; payloads 1008 bits each),
    /// with the CRC-appended header and block bits.
    pub fn grid(&self, cfg: &FrameConfig, header_msg: &[u8], payloads: &[Vec<u8>]) -> (Grid, Vec<u8>, Vec<Vec<u8>>) {
        assert_eq!(header_msg.len(), params::HEADER_INFO_BITS - params::CRC_BITS);
        assert_eq!(payloads.len(), cfg.n_blocks);
        let header_bits = crc::append_crc(header_msg);
        let hpoints = header_points(cfg, &header_bits);

        let rung = cfg.rung();
        let dcode = ldpc::data_code();
        let mut block_bits = Vec::with_capacity(cfg.n_blocks);
        let mut stream = Vec::with_capacity(cfg.data_symbols() * cfg.data_tones.len() * rung.bits_per_point);
        for (i, p) in payloads.iter().enumerate() {
            assert_eq!(p.len(), params::DATA_PAYLOAD_BITS);
            let st = block_stages(dcode, p, rung.coded_bits, 1 + i as u32);
            stream.extend_from_slice(&st.interleaved);
            block_bits.push(st.info);
        }
        let d_res = cfg.data_symbols() * cfg.data_tones.len() * rung.bits_per_point;
        stream.extend(gold::pad_bits(d_res - stream.len()));
        let dpoints = mapping::modulate(&stream, rung.bits_per_point);

        let total = cfg.total_symbols();
        let table = gold::pilot_table(total);
        let mut grid = Grid::zeros(total, cfg.width);
        let pre = preamble::grid(cfg.width);
        grid.data[..pre.data.len()].copy_from_slice(&pre.data);
        let base_pilots = params::pilots_for(BASE_TONES);
        fill_symbols(&mut grid, PREAMBLE_SYMBOLS, &hpoints, &cfg.base_data_tones, &base_pilots, &table, 1.0);
        if cfg.has_probe {
            fill_probe(&mut grid, cfg.first_probe(), &table);
        }
        fill_symbols(&mut grid, cfg.first_data(), &dpoints, &cfg.data_tones, &cfg.data_pilots, &table, cfg.data_gain());
        (grid, header_bits, block_bits)
    }

    /// Build a frame: grid, then samples with clip-and-filter per segment
    /// (preamble and header on the base tones, the probe on every tone up to
    /// the width, data on the active tones), all at the width's scale.
    pub fn build(&mut self, cfg: &FrameConfig, header_msg: &[u8], payloads: &[Vec<u8>]) -> TxFrame {
        let (grid, header_bits, block_bits) = self.grid(cfg, header_msg, payloads);
        let scale = cfg.width;
        let (fp, fd) = (cfg.first_probe(), cfg.first_data());
        let base: Vec<usize> = (0..BASE_TONES).collect();
        let all: Vec<usize> = (0..cfg.width).collect();
        let segments: [(usize, usize, &[usize]); 3] =
            [(0, fp, &base), (fp, fd, &all), (fd, grid.rows, &cfg.data_active_tones)];
        let mut samples = Vec::with_capacity(cfg.n_samples());
        for (a, b, tones) in segments {
            if a == b {
                continue;
            }
            let x = self.ofdm.modulate(&grid.slice_rows(a, b), cfg.cp, scale);
            if cfg.clip {
                samples.extend(self.ofdm.clip_and_filter(&x, cfg.width, tones, cfg.cp, cfg.clip_db, scale));
            } else {
                samples.extend(x);
            }
        }
        TxFrame { config: cfg.clone(), samples, header_bits, block_bits, grid }
    }
}
