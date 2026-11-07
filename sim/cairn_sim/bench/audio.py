"""WAV input and output, resampling, and Morse keying for the bench kit."""

from __future__ import annotations

from math import gcd

import numpy as np
from scipy import signal
from scipy.io import wavfile

from .. import params

FS = params.FS


def read_wav(path) -> tuple[int, np.ndarray]:
    """Read a PCM or float WAV as floats in [-1, 1], shape (samples, channels)."""
    fs, data = wavfile.read(str(path))
    if data.ndim == 1:
        data = data[:, None]
    if data.dtype == np.uint8:
        x = (data.astype(np.float64) - 128.0) / 128.0
    elif np.issubdtype(data.dtype, np.integer):
        x = data.astype(np.float64) / float(-np.iinfo(data.dtype).min)
    else:
        x = data.astype(np.float64)
    return int(fs), x


def write_wav(path, x: np.ndarray, fs: int = FS) -> None:
    """Write mono 16-bit PCM; samples beyond full scale are clipped."""
    pcm = np.clip(np.rint(np.asarray(x) * 32768.0), -32768, 32767).astype("<i2")
    wavfile.write(str(path), int(fs), pcm)


def to_mono(x: np.ndarray, channel: str = "auto") -> tuple[np.ndarray, str]:
    """Pick one channel of (samples, channels): ``left``, ``right``, ``mix``,
    a channel number, or ``auto`` (the loudest by RMS). Returns (mono, name)."""
    if x.shape[1] == 1:
        return x[:, 0], "mono"
    if channel == "mix":
        return x.mean(axis=1), "mix"
    if channel in ("left", "right"):
        return x[:, 0 if channel == "left" else 1], channel
    if channel.isdigit():
        return x[:, int(channel)], f"channel {channel}"
    rms = np.sqrt(np.mean(x ** 2, axis=0))
    i = int(np.argmax(rms))
    return x[:, i], f"auto: channel {i}"


def resample_to_fs(x: np.ndarray, fs_in: int, fs_out: int = FS) -> np.ndarray:
    """Polyphase resample to ``fs_out`` (a no-op at the same rate)."""
    if fs_in == fs_out:
        return x
    g = gcd(fs_in, fs_out)
    return signal.resample_poly(x, fs_out // g, fs_in // g)


def db(x: float) -> float:
    return float(20 * np.log10(max(x, 1e-12)))


# --- Morse -------------------------------------------------------------------

MORSE = {
    "A": ".-", "B": "-...", "C": "-.-.", "D": "-..", "E": ".", "F": "..-.", "G": "--.",
    "H": "....", "I": "..", "J": ".---", "K": "-.-", "L": ".-..", "M": "--", "N": "-.",
    "O": "---", "P": ".--.", "Q": "--.-", "R": ".-.", "S": "...", "T": "-", "U": "..-",
    "V": "...-", "W": ".--", "X": "-..-", "Y": "-.--", "Z": "--..",
    "0": "-----", "1": ".----", "2": "..---", "3": "...--", "4": "....-", "5": ".....",
    "6": "-....", "7": "--...", "8": "---..", "9": "----.", "/": "-..-.", "-": "-....-",
}


def _shape(n: int, ramp: int) -> np.ndarray:
    env = np.ones(n)
    r = min(ramp, n // 2)
    if r:
        up = 0.5 - 0.5 * np.cos(np.pi * (np.arange(r) + 0.5) / r)
        env[:r] = up
        env[n - r:] = up[::-1]
    return env


def morse(text: str, wpm: float = 20.0, freq_hz: float = 800.0, peak: float = 0.7,
          fs: int = FS, ramp_s: float = 0.005) -> np.ndarray:
    """Keyed sine at ``freq_hz`` for ``text`` (PARIS timing: dit = 1.2 / wpm s).

    Keying edges are raised-cosine ramps of ``ramp_s`` so the tone does not
    click; the peak is ``peak`` on every element longer than the ramps.
    """
    dit = int(round(1.2 / wpm * fs))
    ramp = int(round(ramp_s * fs))
    parts: list[np.ndarray] = []
    words = text.upper().split()
    for wi, word in enumerate(words):
        for ci, ch in enumerate(word):
            code = MORSE.get(ch)
            if code is None:
                raise ValueError(f"no Morse code for {ch!r}")
            for ei, el in enumerate(code):
                n = dit * (1 if el == "." else 3)
                t = np.arange(n)
                parts.append(peak * _shape(n, ramp) * np.sin(2 * np.pi * freq_hz * t / fs))
                if ei < len(code) - 1:
                    parts.append(np.zeros(dit))
            if ci < len(word) - 1:
                parts.append(np.zeros(3 * dit))
        if wi < len(words) - 1:
            parts.append(np.zeros(7 * dit))
    return np.concatenate(parts) if parts else np.zeros(0)
