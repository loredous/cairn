"""Channel models (SPEC 14.2).

Audio SNR, as used throughout the simulation: the signal's mean power over
the noise power in the occupied band (300 Hz to the top active tone + 50 Hz),
both measured at the receiver's audio output. With 100 Hz tones filling that
band, audio SNR equals the per-tone SNR the demodulator sees.
"""

from __future__ import annotations

import numpy as np

from . import params


def occupied_band(width: int) -> tuple[float, float]:
    return params.BAND_LOW_HZ, params.band_high_hz(width)


def awgn(x: np.ndarray, snr_db: float, width: int, rng: np.random.Generator,
         signal_power: float | None = None) -> np.ndarray:
    """Add white Gaussian noise so the in-band audio SNR is ``snr_db``."""
    lo, hi = occupied_band(width)
    p_sig = float(np.mean(x ** 2)) if signal_power is None else signal_power
    in_band_noise = p_sig / 10 ** (snr_db / 10)
    variance = in_band_noise * (params.FS / 2) / (hi - lo)
    return x + rng.standard_normal(x.shape) * np.sqrt(variance)
