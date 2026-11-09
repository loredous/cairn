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


def serve() -> None:
    """Job server for scripted sweeps (modem/tools/bench_14_1.py --simulate): one JSON
    object per stdin line ({"inp", "out", "profile", "snr", "ppm", "width", "seed",
    "gain_db"}), one JSON reply per job, so the NumPy start-up cost is paid once.
    ``gain_db`` scales the input before the channel (a drive change)."""
    import json
    import sys

    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            j = json.loads(line)
            x = read(j["inp"]) * 10 ** (float(j.get("gain_db", 0.0)) / 20)
            ch = channel.FmChannel(profile=j.get("profile", "mic"), snr_db=float(j.get("snr", 25.0)),
                                   sco_ppm=float(j.get("ppm", 0.0)), width=int(j.get("width", params.BASE_TONES)))
            y = ch(x, np.random.default_rng(int(j.get("seed", 1))))
            y *= 0.5 / np.max(np.abs(y))
            write(j["out"], y)
            reply = {"ok": True, "seconds": len(y) / params.FS}
        except Exception as e:  # noqa: BLE001 - report every failure to the caller
            reply = {"ok": False, "error": f"{type(e).__name__}: {e}"}
        print(json.dumps(reply), flush=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("inp", nargs="?")
    ap.add_argument("out", nargs="?")
    ap.add_argument("--serve", action="store_true", help="read JSON jobs from stdin (see serve())")
    ap.add_argument("--profile", default="mic", choices=sorted(channel.PROFILES))
    ap.add_argument("--snr", type=float, default=25.0, help="in-band audio SNR, dB (default 25)")
    ap.add_argument("--ppm", type=float, default=0.0, help="sample-clock offset, ppm")
    ap.add_argument("--width", type=int, default=params.BASE_TONES, help="tones the SNR is measured over")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    if a.serve:
        serve()
        return
    if not (a.inp and a.out):
        ap.error("inp and out are required unless --serve")
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
