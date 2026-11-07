//! The three 5G NR LDPC codes as Cairn uses them (SPEC 6.3, 20.2, A.3).
//!
//! - The lifted matrices come from the published `codes/<stem>_lifted.txt`
//!   files (block row, block column, shift; block (i, j) is the Z x Z
//!   identity with the one of row z in column j Z + (z + shift) mod Z).
//! - Shortening: information bits K' .. K-1 are filler zeros, never sent.
//! - Rate matching is 38.212 5.4.2 with rv0 and no limited buffer: the first
//!   2Z codeword bits are punctured, the rest form a circular buffer with the
//!   fillers skipped, and E bits are read from it.
//! - Decoding is layered normalized min-sum on `f32`, exactly the Stage 1
//!   decoder's variant (`sim/cairn_sim/ldpc.py`): rows in base-graph order,
//!   alpha 0.75, a tie for the minimum giving min2 = min1, early stop once the
//!   CRC passes and the syndrome over the decoded rows is satisfied.
//!
//! LLR convention: L = log P(bit = 0) / P(bit = 1).

use std::sync::OnceLock;

use crate::crc;
use crate::params::{self, CodeSpec};

/// LLR of a known zero (filler): far above any channel LLR, finite in f32.
pub const FILLER_LLR: f32 = 1.0e20;

const HEADER_LIFTED: &str = include_str!("../../../codes/header_bg2_z15_lifted.txt");
const CONTROL_LIFTED: &str = include_str!("../../../codes/control_bg2_z26_lifted.txt");
const DATA_LIFTED: &str = include_str!("../../../codes/data_bg1_z48_lifted.txt");

/// A lifted, shortened code with its encoder and decoder tables.
#[derive(Debug)]
pub struct LiftedCode {
    /// The code's parameters.
    pub spec: CodeSpec,
    /// Lifting size Z.
    pub z: usize,
    /// Base-graph rows.
    pub base_rows: usize,
    /// Base-graph columns.
    pub base_cols: usize,
    /// Information columns of the base graph (22 for BG1, 10 for BG2).
    pub info_cols: usize,
    /// K = info_cols Z.
    pub k_native: usize,
    /// K', the real information bits (CRC included).
    pub k: usize,
    /// N = base_cols Z, before puncturing.
    pub n_codeword: usize,
    /// Per base row: (block column, shift) pairs in file order.
    pub layers: Vec<Vec<(usize, usize)>>,
    /// Circular buffer: codeword positions after the 2Z punctured bits, fillers removed.
    pub buffer: Vec<usize>,
    /// Inverse of the 4Z x 4Z core parity block, rows as bit words.
    core_inv: Vec<Vec<u64>>,
}

fn parse_lifted(text: &str) -> Vec<(usize, usize, usize)> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let v: Vec<usize> = l.split_whitespace().map(|t| t.parse().expect("integer")).collect();
            assert_eq!(v.len(), 3, "lifted line {l:?}");
            (v[0], v[1], v[2])
        })
        .collect()
}

/// GF(2) inverse of a square matrix of 0/1 rows, as packed u64 words.
fn gf2_inverse(a: &[Vec<u8>]) -> Vec<Vec<u64>> {
    let n = a.len();
    let words = (2 * n).div_ceil(64);
    let mut m: Vec<Vec<u64>> = (0..n)
        .map(|i| {
            let mut row = vec![0u64; words];
            for (j, &v) in a[i].iter().enumerate() {
                if v != 0 {
                    row[j / 64] |= 1 << (j % 64);
                }
            }
            row[(n + i) / 64] |= 1 << ((n + i) % 64);
            row
        })
        .collect();
    for col in 0..n {
        let (w, b) = (col / 64, 1u64 << (col % 64));
        let pivot = (col..n).find(|&r| m[r][w] & b != 0).expect("core parity block is singular");
        m.swap(col, pivot);
        let prow = m[col].clone();
        for (r, row) in m.iter_mut().enumerate() {
            if r != col && row[w] & b != 0 {
                for (x, y) in row.iter_mut().zip(&prow) {
                    *x ^= y;
                }
            }
        }
    }
    let out_words = n.div_ceil(64);
    m.iter()
        .map(|row| {
            let mut out = vec![0u64; out_words];
            for j in 0..n {
                let src = n + j;
                if row[src / 64] >> (src % 64) & 1 == 1 {
                    out[j / 64] |= 1 << (j % 64);
                }
            }
            out
        })
        .collect()
}

impl LiftedCode {
    fn build(spec: CodeSpec, text: &str) -> Self {
        let z = spec.lifting;
        let (base_rows, base_cols, info_cols) = match spec.base_graph {
            1 => (46, 68, 22),
            2 => (42, 52, 10),
            _ => panic!("no base graph {}", spec.base_graph),
        };
        let entries = parse_lifted(text);
        let mut layers = vec![Vec::new(); base_rows];
        for (i, j, s) in entries {
            assert!(i < base_rows && j < base_cols && s < z, "lifted entry out of range");
            layers[i].push((j, s));
        }
        let k_native = info_cols * z;
        let k = spec.info_bits;
        let n_codeword = base_cols * z;
        let buffer: Vec<usize> = (2 * z..n_codeword).filter(|&p| !(k <= p && p < k_native)).collect();

        // Structure the encoder relies on (true of both NR base graphs): the
        // four core rows touch only the information and four core parity
        // columns, and each later row i owns parity column info_cols + i,
        // which no other row uses, and touches no other extension column.
        for (i, layer) in layers.iter().enumerate() {
            for &(j, _) in layer {
                if i < 4 {
                    assert!(j < info_cols + 4, "core row {i} touches column {j}");
                } else {
                    assert!(j < info_cols + 4 || j == info_cols + i, "row {i} touches column {j}");
                }
            }
            if i >= 4 {
                assert!(layer.iter().any(|&(j, _)| j == info_cols + i));
            }
        }
        let n_core = 4 * z;
        let mut a = vec![vec![0u8; n_core]; n_core];
        for (i, layer) in layers.iter().take(4).enumerate() {
            for &(j, s) in layer {
                if j >= info_cols {
                    let c0 = (j - info_cols) * z;
                    for r in 0..z {
                        a[i * z + r][c0 + (r + s) % z] ^= 1;
                    }
                }
            }
        }
        let core_inv = gf2_inverse(&a);
        LiftedCode { spec, z, base_rows, base_cols, info_cols, k_native, k, n_codeword, layers, buffer, core_inv }
    }

    /// Full codeword (N bits: information, fillers, parity) of K' information bits.
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        assert_eq!(info.len(), self.k, "{} code takes {} information bits", self.spec.name, self.k);
        let z = self.z;
        let mut c = vec![0u8; self.n_codeword];
        c[..self.k].copy_from_slice(info);
        let row_sum = |c: &[u8], layer: &[(usize, usize)], r: usize, skip: usize| -> u8 {
            let mut acc = 0u8;
            for &(j, s) in layer {
                if j != skip {
                    acc ^= c[j * z + (r + s) % z];
                }
            }
            acc
        };
        // Core: syndrome of the information part, then the inverse core block.
        let n_core = 4 * z;
        let mut syn = vec![0u64; n_core.div_ceil(64)];
        for (i, layer) in self.layers.iter().take(4).enumerate() {
            for r in 0..z {
                let mut acc = 0u8;
                for &(j, s) in layer {
                    if j < self.info_cols {
                        acc ^= c[j * z + (r + s) % z];
                    }
                }
                if acc != 0 {
                    let idx = i * z + r;
                    syn[idx / 64] |= 1 << (idx % 64);
                }
            }
        }
        let p0 = self.info_cols * z;
        for (row, inv) in self.core_inv.iter().enumerate() {
            let parity = inv.iter().zip(&syn).map(|(a, b)| (a & b).count_ones()).sum::<u32>() & 1;
            c[p0 + row] = parity as u8;
        }
        // Extension rows: each solves for its own parity column.
        for (i, layer) in self.layers.iter().enumerate().skip(4) {
            let col = self.info_cols + i;
            let shift = layer.iter().find(|&&(j, _)| j == col).map(|&(_, s)| s).unwrap();
            for r in 0..z {
                c[col * z + (r + shift) % z] = row_sum(&c, layer, r, col);
            }
        }
        c
    }

    /// Codeword positions of the `e` bits sent, starting `start` bits into the buffer.
    pub fn tx_positions(&self, e: usize, start: usize) -> Vec<usize> {
        (0..e).map(|i| self.buffer[(start + i) % self.buffer.len()]).collect()
    }

    /// The `e` bits sent for a codeword (rv0).
    pub fn rate_match(&self, codeword: &[u8], e: usize) -> Vec<u8> {
        self.tx_positions(e, 0).into_iter().map(|p| codeword[p]).collect()
    }

    /// Encode and rate-match.
    pub fn encode_rate_matched(&self, info: &[u8], e: usize) -> Vec<u8> {
        self.rate_match(&self.encode(info), e)
    }

    /// Received LLRs of the `llr.len()` bits sent from buffer index `start` to
    /// codeword LLRs (N); punctured and unsent bits are erasures, fillers known
    /// zeros, repeats add up.
    pub fn rate_recover(&self, llr: &[f64], start: usize) -> Vec<f32> {
        let mut out = vec![0f32; self.n_codeword];
        out[self.k..self.k_native].fill(FILLER_LLR);
        for (p, &l) in self.tx_positions(llr.len(), start).into_iter().zip(llr) {
            out[p] += l as f32;
        }
        out
    }

    /// Base rows worth decoding when only `sent` positions were sent: the four
    /// core rows, and each later row whose own parity column was sent.
    pub fn rows_needed(&self, sent: &[usize]) -> Vec<usize> {
        let mut cols = vec![false; self.base_cols];
        for &p in sent {
            cols[p / self.z] = true;
        }
        (0..self.base_rows).filter(|&i| i < 4 || cols[self.info_cols + i]).collect()
    }

    /// Rows needed for an rv0 transmission of `e` bits.
    pub fn rows_for_length(&self, e: usize) -> Vec<usize> {
        self.rows_needed(&self.tx_positions(e, 0))
    }

    /// True if hard decisions satisfy every check of `rows`.
    pub fn syndrome_ok(&self, hard: &[u8], rows: &[usize]) -> bool {
        let z = self.z;
        rows.iter().all(|&i| {
            (0..z).all(|r| self.layers[i].iter().fold(0u8, |acc, &(j, s)| acc ^ hard[j * z + (r + s) % z]) == 0)
        })
    }

    /// A decoder with scratch space for this code.
    pub fn decoder(&self) -> Decoder<'_> {
        Decoder::new(self)
    }
}

/// Result of decoding one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeResult {
    /// Hard decisions on the K' information bits.
    pub bits: Vec<u8>,
    /// CRC passed and syndrome satisfied.
    pub ok: bool,
    /// Iterations used.
    pub iterations: usize,
}

/// Layered normalized min-sum decoder for one code, reusing its buffers.
pub struct Decoder<'a> {
    code: &'a LiftedCode,
    lv: Vec<f32>,
    msgs: Vec<f32>,
    q: Vec<f32>,
    hard: Vec<u8>,
    min1: Vec<f32>,
    min2: Vec<f32>,
    sign: Vec<u32>,
}

impl<'a> Decoder<'a> {
    fn new(code: &'a LiftedCode) -> Self {
        let z = code.z;
        let edges: usize = code.layers.iter().map(Vec::len).sum();
        let max_deg = code.layers.iter().map(Vec::len).max().unwrap_or(0);
        Decoder {
            code,
            lv: vec![0.0; code.n_codeword],
            msgs: vec![0.0; edges * z],
            q: vec![0.0; max_deg * z],
            hard: vec![0; code.n_codeword],
            min1: vec![0.0; z],
            min2: vec![0.0; z],
            sign: vec![0; z],
        }
    }

    /// Decode codeword LLRs (from [`LiftedCode::rate_recover`]) over `rows`.
    pub fn decode(&mut self, llr: &[f32], rows: &[usize], max_iter: usize, alpha: f32) -> DecodeResult {
        let code = self.code;
        let z = code.z;
        assert_eq!(llr.len(), code.n_codeword);
        self.lv.copy_from_slice(llr);
        // Message offsets: rows in `rows` order.
        let mut offsets = Vec::with_capacity(rows.len());
        let mut off = 0;
        for &i in rows {
            offsets.push(off);
            off += code.layers[i].len() * z;
        }
        self.msgs[..off].fill(0.0);
        let k = code.k;
        for it in 1..=max_iter {
            for (li, &i) in rows.iter().enumerate() {
                self.layer(&code.layers[i], offsets[li], alpha);
            }
            for (h, &v) in self.hard.iter_mut().zip(&self.lv) {
                *h = (v < 0.0) as u8;
            }
            if crc::check(&self.hard[..k]) && code.syndrome_ok(&self.hard, rows) {
                return DecodeResult { bits: self.hard[..k].to_vec(), ok: true, iterations: it };
            }
        }
        DecodeResult { bits: self.hard[..k].to_vec(), ok: false, iterations: max_iter }
    }

    #[inline]
    fn layer(&mut self, layer: &[(usize, usize)], msg_off: usize, alpha: f32) {
        let z = self.code.z;
        let deg = layer.len();
        let lv = &mut self.lv;
        let q = &mut self.q[..deg * z];
        let msgs = &mut self.msgs[msg_off..msg_off + deg * z];
        // Gather q = lv - msg, the rotation split into two contiguous runs.
        for (e, &(j, s)) in layer.iter().enumerate() {
            let col = &lv[j * z..(j + 1) * z];
            let qe = &mut q[e * z..(e + 1) * z];
            let me = &msgs[e * z..(e + 1) * z];
            let n1 = z - s;
            for r in 0..n1 {
                qe[r] = col[s + r] - me[r];
            }
            for r in n1..z {
                qe[r] = col[r - n1] - me[r];
            }
        }
        let (min1, min2, sign) = (&mut self.min1, &mut self.min2, &mut self.sign);
        min1.fill(f32::INFINITY);
        min2.fill(f32::INFINITY);
        sign.fill(0);
        for e in 0..deg {
            let qe = &q[e * z..(e + 1) * z];
            for r in 0..z {
                let m = qe[r].abs();
                min2[r] = min2[r].min(min1[r].max(m));
                min1[r] = min1[r].min(m);
                sign[r] ^= (qe[r] < 0.0) as u32;
            }
        }
        for e in 0..deg {
            let qe = &mut q[e * z..(e + 1) * z];
            let me = &mut msgs[e * z..(e + 1) * z];
            for r in 0..z {
                let v = qe[r];
                let m = v.abs();
                let mut rr = if m == min1[r] { min2[r] } else { min1[r] };
                rr *= alpha;
                let neg = sign[r] ^ ((v < 0.0) as u32);
                let rr = f32::from_bits(rr.to_bits() ^ (neg << 31));
                me[r] = rr;
                qe[r] = v + rr;
            }
        }
        for (e, &(j, s)) in layer.iter().enumerate() {
            let col = &mut lv[j * z..(j + 1) * z];
            let qe = &q[e * z..(e + 1) * z];
            let n1 = z - s;
            col[s..z].copy_from_slice(&qe[..n1]);
            col[..s].copy_from_slice(&qe[n1..]);
        }
    }
}

/// The header code (BG2, Z = 15).
pub fn header_code() -> &'static LiftedCode {
    static C: OnceLock<LiftedCode> = OnceLock::new();
    C.get_or_init(|| LiftedCode::build(params::HEADER_CODE, HEADER_LIFTED))
}

/// The control code (BG2, Z = 26).
pub fn control_code() -> &'static LiftedCode {
    static C: OnceLock<LiftedCode> = OnceLock::new();
    C.get_or_init(|| LiftedCode::build(params::CONTROL_CODE, CONTROL_LIFTED))
}

/// The data code (BG1, Z = 48).
pub fn data_code() -> &'static LiftedCode {
    static C: OnceLock<LiftedCode> = OnceLock::new();
    C.get_or_init(|| LiftedCode::build(params::DATA_CODE, DATA_LIFTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codewords_satisfy_every_check() {
        for code in [header_code(), control_code(), data_code()] {
            let info: Vec<u8> = (0..code.k).map(|i| ((i * 31 + i / 7) % 3 == 0) as u8).collect();
            let info = crate::crc::append_crc(&info[..code.k - 16]);
            let c = code.encode(&info);
            let all: Vec<usize> = (0..code.base_rows).collect();
            assert!(code.syndrome_ok(&c, &all), "{}", code.spec.name);
        }
    }

    #[test]
    fn clean_decode() {
        let code = data_code();
        let info = crate::crc::append_crc(&vec![1u8; 1008]);
        let tx = code.encode_rate_matched(&info, 1368);
        let llr: Vec<f64> = tx.iter().map(|&b| if b == 0 { 4.0 } else { -4.0 }).collect();
        let res = code.decoder().decode(&code.rate_recover(&llr, 0), &code.rows_for_length(1368), 30, 0.75);
        assert!(res.ok);
        assert_eq!(res.bits, info);
    }
}
