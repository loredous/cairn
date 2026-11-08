"""Cut the off-air regression fixtures from the 2026-11-07 two-radio captures.

    uv run --package cairn-sim python modem/tools/cut_offair_fixtures.py CAPTURE_DIR [--out DIR]

CAPTURE_DIR is the session folder (``cap/cap_AtoB.wav``, ``cap/cap_BtoA.wav``
and the transmitted references ``cap/{AtoB,BtoA}_ref{1..4}.{wav,json}``).
Writes ``modem/fixtures/offair/``: one 16-bit WAV per frame (the samples
exactly as captured, from ``LEAD_S`` before the frame's first preamble
sample to ``TAIL_S`` after its end), the reference manifests, and
``offair.json`` indexing them with what each frame carried (header and
control block bits, payloads decoded from the clean reference) and the
direction's long-frame reference (clock offset and SNR of the 4 s frame,
measured on the full capture). Each frame also records how many data blocks
the Python receiver decodes from the fixture with and without level
tracking, its level fit (``python_level``, for the Rust receiver to agree
with) and the data symbols' gain against the S2 estimate measured with
every sent symbol known (``data_gain_db``, what the tracker should find).
"""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import numpy as np

from cairn_sim import frame, params, preamble
from cairn_sim.bench.audio import read_wav
from cairn_sim.crc import append_crc

ROOT = Path(__file__).resolve().parents[1]
LEAD_S = 0.1
TAIL_S = 0.05
FS = 48_000
NAMES = {1: "1blk", 2: "4s", 3: "probe", 4: "ui"}
KEEP = {"AtoB": (1, 2, 3, 4), "BtoA": (1, 2, 3, 4)}
# The capture lost 1276 samples (26.6 ms, an input xrun the live session
# logged as "xruns 2 gaps 2 (64.8 ms)") inside the B->A one-block frame's
# data block, at this capture sample: the data block cannot decode.
GAPS = {("BtoA", 1): (542419, 1276)}
LINK = frame.LinkState(width=24, cap=24)


def mono16(path: Path) -> tuple[np.ndarray, np.ndarray]:
    """(int16 samples, float samples) of a mono 16-bit WAV."""
    fs, x = read_wav(path)
    assert fs == FS
    x = np.asarray(x).reshape(len(x), -1)[:, 0]
    q = np.round(x * 32768.0).clip(-32768, 32767).astype(np.int16)
    return q, q.astype(float) / 32768.0


def write_wav(path: Path, q: np.ndarray) -> None:
    import wave
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(FS)
        w.writeframes(q.astype("<i2").tobytes())


def decode(x: np.ndarray, **kw) -> list[frame.RxResult]:
    return [frame.receive_header_first(x, LINK, start=int(d.start), cp=int(d.cp), **kw) for d in preamble.detect(x)]


def data_gain_db(r: frame.RxResult, payloads: list[str]) -> float | None:
    """The data symbols' common gain against the S2 channel estimate, dB,
    measured with every transmitted symbol known: the real part of the
    noise-weighted projection of the S2-equalised data symbols (``r``
    decoded without level tracking) on the sent ones, over all data rows.
    What the level tracker should find; None for a frame without data."""
    cfg = r.config
    if not cfg.n_blocks:
        return None
    bits = np.array([np.unpackbits(np.frombuffer(bytes.fromhex(p), np.uint8))[:1008] for p in payloads], np.uint8)
    sent = frame.rebuild(cfg, r.header_bits, append_crc(bits), r.control_bits if cfg.n_control else None)
    d0 = cfg.first_data
    dt = list(cfg.data_tones)
    x = sent.grid[d0:, dt] / cfg.data_gain
    e = r.detail["eq"][d0 - params.PREAMBLE_SYMBOLS:, dt]
    g0 = np.sum(np.real(np.conj(x) * e)) / np.sum(np.abs(x) ** 2)
    w = 1 / np.maximum(np.mean(np.abs(e - g0 * x) ** 2, axis=0), 1e-9)      # each tone by its own error
    g = np.sum(w * np.real(np.conj(x) * e)) / np.sum(w * np.abs(x) ** 2)
    return float(20 * np.log10(g))


def level_entry(r: frame.RxResult) -> dict:
    """The Python receiver's level fit, for the Rust receiver to agree with."""
    lv = r.detail["level"]
    return {"applied": bool(lv.applied), "segments": [float(v) for v in lv.segments], "settle": float(lv.settle),
            "reference": float(lv.reference), "level": [float(v) for v in lv.level]}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("capture_dir", type=Path)
    ap.add_argument("--out", type=Path, default=ROOT / "fixtures" / "offair")
    a = ap.parse_args()
    cap = a.capture_dir / "cap"
    a.out.mkdir(parents=True, exist_ok=True)
    index = {"source": "2026-11-07 two-radio session, cap/cap_AtoB.wav and cap/cap_BtoA.wav",
             "lead_s": LEAD_S, "tail_s": TAIL_S, "directions": {}}
    for direction in ("AtoB", "BtoA"):
        q, x = mono16(cap / f"cap_{direction}.wav")
        results = decode(x)
        assert len(results) == 4, f"{direction}: {len(results)} frames"
        long = results[1]
        cfg = long.config
        entry = {"capture": f"cap/cap_{direction}.wav",
                 "long_frame": {"clock_ppm": -long.detail["drift"] / cfg.stride * 1e6,
                                "clock_sd_ppm": long.detail["drift_sd"] / cfg.stride * 1e6,
                                "snr_db": float(long.detail["snr_db"]),
                                "blocks_ok": int(long.block_ok.sum()), "blocks": int(long.block_ok.size)},
                 "frames": []}
        for i, r in enumerate(results, start=1):
            ref_json = cap / f"{direction}_ref{i}.json"
            shutil.copyfile(ref_json, a.out / ref_json.name)
            if i not in KEEP[direction]:
                continue
            man = json.loads(ref_json.read_text())["frames"][0]
            lo = r.start - int(LEAD_S * FS)
            hi = r.start + man["n_samples"] + int(TAIL_S * FS)
            name = f"{direction}_{NAMES[i]}.wav"
            write_wav(a.out / name, q[lo:hi])
            # What was sent: the header and control blocks from the manifest,
            # the payloads decoded from the clean transmitted reference.
            _, xr = mono16(cap / f"{direction}_ref{i}.wav")
            sent = decode(xr)
            assert len(sent) == 1 and sent[0].block_ok.all() and sent[0].header is not None
            payloads = [bytes(np.packbits(b[:1008])).hex() for b in sent[0].block_bits]
            gap = GAPS.get((direction, i))
            # The fixture decoded as the tests decode it: header first, with
            # and without level tracking.
            _, xf = mono16(a.out / name)
            on = frame.receive(xf, link=LINK)
            off = frame.receive(xf, link=LINK, track_levels=False)
            entry["frames"].append({
                "wav": name, "manifest": ref_json.name, "kind": NAMES[i], "type": man["header"]["type"],
                "data_decodable": gap is None,
                "capture_gap": None if gap is None else {"at_sample": gap[0] - lo, "samples_lost": gap[1]},
                "capture_start_sample": int(r.start), "start_sample": int(LEAD_S * FS), "cp": man["cp"],
                "mcs": man["mcs"], "n_blocks": man["n_blocks"], "n_control": man["n_control"],
                "header_message_hex": man["header"]["message_hex"],
                "control_hex": man["header"]["control_hex"], "payload_hex": payloads,
                "blocks_ok": int(on.block_ok.sum()), "blocks_ok_untracked": int(off.block_ok.sum()),
                "data_gain_db": data_gain_db(off, payloads), "python_level": level_entry(on),
            })
            print(f"{name}: {(hi - lo) / FS:.2f} s, blocks {int(r.block_ok.sum())}/{r.block_ok.size}, "
                  f"clock {-r.detail['drift'] / r.config.stride * 1e6:+.1f} ppm, SNR {r.detail['snr_db']:.1f} dB")
        index["directions"][direction] = entry
    (a.out / "offair.json").write_text(json.dumps(index, indent=1) + "\n")


if __name__ == "__main__":
    main()
