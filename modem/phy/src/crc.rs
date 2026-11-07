//! CRC-16/CCITT-FALSE (SPEC 6.3): polynomial 0x1021, initial value 0xFFFF,
//! no reflection, no final XOR. Bits are `u8` values 0/1, most significant
//! bit first.

use crate::params::{CRC_BITS, CRC_INIT, CRC_POLY};

/// CRC of a byte string.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = CRC_INIT;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ CRC_POLY } else { crc << 1 };
        }
    }
    crc
}

/// CRC of a bit string (any length).
pub fn crc16_bits(bits: &[u8]) -> u16 {
    let mut crc = CRC_INIT;
    for &b in bits {
        let top = ((crc >> 15) as u8 & 1) ^ (b & 1);
        crc <<= 1;
        if top != 0 {
            crc ^= CRC_POLY;
        }
    }
    crc
}

/// Bits followed by their CRC-16.
pub fn append_crc(bits: &[u8]) -> Vec<u8> {
    let crc = crc16_bits(bits);
    let mut out = Vec::with_capacity(bits.len() + CRC_BITS);
    out.extend_from_slice(bits);
    out.extend((0..CRC_BITS).map(|i| ((crc >> (CRC_BITS - 1 - i)) & 1) as u8));
    out
}

/// True if the last 16 bits are the CRC of the bits before them.
pub fn check(block: &[u8]) -> bool {
    if block.len() < CRC_BITS {
        return false;
    }
    let n = block.len() - CRC_BITS;
    let crc = crc16_bits(&block[..n]);
    block[n..].iter().enumerate().all(|(i, &b)| ((crc >> (CRC_BITS - 1 - i)) & 1) as u8 == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        // CRC-16/CCITT-FALSE check value.
        assert_eq!(crc16(b"123456789"), 0x29B1);
        let bits: Vec<u8> = b"123456789".iter().flat_map(|&b| (0..8).map(move |i| (b >> (7 - i)) & 1)).collect();
        assert_eq!(crc16_bits(&bits), 0x29B1);
        assert!(check(&append_crc(&bits)));
    }
}
