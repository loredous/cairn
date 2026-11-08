//! Protocol parameters, mirrored from `docs/SPEC.md` (SPEC 21.3: every
//! algorithm parameter of the core comes from this one file).
//!
//! Section references are to `docs/SPEC.md`; ADR numbers to `adrs/`. The
//! Stage 1 simulation keeps the same numbers in `sim/cairn_sim/params.py`.
//! `tests/spec_tables.rs` parses the specification's tables (6.2, 6.3, 7.1,
//! A.1, A.3) and checks them against this file.

// --- 5.1 / 5.2 waveform -------------------------------------------------------

/// Samples per second (5.1).
pub const FS: u32 = 48_000;
/// Tone spacing in Hz (5.2).
pub const TONE_SPACING_HZ: f64 = 100.0;
/// Useful symbol, 10.0 ms (5.2).
pub const N_USEFUL: usize = 480;
/// Short cyclic prefix, 1.25 ms (5.2).
pub const CP_SHORT: usize = 60;
/// Long cyclic prefix, 2.5 ms: repeater mode or on request (5.2).
pub const CP_LONG: usize = 120;
/// Tone k sits at 350 + 100 k Hz (5.2).
pub const FIRST_TONE_HZ: f64 = 350.0;
/// Highest tone count (5.2).
pub const MAX_TONES: usize = 56;
/// Preamble, header and control blocks use tones 0-23 (5.2).
pub const BASE_TONES: usize = 24;
/// Continuous pilot tones; 37 and 52 only when active (5.2).
pub const PILOT_TONES: [usize; 4] = [3, 20, 37, 52];
/// Pilots 3 and 20 cannot be masked (5.4).
pub const UNMASKABLE_TONES: [usize; 2] = [3, 20];
/// Occupied band starts here (5.2).
pub const BAND_LOW_HZ: f64 = 300.0;
/// Tone k is FFT bin 3.5 + k of the 480-sample useful symbol (5.2, ADR 0005
/// `symbol-waveform`): every tone sits on a half bin, so the prefix is the
/// negated tail of the useful symbol.
pub const BIN_OFFSET: f64 = FIRST_TONE_HZ / TONE_SPACING_HZ;
/// Integer part of [`BIN_OFFSET`]: tone k uses integer bin 3 + k, shifted half a bin.
pub const FIRST_BIN: usize = 3;

/// Centre frequency of tone `k` in Hz.
pub fn tone_freq(k: usize) -> f64 {
    FIRST_TONE_HZ + TONE_SPACING_HZ * k as f64
}

/// Upper edge of the occupied band at a width (5.2): highest tone + 50 Hz.
pub fn band_high_hz(width: usize) -> f64 {
    tone_freq(width - 1) + TONE_SPACING_HZ / 2.0
}

/// Pilot tones below `width`, lowest first.
pub fn pilots_for(width: usize) -> Vec<usize> {
    PILOT_TONES.iter().copied().filter(|&p| p < width).collect()
}

/// Width cap by channel (5.2): 30 tones on 12.5 kHz, 56 on 25 kHz.
pub const WIDTH_CAP_12K5: usize = 30;
/// Width cap on a 25 kHz channel (5.2).
pub const WIDTH_CAP_25K: usize = 56;

// --- 6.1 preamble, 6.4 sequences, 8.1 frame layout ------------------------------

/// Preamble symbols: S1, S1, S2, S2 (6.1).
pub const PREAMBLE_SYMBOLS: usize = 4;
/// Width probe symbols (5.4).
pub const PROBE_SYMBOLS: usize = 2;
/// S1 on base tone k is exp(j 2 pi u[k] / 256) (6.1).
pub const S1_PHASE_UNITS: [u32; BASE_TONES] =
    [181, 160, 172, 183, 202, 203, 204, 79, 7, 184, 91, 108, 195, 191, 29, 125, 88, 203, 26, 202, 223, 107, 168, 82];
/// Denominator of the S1 phase units (6.1).
pub const S1_PHASE_DENOM: u32 = 256;
/// S2 on base tone k is 1 - 2 c(k) with c_init = 5237 (6.1).
pub const S2_C_INIT: u32 = 5237;
/// Pilot j in symbol s carries 1 - 2 c(4 s + j), c_init = 0x0B1D (5.2).
pub const PILOT_C_INIT: u32 = 0x0B1D;
/// Probe symbol r tone k carries 1 - 2 c(56 r + k), c_init = 0x0D0E (6.4).
pub const PROBE_C_INIT: u32 = 0x0D0E;
/// Gold sequence offset Nc (TS 38.211 5.2.1, SPEC 6.4).
pub const GOLD_NC: usize = 1600;
/// Scrambler position of the header; the frame's blocks take 1, 2, ... (6.4).
pub const HEADER_POSITION: u32 = 0;
/// Scrambler position of padding bits: c_init = 256 (6.4).
pub const PAD_POSITION: u32 = 255;

/// Header information bits including the CRC-16 (6.3, 8.2).
pub const HEADER_INFO_BITS: usize = 144;
/// Control block information bits including the CRC-16 (6.3).
pub const CONTROL_INFO_BITS: usize = 256;
/// Data block information bits: 126 payload bytes + CRC-16 (6.3).
pub const DATA_INFO_BITS: usize = 1024;
/// Data block payload bits (6.3, B.4).
pub const DATA_PAYLOAD_BITS: usize = 1008;
/// CRC-16 length (6.3).
pub const CRC_BITS: usize = 16;
/// Longest frame (8.1, 10.2).
pub const MAX_FRAME_S: f64 = 4.0;
/// Most data blocks in a frame (8.1).
pub const MAX_DATA_BLOCKS: usize = 128;
/// Most control blocks in a frame (8.2).
pub const MAX_CONTROL_BLOCKS: usize = 3;

// --- 6.2 MCS ladder, A.3 code lengths, 7.1 clip levels ---------------------------

/// One rung of the 6.2 ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rung {
    /// Rung id.
    pub mcs: u8,
    /// Constellation bits per tone-symbol (Q).
    pub bits_per_point: usize,
    /// Code rate as (numerator, denominator).
    pub code_rate: (u32, u32),
    /// Data block length on air, E (A.3).
    pub coded_bits: usize,
    /// 7 dB-equivalent SNR for 1% block error at 24 tones (6.2, ADR 0003).
    pub threshold_db: f64,
    /// Clip-and-filter level, dB above symbol RMS (7.1, ADR 0003).
    pub clip_db: f64,
}

impl Rung {
    /// Modulation name as in the 6.2 table.
    pub fn modulation(&self) -> &'static str {
        match self.bits_per_point {
            2 => "QPSK",
            4 => "16-QAM",
            6 => "64-QAM",
            8 => "256-QAM",
            _ => unreachable!(),
        }
    }
}

/// The MCS ladder (6.2) with A.3 coded lengths and the ADR 0003 clip levels.
pub const RUNGS: [Rung; 7] = [
    Rung { mcs: 0, bits_per_point: 2, code_rate: (1, 2), coded_bits: 2048, threshold_db: 3.5, clip_db: 7.0 },
    Rung { mcs: 1, bits_per_point: 2, code_rate: (3, 4), coded_bits: 1368, threshold_db: 6.0, clip_db: 7.0 },
    Rung { mcs: 2, bits_per_point: 4, code_rate: (1, 2), coded_bits: 2048, threshold_db: 8.5, clip_db: 7.0 },
    Rung { mcs: 3, bits_per_point: 4, code_rate: (3, 4), coded_bits: 1368, threshold_db: 12.5, clip_db: 7.0 },
    Rung { mcs: 4, bits_per_point: 6, code_rate: (2, 3), coded_bits: 1536, threshold_db: 17.5, clip_db: 8.0 },
    Rung { mcs: 5, bits_per_point: 6, code_rate: (5, 6), coded_bits: 1232, threshold_db: 21.0, clip_db: 9.0 },
    Rung { mcs: 6, bits_per_point: 8, code_rate: (3, 4), coded_bits: 1368, threshold_db: 25.5, clip_db: 10.0 },
];

/// Reference clip level for the 7 dB-equivalent SNR scale (7.1, ADR 0003).
pub const CLIP_DB_REFERENCE: f64 = 7.0;

/// The rung for `mcs`, if it exists.
pub fn rung(mcs: u8) -> Option<&'static Rung> {
    RUNGS.get(mcs as usize)
}

/// dB to add to an SNR measured at rung `mcs` to put it on the 7 dB-clip
/// scale (7.1 "SNR reference", ADR 0003 `snr-reference`).
pub fn clip_offset_db(mcs: u8) -> f64 {
    RUNGS[mcs as usize].clip_db - CLIP_DB_REFERENCE
}

/// Output peak level (7.1).
pub const PEAK_DBFS: f64 = -3.0;

// --- 6.3 / 20.2 LDPC codes ---------------------------------------------------------

/// One of the three lifted, shortened NR LDPC codes (6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSpec {
    /// Name as in `codes/` and `vectors/`.
    pub name: &'static str,
    /// NR base graph, 1 or 2.
    pub base_graph: u8,
    /// Lifting size Z.
    pub lifting: usize,
    /// Information bits after shortening, CRC-16 included (K').
    pub info_bits: usize,
    /// Coded lengths used on air (E).
    pub coded_bits: &'static [usize],
}

impl CodeSpec {
    /// File stem in `codes/`.
    pub fn file_stem(&self) -> &'static str {
        match self.name {
            "header" => "header_bg2_z15",
            "control" => "control_bg2_z26",
            "data" => "data_bg1_z48",
            _ => unreachable!(),
        }
    }

    /// 38.212 Table 5.3.2-1 lifting-size set index for Z.
    pub fn set_index(&self) -> usize {
        let mut odd = self.lifting;
        while odd % 2 == 0 {
            odd /= 2;
        }
        let odd = if odd == 1 { 2 } else { odd };
        [2, 3, 5, 7, 9, 11, 13, 15].iter().position(|&a| a == odd).expect("Z in 38.212 table")
    }
}

/// Header code: BG2, Z = 15, K' = 144; 288 bits (432 held as contingency).
pub const HEADER_CODE: CodeSpec =
    CodeSpec { name: "header", base_graph: 2, lifting: 15, info_bits: HEADER_INFO_BITS, coded_bits: &[288, 432] };
/// Control code: BG2, Z = 26, K' = 256; 512 bits.
pub const CONTROL_CODE: CodeSpec =
    CodeSpec { name: "control", base_graph: 2, lifting: 26, info_bits: CONTROL_INFO_BITS, coded_bits: &[512] };
/// Data code: BG1, Z = 48, K' = 1024; 2048, 1536, 1368 or 1232 bits.
pub const DATA_CODE: CodeSpec = CodeSpec {
    name: "data",
    base_graph: 1,
    lifting: 48,
    info_bits: DATA_INFO_BITS,
    coded_bits: &[2048, 1536, 1368, 1232],
};

/// Coded length of a header (rate 1/2).
pub const HEADER_CODED_BITS: usize = 288;
/// Coded length of a control block (rate 1/2).
pub const CONTROL_CODED_BITS: usize = 512;

/// Layered normalized min-sum iterations (21.2 `ldpc`: 30; 6.3: 20-50).
pub const LDPC_MAX_ITER: usize = 30;
/// Min-sum normalization factor (the Stage 1 decoder's).
pub const LDPC_ALPHA: f32 = 0.75;

/// CRC-16/CCITT-FALSE polynomial (6.3).
pub const CRC_POLY: u16 = 0x1021;
/// CRC-16/CCITT-FALSE initial value (6.3).
pub const CRC_INIT: u16 = 0xFFFF;

// --- Appendix A timing -----------------------------------------------------------

/// TXDelay assumed by Appendix A.
pub const APPENDIX_A_TXDELAY_S: f64 = 0.150;
/// TXTail assumed by the Stage 1 budget (`link_sim.Budget`).
pub const APPENDIX_A_TXTAIL_S: f64 = 0.010;

/// Block symbols available in a frame of `frame_s` seconds (A.1: 330 for 4 s).
pub fn block_symbols_available(frame_s: f64, cp: usize) -> usize {
    let symbol_s = (N_USEFUL + cp) as f64 / FS as f64;
    let usable = frame_s - APPENDIX_A_TXDELAY_S - APPENDIX_A_TXTAIL_S;
    ((usable / symbol_s + 1e-9) as usize) - PREAMBLE_SYMBOLS - header_symbols()
}

/// Header symbols: 288 coded bits over 22 base data tones at QPSK (A.1: 7).
pub const fn header_symbols() -> usize {
    HEADER_CODED_BITS.div_ceil(2 * (BASE_TONES - 2))
}

/// Control block symbols on the unmasked base tones (A.1: 12).
pub const fn control_symbols_per_block() -> usize {
    CONTROL_CODED_BITS.div_ceil(2 * (BASE_TONES - 2))
}

/// Data-block symbols of a DATA frame of `frame_s` seconds at a width with
/// no mask (A.1: 318 / 316 for 4 s): the block symbols less the ACK control
/// block every DATA frame carries (SPEC 8.4) and, above 24 tones, the width
/// probe (ADR 0007).
pub fn data_block_symbols(width: usize, frame_s: f64) -> usize {
    let probe = if width > BASE_TONES { PROBE_SYMBOLS } else { 0 };
    block_symbols_available(frame_s, CP_SHORT) - control_symbols_per_block() - probe
}

/// Data blocks that fit a DATA frame of `frame_s` seconds at a width and
/// rung with no mask (A.2).
pub fn blocks_per_frame(mcs: u8, width: usize, frame_s: f64) -> usize {
    let r = &RUNGS[mcs as usize];
    let data_tones = width - pilots_for(width).len();
    data_block_symbols(width, frame_s) * r.bits_per_point * data_tones / r.coded_bits
}

/// Peer turnaround plus its TXDelay assumed by SPEC 7.4 and Appendix A.
pub const APPENDIX_A_PEER_TURNAROUND_S: f64 = 0.250;

/// Net one-direction throughput of Appendix A.2, bits/s: the blocks of
/// [`blocks_per_frame`] over the cycle of the frame (TXDelay, preamble,
/// header, probe, ACK control block, the data run padded to whole symbols,
/// TXTail) and the 0.51 s peer ACK cycle (turnaround and TXDelay, then
/// preamble, header and one control block).
pub fn net_bps(mcs: u8, width: usize, frame_s: f64) -> f64 {
    let r = &RUNGS[mcs as usize];
    let symbol_s = (N_USEFUL + CP_SHORT) as f64 / FS as f64;
    let blocks = blocks_per_frame(mcs, width, frame_s);
    let data_tones = width - pilots_for(width).len();
    let data_symbols = (blocks * r.coded_bits).div_ceil(r.bits_per_point * data_tones);
    let probe = if width > BASE_TONES { PROBE_SYMBOLS } else { 0 };
    let fixed = PREAMBLE_SYMBOLS + header_symbols() + probe + control_symbols_per_block();
    let frame = APPENDIX_A_TXDELAY_S + (fixed + data_symbols) as f64 * symbol_s + APPENDIX_A_TXTAIL_S;
    let ack = (PREAMBLE_SYMBOLS + header_symbols() + control_symbols_per_block()) as f64 * symbol_s;
    (blocks * DATA_PAYLOAD_BITS) as f64 / (frame + APPENDIX_A_PEER_TURNAROUND_S + ack)
}
