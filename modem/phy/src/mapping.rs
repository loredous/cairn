//! Constellation labeling and the max-log demapper (SPEC 6.2, 6.4; ADR 0005
//! `constellation-labeling`): TS 38.211 5.1 Gray mapping, even bits on I,
//! odd bits on Q, the first bit of each axis the sign, unit average power.

use num_complex::Complex64;

/// Gray PAM levels (unnormalized odd integers), indexed by the axis label
/// read most significant bit first (the axis's first bit is the MSB).
pub fn pam_levels(bits_per_axis: usize) -> Vec<f64> {
    fn inner(rest: &[u8]) -> f64 {
        match rest.split_first() {
            None => 1.0,
            Some((&b, tail)) => (1u32 << rest.len()) as f64 - (1.0 - 2.0 * b as f64) * inner(tail),
        }
    }
    let m = bits_per_axis;
    (0..1usize << m)
        .map(|v| {
            let label: Vec<u8> = (0..m).map(|i| ((v >> (m - 1 - i)) & 1) as u8).collect();
            (1.0 - 2.0 * label[0] as f64) * inner(&label[1..])
        })
        .collect()
}

/// Scale to unit average power: 1/sqrt(2), 1/sqrt(10), 1/sqrt(42), 1/sqrt(170).
pub fn axis_scale(bits_per_point: usize) -> f64 {
    let levels = pam_levels(bits_per_point / 2);
    let mean = levels.iter().map(|l| l * l).sum::<f64>() / levels.len() as f64;
    1.0 / (2.0 * mean).sqrt()
}

/// Unnormalized I and Q levels of each point (odd integers).
pub fn iq_levels(bits: &[u8], bits_per_point: usize) -> Vec<(i32, i32)> {
    let m = bits_per_point / 2;
    let levels = pam_levels(m);
    bits.chunks_exact(bits_per_point)
        .map(|b| {
            let (mut i, mut q) = (0usize, 0usize);
            for a in 0..m {
                i = (i << 1) | b[2 * a] as usize;
                q = (q << 1) | b[2 * a + 1] as usize;
            }
            (levels[i] as i32, levels[q] as i32)
        })
        .collect()
}

/// Bits (length a multiple of `bits_per_point`) to unit-power points.
pub fn modulate(bits: &[u8], bits_per_point: usize) -> Vec<Complex64> {
    let s = axis_scale(bits_per_point);
    iq_levels(bits, bits_per_point).into_iter().map(|(i, q)| Complex64::new(i as f64 * s, q as f64 * s)).collect()
}

/// Max-log demapper tables for one constellation.
#[derive(Clone, Debug)]
pub struct Demapper {
    bits_per_point: usize,
    levels: Vec<f64>,
    /// For each axis bit, the levels whose label has that bit 0 and 1.
    zeros: Vec<Vec<f64>>,
    ones: Vec<Vec<f64>>,
}

impl Demapper {
    /// Tables for `bits_per_point` = 2, 4, 6 or 8.
    pub fn new(bits_per_point: usize) -> Self {
        let m = bits_per_point / 2;
        let s = axis_scale(bits_per_point);
        let levels: Vec<f64> = pam_levels(m).into_iter().map(|l| l * s).collect();
        let mut zeros = vec![Vec::new(); m];
        let mut ones = vec![Vec::new(); m];
        for (v, &l) in levels.iter().enumerate() {
            for bit in 0..m {
                if (v >> (m - 1 - bit)) & 1 == 1 {
                    ones[bit].push(l);
                } else {
                    zeros[bit].push(l);
                }
            }
        }
        Demapper { bits_per_point, levels, zeros, ones }
    }

    /// Bits per point.
    pub fn bits_per_point(&self) -> usize {
        self.bits_per_point
    }

    /// Scaled PAM levels.
    pub fn levels(&self) -> &[f64] {
        &self.levels
    }

    /// LLRs L = log P(0)/P(1) of one equalized point with complex noise
    /// variance `nv`, appended to `out` in transmitted bit order.
    #[inline]
    pub fn push_llrs(&self, y: Complex64, nv: f64, out: &mut Vec<f64>) {
        let m = self.bits_per_point / 2;
        let base = out.len();
        out.resize(base + self.bits_per_point, 0.0);
        for (axis, comp) in [(0usize, y.re), (1usize, y.im)] {
            for bit in 0..m {
                let d1 = self.ones[bit].iter().map(|l| (comp - l) * (comp - l)).fold(f64::INFINITY, f64::min);
                let d0 = self.zeros[bit].iter().map(|l| (comp - l) * (comp - l)).fold(f64::INFINITY, f64::min);
                out[base + 2 * bit + axis] = (d1 - d0) / nv;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_38_211() {
        assert_eq!(pam_levels(1), vec![1.0, -1.0]);
        assert_eq!(pam_levels(2), vec![1.0, 3.0, -1.0, -3.0]);
        assert_eq!(pam_levels(3), vec![3.0, 1.0, 5.0, 7.0, -3.0, -1.0, -5.0, -7.0]);
        for (q, p) in [(2, 2.0f64), (4, 10.0), (6, 42.0), (8, 170.0)] {
            assert_eq!(axis_scale(q), 1.0 / p.sqrt());
        }
    }

    #[test]
    fn hard_decisions_round_trip() {
        for q in [2, 4, 6, 8] {
            let bits: Vec<u8> = (0..q * 64).map(|i| ((i * 7 + i / 3) % 2) as u8).collect();
            let pts = modulate(&bits, q);
            let d = Demapper::new(q);
            let mut llr = Vec::new();
            for p in pts {
                d.push_llrs(p, 0.1, &mut llr);
            }
            let hard: Vec<u8> = llr.iter().map(|&l| (l < 0.0) as u8).collect();
            assert_eq!(hard, bits);
        }
    }
}
