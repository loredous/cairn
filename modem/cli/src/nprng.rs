//! NumPy's `default_rng(seed)` (SeedSequence + PCG64) and
//! `Generator.integers(0, 2, n, dtype=np.uint8)`, bit for bit, so frames
//! built here carry the same header and payload bits as the simulator's
//! `frame.build(cfg, np.random.default_rng(seed))`. That lets
//! `cairn-bench rx` score a WAV written by `cairn-modem tx --wav` exactly
//! as it scores its own (header and block bit comparison included).

const INIT_A: u32 = 0x43b0_d7e5;
const MULT_A: u32 = 0x931e_8875;
const INIT_B: u32 = 0x8b51_f9dd;
const MULT_B: u32 = 0x58f3_8ded;
const MIX_MULT_L: u32 = 0xca01_f9dd;
const MIX_MULT_R: u32 = 0x4973_f715;
const XSHIFT: u32 = 16;
const POOL: usize = 4;
const PCG_MULT: u128 = 0x2360_ED05_1FC6_5DA4_4385_DF64_9FCC_F645;

fn hashmix(mut value: u32, hash_const: &mut u32) -> u32 {
    value ^= *hash_const;
    *hash_const = hash_const.wrapping_mul(MULT_A);
    value = value.wrapping_mul(*hash_const);
    value ^ (value >> XSHIFT)
}

fn mix(x: u32, y: u32) -> u32 {
    let r = MIX_MULT_L.wrapping_mul(x).wrapping_sub(MIX_MULT_R.wrapping_mul(y));
    r ^ (r >> XSHIFT)
}

/// `np.random.SeedSequence(seed).generate_state(4, np.uint64)`.
fn seed_sequence_state(seed: u64) -> [u64; 4] {
    let mut entropy = Vec::new();
    let mut n = seed;
    if n == 0 {
        entropy.push(0);
    }
    while n > 0 {
        entropy.push(n as u32);
        n >>= 32;
    }
    let mut pool = [0u32; POOL];
    let mut hc = INIT_A;
    for (i, p) in pool.iter_mut().enumerate() {
        *p = hashmix(entropy.get(i).copied().unwrap_or(0), &mut hc);
    }
    for src in 0..POOL {
        for dst in 0..POOL {
            if src != dst {
                pool[dst] = mix(pool[dst], hashmix(pool[src], &mut hc));
            }
        }
    }
    for &e in entropy.iter().skip(POOL) {
        for p in pool.iter_mut() {
            *p = mix(*p, hashmix(e, &mut hc));
        }
    }
    let mut hb = INIT_B;
    let mut words = [0u32; 8];
    for (i, w) in words.iter_mut().enumerate() {
        let mut v = pool[i % POOL];
        v ^= hb;
        hb = hb.wrapping_mul(MULT_B);
        v = v.wrapping_mul(hb);
        *w = v ^ (v >> XSHIFT);
    }
    let mut out = [0u64; 4];
    for (i, o) in out.iter_mut().enumerate() {
        *o = words[2 * i] as u64 | ((words[2 * i + 1] as u64) << 32);
    }
    out
}

/// NumPy's PCG64 bit generator.
pub struct Pcg64 {
    state: u128,
    inc: u128,
    spare: Option<u32>,
}

impl Pcg64 {
    /// `np.random.default_rng(seed)`.
    pub fn new(seed: u64) -> Self {
        let s = seed_sequence_state(seed);
        let initstate = ((s[0] as u128) << 64) | s[1] as u128;
        let initseq = ((s[2] as u128) << 64) | s[3] as u128;
        let mut g = Pcg64 { state: 0, inc: (initseq << 1) | 1, spare: None };
        g.step();
        g.state = g.state.wrapping_add(initstate);
        g.step();
        g
    }

    fn step(&mut self) {
        self.state = self.state.wrapping_mul(PCG_MULT).wrapping_add(self.inc);
    }

    /// Next 64-bit output (XSL-RR).
    pub fn next_u64(&mut self) -> u64 {
        self.step();
        let hi = (self.state >> 64) as u64;
        let lo = self.state as u64;
        (hi ^ lo).rotate_right((self.state >> 122) as u32)
    }

    /// Next 32-bit output (NumPy buffers the high half of a 64-bit draw).
    pub fn next_u32(&mut self) -> u32 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let v = self.next_u64();
        self.spare = Some((v >> 32) as u32);
        v as u32
    }

    /// `integers(0, 2, n, dtype=np.uint8)`: Lemire's method on buffered
    /// bytes, which for a range of two is each byte's top bit.
    pub fn bits(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut buf = 0u32;
        let mut left = 0;
        for _ in 0..n {
            if left == 0 {
                buf = self.next_u32();
                left = 3;
            } else {
                buf >>= 8;
                left -= 1;
            }
            out.push(((buf as u8 as u16 * 2) >> 8) as u8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(bits: &[u8]) -> String {
        bits.iter().map(|b| char::from(b'0' + b)).collect()
    }

    // Recorded from NumPy 2.5.4 (`uv run --package cairn-sim python`):
    //   default_rng(1001).integers(0, 2, 16, dtype=np.uint8)
    //   default_rng(0).bit_generator.random_raw()
    //   g = default_rng(7); g.integers(0, 2, 128, ...)[:40]; g.integers(0, 2, (3, 1008), ...)[2, -40:]
    //   default_rng(2**40 + 5).bit_generator.random_raw()
    #[test]
    fn matches_numpy() {
        assert_eq!(s(&Pcg64::new(1001).bits(16)), "0101001100110100");
        assert_eq!(Pcg64::new(0).next_u64(), 11749869230777074271);
        let mut g = Pcg64::new(7);
        assert_eq!(s(&g.bits(128)[..40]), "1011100110011111010100110001101000001110");
        assert_eq!(s(&g.bits(3 * 1008)[3 * 1008 - 40..]), "0011101110010001001011111001100100011101");
        assert_eq!(Pcg64::new((1 << 40) + 5).next_u64(), 87427594128977387);
    }
}
