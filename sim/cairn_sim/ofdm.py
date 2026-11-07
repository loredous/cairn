"""OFDM symbol assembly and demodulation (SPEC 5.2, 7.1).

Tone k sits at 350 + 100 k Hz, which is FFT bin 3.5 + k of the 480-sample
useful symbol: a half-bin offset. Each tone then has a half-integer number of
cycles per useful symbol, so the continuous extension of the symbol into its
prefix is the *negated* tail (x[n - 480] = -x[n]). The modulator evaluates the
tones directly over the prefix and the useful part, which produces exactly
that; the demodulator undoes the half-bin offset before its FFT. A channel
delay shorter than the prefix still turns into one complex gain per tone.

The output is real audio. With every active tone at unit average power, the
mean signal power (before clip-and-filter) is 1.
"""

from __future__ import annotations

import numpy as np

from . import params

N = params.N_USEFUL
FIRST_BIN = int(params.BIN_OFFSET)  # 3: tone k uses integer bin 3 + k, shifted half a bin


def _half_bin(n: np.ndarray) -> np.ndarray:
    return np.exp(1j * np.pi * n / N)


def modulate(grid: np.ndarray, cp: int = params.CP_SHORT, active: int | None = None) -> np.ndarray:
    """Complex tone values (n_sym, n_tones) -> real samples (n_sym * (N + cp),).

    Column k of ``grid`` is tone k; zero columns are silent tones. The output
    is scaled by sqrt(2 / active), so the mean power is 1 when ``active``
    tones are all at unit power. ``active`` defaults to the number of
    non-silent columns; a frame passes one value for every symbol so each
    tone keeps the same amplitude throughout (SPEC 6.2: the transmitter never
    scales individual tones).
    """
    grid = np.atleast_2d(grid)
    n_sym, n_tones = grid.shape
    if active is None:
        active = max(int(np.count_nonzero(np.any(grid != 0, axis=0))), 1)
    spec = np.zeros((n_sym, N), dtype=complex)
    spec[:, FIRST_BIN:FIRST_BIN + n_tones] = grid
    base = np.fft.ifft(spec, axis=1) * N                      # integer-bin symbol
    n = np.arange(-cp, N)
    ext = base[:, n % N] * _half_bin(n)[None, :]
    return (np.sqrt(2.0 / active) * ext.real).reshape(-1)


def demodulate(samples: np.ndarray, start: float, n_sym: int, n_tones: int,
               cp: int = params.CP_SHORT, active: int | None = None) -> np.ndarray:
    """Real samples -> tone values (n_sym, n_tones).

    ``start`` is the sample index of the first symbol's useful part (after
    its prefix). ``active`` is the number of non-silent tones the
    transmitter scaled by (default ``n_tones``).
    """
    active = active or n_tones
    stride = N + cp
    first = int(round(start))
    idx = first + stride * np.arange(n_sym)[:, None] + np.arange(N)[None, :]
    seg = samples[idx] * np.conj(_half_bin(np.arange(N)))[None, :]
    spec = np.fft.fft(seg, axis=1)
    scale = 2.0 / (N * np.sqrt(2.0 / active))
    return spec[:, FIRST_BIN:FIRST_BIN + n_tones] * scale


def clip_and_filter(samples: np.ndarray, n_tones: int, active_tones: np.ndarray,
                    cp: int = params.CP_SHORT,
                    clip_db: float = params.CLIP_DB_ABOVE_RMS,
                    scale_tones: int | None = None) -> np.ndarray:
    """SPEC 7.1: clip each symbol at clip_db above its RMS, filter, clip again.

    The filter keeps only the active tones: it takes each clipped symbol's
    useful part to the tone domain, zeroes everything else, and rebuilds the
    symbol with its (negated-tail) prefix. The second clip bounds the peaks
    the filter regrows, so PAPR <= clip_db holds at the output.
    """
    stride = N + cp
    sym = samples.reshape(-1, stride).copy()
    n_sym = sym.shape[0]
    rms = np.sqrt(np.mean(sym ** 2, axis=1, keepdims=True))
    limit = rms * 10 ** (clip_db / 20)
    sym = np.clip(sym, -limit, limit)
    scale = scale_tones or len(active_tones)
    tones = demodulate(sym.reshape(-1), cp, n_sym, n_tones, cp, active=scale)
    keep = np.zeros(n_tones, dtype=bool)
    keep[np.asarray(active_tones)] = True
    tones[:, ~keep] = 0
    rebuilt = modulate(tones, cp, active=scale).reshape(n_sym, stride)
    rms2 = np.sqrt(np.mean(rebuilt ** 2, axis=1, keepdims=True))
    limit2 = rms2 * 10 ** (clip_db / 20)
    return np.clip(rebuilt, -limit2, limit2).reshape(-1)


def papr_db(samples: np.ndarray, cp: int = params.CP_SHORT) -> np.ndarray:
    """Per-symbol peak-to-average power ratio in dB."""
    sym = samples.reshape(-1, N + cp)
    return 10 * np.log10(np.max(sym ** 2, axis=1) / np.mean(sym ** 2, axis=1))
