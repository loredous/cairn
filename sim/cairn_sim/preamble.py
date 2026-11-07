"""Preamble sequences S1 and S2 (SPEC 6.1, 20.3).

PROVISIONAL until Stage 1 fixes them. S1 is a constant-amplitude sequence
on the 24 base tones; SPEC 20.3 asks for a PAPR search starting from Newman
phases, and ``newman_phases`` is that starting point. S2 is BPSK from a
24-bit PN segment, taken here from the 38.211 Gold sequence.

Frame order: S1, S1, S2, S2, each a full symbol with its own prefix. The
repeat makes the signal periodic at lag N + cp (540 or 600 samples), which
is what the autocorrelation detector looks for.
"""

from __future__ import annotations

import numpy as np

from . import mapping, params

S2_C_INIT = 0x5332  # provisional


def newman_phases(n: int = params.BASE_TONES) -> np.ndarray:
    k = np.arange(n)
    return np.pi * k ** 2 / n


def s1(n: int = params.BASE_TONES) -> np.ndarray:
    return np.exp(1j * newman_phases(n))


def s2(n: int = params.BASE_TONES) -> np.ndarray:
    return 1.0 - 2.0 * mapping.gold_sequence(S2_C_INIT, n).astype(float)


def grid(width: int) -> np.ndarray:
    """The four preamble symbols as tone values (4, width); base tones only."""
    g = np.zeros((params.PREAMBLE_SYMBOLS, width), dtype=complex)
    b = params.BASE_TONES
    g[0, :b] = g[1, :b] = s1()
    g[2, :b] = g[3, :b] = s2()
    return g
