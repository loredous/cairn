//! The length-31 Gold sequence of 3GPP TS 38.211 5.2.1 (SPEC 6.4) and the
//! sequences built from it: the scrambler, S2, the pilots and the width probe.

use crate::params::{GOLD_NC, PAD_POSITION, PILOT_C_INIT, PILOT_TONES, PROBE_C_INIT};

/// c(n), n = 0 .. length-1, for `c_init` (bits 0/1).
pub fn sequence(c_init: u32, length: usize) -> Vec<u8> {
    // Run both shift registers as 31-bit words: bit i of x holds x(n + i).
    let mut x1: u32 = 1;
    let mut x2: u32 = c_init & 0x7FFF_FFFF;
    let mut out = Vec::with_capacity(length);
    for n in 0..GOLD_NC + length {
        if n >= GOLD_NC {
            out.push(((x1 ^ x2) & 1) as u8);
        }
        let f1 = ((x1 >> 3) ^ x1) & 1;
        let f2 = ((x2 >> 3) ^ (x2 >> 2) ^ (x2 >> 1) ^ x2) & 1;
        x1 = (x1 >> 1) | (f1 << 30);
        x2 = (x2 >> 1) | (f2 << 30);
    }
    out
}

/// Scrambling sequence of the block at frame `position` (c_init = 1 + position).
pub fn scramble_sequence(position: u32, length: usize) -> Vec<u8> {
    sequence(1 + position, length)
}

/// XOR `bits` with the scrambling sequence for `position`, in place.
pub fn scramble(bits: &mut [u8], position: u32) {
    let seq = scramble_sequence(position, bits.len());
    for (b, c) in bits.iter_mut().zip(seq) {
        *b ^= c;
    }
}

/// Undo scrambling on LLRs: negate where the sequence is 1.
pub fn descramble_llr(llr: &mut [f64], position: u32) {
    let seq = scramble_sequence(position, llr.len());
    for (l, c) in llr.iter_mut().zip(seq) {
        if c == 1 {
            *l = -*l;
        }
    }
}

/// Padding bits: c(0), c(1), ... at position 255 (SPEC 6.4).
pub fn pad_bits(n: usize) -> Vec<u8> {
    scramble_sequence(PAD_POSITION, n)
}

/// BPSK value 1 - 2 c.
#[inline]
pub fn bpsk(c: u8) -> f64 {
    1.0 - 2.0 * c as f64
}

/// Pilot values for symbols 0 .. n_symbols-1 on all four pilot slots:
/// pilot j of symbol s is 1 - 2 c(4 s + j) (SPEC 5.2). Row-major
/// (n_symbols, 4).
pub fn pilot_table(n_symbols: usize) -> Vec<[f64; 4]> {
    let c = sequence(PILOT_C_INIT, n_symbols * PILOT_TONES.len());
    c.chunks_exact(4).map(|ch| [bpsk(ch[0]), bpsk(ch[1]), bpsk(ch[2]), bpsk(ch[3])]).collect()
}

/// Pilot value on pilot tone `tone` in symbol `s`.
pub fn pilot_value(table: &[[f64; 4]], s: usize, tone: usize) -> f64 {
    let j = PILOT_TONES.iter().position(|&p| p == tone).expect("pilot tone");
    table[s][j]
}

/// Width-probe values before pilots are inserted: probe symbol r, tone k is
/// 1 - 2 c(56 r + k), c_init = 0x0D0E (SPEC 6.4).
pub fn probe_values() -> Vec<f64> {
    sequence(PROBE_C_INIT, crate::params::PROBE_SYMBOLS * crate::params::MAX_TONES).into_iter().map(bpsk).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight transcription of the TS 38.211 recursion, as in the sim.
    fn reference(c_init: u32, length: usize) -> Vec<u8> {
        let total = GOLD_NC + length;
        let mut x1 = vec![0u8; total + 31];
        let mut x2 = vec![0u8; total + 31];
        x1[0] = 1;
        for (i, x) in x2.iter_mut().take(31).enumerate() {
            *x = ((c_init >> i) & 1) as u8;
        }
        for n in 0..total {
            x1[n + 31] = x1[n + 3] ^ x1[n];
            x2[n + 31] = x2[n + 3] ^ x2[n + 2] ^ x2[n + 1] ^ x2[n];
        }
        (0..length).map(|n| x1[n + GOLD_NC] ^ x2[n + GOLD_NC]).collect()
    }

    #[test]
    fn matches_recursion() {
        for c_init in [0, 1, 2, 256, 5237, 0x0B1D, 0x0D0E, 0x7FFF_FFFF] {
            assert_eq!(sequence(c_init, 3000), reference(c_init, 3000));
        }
    }
}
