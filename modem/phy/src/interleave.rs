//! Block interleaver (SPEC 6.4, ADR 0005 `block-interleaver`): bit i of a
//! block of E bits moves to position (a * i) mod E, with a = 0.618 E rounded
//! half up, then raised to the next integer coprime to E.

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The multiplier a for a block of `e` bits.
pub fn multiplier(e: usize) -> usize {
    let mut a = (618 * e + 500) / 1000;
    while gcd(a, e) != 1 {
        a += 1;
    }
    a
}

/// Permutation p with interleaved[p[i]] = coded[i].
pub fn permutation(e: usize) -> Vec<usize> {
    let a = multiplier(e);
    (0..e).map(|i| (a * i) % e).collect()
}

/// Interleave a block of bits.
pub fn interleave<T: Copy + Default>(bits: &[T]) -> Vec<T> {
    let p = permutation(bits.len());
    let mut out = vec![T::default(); bits.len()];
    for (i, &b) in bits.iter().enumerate() {
        out[p[i]] = b;
    }
    out
}

/// Undo [`interleave`]: out[i] = values[p[i]].
pub fn deinterleave<T: Copy>(values: &[T]) -> Vec<T> {
    permutation(values.len()).into_iter().map(|j| values[j]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_multipliers() {
        // SPEC 6.4: 179 (288), 269 (432), 317 (512), 1267, 949, 845, 761.
        for (e, a) in [(288, 179), (432, 269), (512, 317), (2048, 1267), (1536, 949), (1368, 845), (1232, 761)] {
            assert_eq!(multiplier(e), a, "E = {e}");
        }
    }

    #[test]
    fn round_trip() {
        let v: Vec<u32> = (0..1368).collect();
        assert_eq!(deinterleave(&interleave(&v)), v);
    }
}
