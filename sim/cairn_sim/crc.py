"""CRC-16/CCITT-FALSE (SPEC 6.3): poly 0x1021, init 0xFFFF, no reflection, no xorout.

Bits are numpy uint8 arrays, most significant bit first. ``crc16_bits`` is the
reference bit-serial version; ``CrcChecker`` evaluates the same CRC as an
affine map over GF(2) so a whole batch of blocks is checked in one matrix
product.
"""

from __future__ import annotations

import numpy as np

POLY = 0x1021
INIT = 0xFFFF


def crc16(data: bytes) -> int:
    crc = INIT
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def crc16_bits(bits: np.ndarray) -> int:
    crc = INIT
    for b in np.asarray(bits, dtype=np.uint8):
        top = ((crc >> 15) & 1) ^ int(b)
        crc = (crc << 1) & 0xFFFF
        if top:
            crc ^= POLY
    return crc


def int_to_bits(value: int, width: int) -> np.ndarray:
    return np.array([(value >> (width - 1 - i)) & 1 for i in range(width)], dtype=np.uint8)


def bits_to_int(bits: np.ndarray) -> int:
    out = 0
    for b in bits:
        out = (out << 1) | int(b)
    return out


def append_crc(bits: np.ndarray) -> np.ndarray:
    """Message bits (any batch shape ending in n) -> bits + CRC-16 (n + 16)."""
    bits = np.asarray(bits, dtype=np.uint8)
    flat = bits.reshape(-1, bits.shape[-1])
    checker = CrcChecker(bits.shape[-1])
    crcs = checker.crc(flat)
    return np.concatenate([flat, crcs], axis=1).reshape(*bits.shape[:-1], bits.shape[-1] + 16)


class CrcChecker:
    """CRC of n message bits as crc = (M @ bits + c0) mod 2, batched."""

    def __init__(self, n: int):
        self.n = n
        self.c0 = int_to_bits(crc16_bits(np.zeros(n, dtype=np.uint8)), 16)
        cols = np.zeros((n, 16), dtype=np.uint8)
        # CRC is affine: crc(e_i) xor crc(0) is column i of M.
        for i in range(n):
            e = np.zeros(n, dtype=np.uint8)
            e[i] = 1
            cols[i] = int_to_bits(crc16_bits(e), 16) ^ self.c0
        self.m = cols.astype(np.float32)  # (n, 16)

    def crc(self, msgs: np.ndarray) -> np.ndarray:
        """(B, n) bits -> (B, 16) CRC bits."""
        prod = (np.asarray(msgs, dtype=np.float32) @ self.m).astype(np.int64)
        return ((prod & 1) ^ self.c0).astype(np.uint8)

    def check(self, blocks: np.ndarray) -> np.ndarray:
        """(B, n + 16) bits with CRC appended -> (B,) bool, True where it passes."""
        blocks = np.asarray(blocks)
        return np.all(self.crc(blocks[:, : self.n]) == blocks[:, self.n:], axis=1)
