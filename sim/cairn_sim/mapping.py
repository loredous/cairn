"""Scrambler, interleaver and constellation mapping (SPEC 6.2, 6.4).

Specified by ADR 0005 (adrs/0005-specify-the-scrambler-interleaver-and-
bit-to-tone-mapping), which made these Stage 1 choices normative; the
published vectors in vectors/bitmap/ are generated from this code
(``bitmap_vectors``):

- Scrambler: the 3GPP length-31 Gold sequence (TS 38.211 5.2.1, Nc = 1600)
  with c_init = 1 + the block's position in the frame (header = 0).
- Interleaver: a linear permutation pi(i) = (a*i) mod E of each block's E
  coded bits, with a = 0.618 E rounded (half up in ADR 0005's integer form,
  floor((618 E + 500) / 1000); the float rounding here agrees for every E
  below 6000), then raised to the next integer coprime to E. It
  needs no knowledge of the tone mask, so it is defined for every mask; the
  bit-to-tone order below spreads the result across tones and symbols.
  ``interleaver_eval`` finds it as good as a uniformly random permutation
  under tone masks, a dead tone and a short burst.
- Labeling: the TS 38.211 5.1 Gray mapping (even bits on I, odd bits on Q,
  first bit of each axis the sign), normalized to unit average power.
- Bit-to-tone order: constellation points fill data tones lowest first
  within an OFDM symbol, then the next symbol; blocks are packed back to
  back (SPEC 6.4).
"""

from __future__ import annotations

import math
from functools import cache

import numpy as np

# --- scrambler ----------------------------------------------------------------


@cache
def gold_sequence(c_init: int, length: int) -> np.ndarray:
    """TS 38.211 5.2.1 pseudo-random sequence c(n), n = 0 .. length-1."""
    nc = 1600
    total = nc + length
    x1 = np.zeros(total + 31, dtype=np.uint8)
    x2 = np.zeros(total + 31, dtype=np.uint8)
    x1[0] = 1
    for i in range(31):
        x2[i] = (c_init >> i) & 1
    for n in range(total):
        x1[n + 31] = x1[n + 3] ^ x1[n]
        x2[n + 31] = x2[n + 3] ^ x2[n + 2] ^ x2[n + 1] ^ x2[n]
    c = x1[nc:nc + length] ^ x2[nc:nc + length]
    c.setflags(write=False)
    return c


def scramble_sequence(position: int, length: int) -> np.ndarray:
    return gold_sequence(1 + position, length)


def scramble(bits: np.ndarray, position: int) -> np.ndarray:
    return bits ^ scramble_sequence(position, bits.shape[-1])


def descramble_llr(llr: np.ndarray, position: int) -> np.ndarray:
    c = scramble_sequence(position, llr.shape[-1])
    return np.where(c == 1, -llr, llr)


# --- interleaver -------------------------------------------------------------


INTERLEAVERS = ("linear", "random")


@cache
def interleaver(e: int, kind: str = "linear", key: int = 0) -> np.ndarray:
    """Permutation p with interleaved[p[i]] = coded[i].

    ``linear`` is the ADR 0005 design above. ``random`` is a reference
    for evaluation only: a uniformly random permutation drawn from a seed of
    (e, key), with ``key`` the block's position so every block differs.
    """
    if kind == "linear":
        a = round(0.618 * e)
        while math.gcd(a, e) != 1:
            a += 1
        p = (a * np.arange(e)) % e
    elif kind == "random":
        p = np.random.default_rng([e, key, 0x1E]).permutation(e)
    else:
        raise ValueError(f"unknown interleaver {kind!r}")
    p.setflags(write=False)
    return p


def interleave(bits: np.ndarray, kind: str = "linear", key: int = 0) -> np.ndarray:
    p = interleaver(bits.shape[-1], kind, key)
    out = np.empty_like(bits)
    out[..., p] = bits
    return out


def deinterleave(values: np.ndarray, kind: str = "linear", key: int = 0) -> np.ndarray:
    return values[..., interleaver(values.shape[-1], kind, key)]


# --- constellations ----------------------------------------------------------


@cache
def pam_levels(bits_per_axis: int) -> tuple[np.ndarray, np.ndarray]:
    """Gray PAM levels (unnormalized) and the bit labels of each level.

    Labels follow 38.211: level = (1-2b0)(2^(m-1) - (1-2b1)(2^(m-2) - ...)),
    where b0 is the first bit of the axis.
    """
    m = bits_per_axis
    labels = np.array([[(v >> (m - 1 - i)) & 1 for i in range(m)] for v in range(2 ** m)],
                      dtype=np.uint8)
    def inner(rest: np.ndarray) -> float:
        if rest.size == 0:
            return 1.0
        return 2.0 ** rest.size - (1 - 2 * int(rest[0])) * inner(rest[1:])

    levels = np.array([(1 - 2 * int(lab[0])) * inner(lab[1:]) for lab in labels])
    return levels, labels


def _axis_scale(bits_per_symbol: int) -> float:
    m = bits_per_symbol // 2
    levels, _ = pam_levels(m)
    return 1.0 / math.sqrt(2 * np.mean(levels ** 2))


def modulate(bits: np.ndarray, bits_per_symbol: int) -> np.ndarray:
    """Bits (..., n) with n a multiple of bits_per_symbol -> complex points."""
    m = bits_per_symbol // 2
    b = np.asarray(bits, dtype=np.uint8).reshape(-1, bits_per_symbol)
    levels, labels = pam_levels(m)
    weights = 2 ** np.arange(m - 1, -1, -1)
    i_idx = b[:, 0::2] @ weights
    q_idx = b[:, 1::2] @ weights
    lookup = np.empty(2 ** m)
    lookup[labels @ weights] = levels
    return (lookup[i_idx] + 1j * lookup[q_idx]) * _axis_scale(bits_per_symbol)


def demodulate_llr(y: np.ndarray, noise_var: np.ndarray | float,
                   bits_per_symbol: int) -> np.ndarray:
    """Max-log LLRs for equalized points y with complex noise variance noise_var.

    Returns LLRs in transmitted bit order, shape (len(y) * bits_per_symbol,).
    """
    m = bits_per_symbol // 2
    levels, labels = pam_levels(m)
    levels = levels * _axis_scale(bits_per_symbol)
    y_in = np.asarray(y)
    y = y_in.reshape(-1)
    nv = np.broadcast_to(np.asarray(noise_var, dtype=float), np.shape(y_in)).reshape(-1)
    out = np.empty((y.size, bits_per_symbol))
    for axis, comp in ((0, y.real), (1, y.imag)):
        d2 = (comp[:, None] - levels[None, :]) ** 2          # (N, 2^m)
        for bit in range(m):
            ones = labels[:, bit] == 1
            d1 = d2[:, ones].min(axis=1)
            d0 = d2[:, ~ones].min(axis=1)
            out[:, 2 * bit + axis] = (d1 - d0) / nv
    return out.reshape(-1)
