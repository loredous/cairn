"""Pass a 48 kHz WAV through the Stage 1 simulator's FM channel (SPEC 14.2).

    uv run --package cairn-sim python modem/tools/fm_channel_wav.py IN.wav OUT.wav \
        [--profile mic] [--snr 25] [--ppm 0] [--seed 1]

For cross-checking the Rust modem: ``cairn-modem tx --wav IN.wav`` writes
frames, this runs them through ``channel.FmChannel`` (the whole recording,
gaps included, so noise fills the silences as on air), and
``cairn-modem listen --wav OUT.wav`` must decode them. The output is
16-bit mono 48 kHz with its peak at -6 dBFS. The manifest next to IN.wav
is copied next to OUT.wav, so ``cairn-bench rx OUT.wav`` scores it too.
"""

from __future__ import annotations

import argparse
import shutil
import wave
from pathlib import Path

import numpy as np

from cairn_sim import channel, params


def read(path: str) -> np.ndarray:
    with wave.open(path, "rb") as w:
        if (w.getnchannels(), w.getsampwidth(), w.getframerate()) != (1, 2, params.FS):
            raise SystemExit(f"{path}: expected 16-bit mono {params.FS} Hz")
        return np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(float) / 32768.0


def write(path: str, x: np.ndarray) -> None:
    q = np.clip(np.round(x * 32768.0), -32768, 32767).astype("<i2")
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(params.FS)
        w.writeframes(q.tobytes())


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--profile", default="mic", choices=sorted(channel.PROFILES))
    ap.add_argument("--snr", type=float, default=25.0, help="in-band audio SNR, dB (default 25)")
    ap.add_argument("--ppm", type=float, default=0.0, help="sample-clock offset, ppm")
    ap.add_argument("--width", type=int, default=params.BASE_TONES, help="tones the SNR is measured over")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    x = read(a.inp)
    ch = channel.FmChannel(profile=a.profile, snr_db=a.snr, sco_ppm=a.ppm, width=a.width)
    y = ch(x, np.random.default_rng(a.seed))
    y *= 0.5 / np.max(np.abs(y))
    write(a.out, y)
    src = Path(a.inp).with_suffix(".json")
    if src.exists():
        shutil.copyfile(src, Path(a.out).with_suffix(".json"))
    print(f"wrote {a.out}: {a.profile}, {a.snr} dB, {a.ppm:+} ppm, {len(y) / params.FS:.2f} s")


if __name__ == "__main__":
    main()
