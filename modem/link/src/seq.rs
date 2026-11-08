//! 16-bit block sequence numbers with RFC 1982 serial-number arithmetic
//! (SERIAL_BITS = 16; ADR 0008 `block-sequence-rules`).
//!
//! Internally the ARQ keeps 64-bit absolute block indices, which never wrap;
//! [`Seq`] is what goes on air, and [`Seq::to_abs`] maps an on-air value back
//! to the absolute index nearest a reference, which is unambiguous because
//! the send window keeps every live sequence within 128 of the reference.

use std::cmp::Ordering;
use std::fmt;

/// An on-air 16-bit sequence number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Seq(pub u16);

impl Seq {
    /// `self + n` modulo 2^16.
    pub fn plus(self, n: u16) -> Seq {
        Seq(self.0.wrapping_add(n))
    }

    /// Signed serial distance from `other` to `self` (positive when `self`
    /// is later). Defined for |distance| < 2^15, as RFC 1982 requires.
    pub fn diff(self, other: Seq) -> i32 {
        self.0.wrapping_sub(other.0) as i16 as i32
    }

    /// RFC 1982 comparison; `None` when the two are exactly 2^15 apart
    /// (undefined in RFC 1982).
    pub fn serial_cmp(self, other: Seq) -> Option<Ordering> {
        let d = self.0.wrapping_sub(other.0);
        match d {
            0 => Some(Ordering::Equal),
            0x8000 => None,
            d if d < 0x8000 => Some(Ordering::Greater),
            _ => Some(Ordering::Less),
        }
    }

    /// The on-air sequence of an absolute index.
    pub fn from_abs(abs: u64) -> Seq {
        Seq(abs as u16)
    }

    /// The absolute index nearest `reference` whose low 16 bits are `self`.
    pub fn to_abs(self, reference: u64) -> u64 {
        let d = self.diff(Seq::from_abs(reference)) as i64;
        (reference as i64 + d).max(0) as u64
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_arithmetic_wraps() {
        assert_eq!(Seq(65535).plus(1), Seq(0));
        assert_eq!(Seq(2).diff(Seq(65534)), 4);
        assert_eq!(Seq(65534).diff(Seq(2)), -4);
        assert_eq!(Seq(1).serial_cmp(Seq(65535)), Some(Ordering::Greater));
        assert_eq!(Seq(0).serial_cmp(Seq(0x8000)), None);
    }

    #[test]
    fn absolute_mapping_round_trips_near_reference() {
        for reference in [0u64, 100, 65_500, 65_536, 131_000] {
            for d in -127i64..=127 {
                let abs = reference as i64 + d;
                if abs < 0 {
                    continue;
                }
                assert_eq!(Seq::from_abs(abs as u64).to_abs(reference), abs as u64);
            }
        }
    }
}
