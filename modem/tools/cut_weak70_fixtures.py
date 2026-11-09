"""Cut the weak 70 cm off-air fixtures from the 2026-11-08 three-band captures.

    uv run --package cairn-sim python modem/tools/cut_weak70_fixtures.py CAPTURE_DIR [--out DIR]

CAPTURE_DIR is the session folder ``2026-11-08-3band``. Writes, into
``modem/fixtures/offair/`` by default, one 16-bit WAV per frame in ``FRAMES``
(the samples exactly as captured, from ``LEAD_S`` before the frame's first
preamble sample to ``TAIL_S`` after its end) and ``weak70.json`` indexing
them: the link state the listener decoded with, what each frame carried (its
header and control block bits as decoded, the payloads ``cairn-modem tx``
drew for its seed), the lines at harmonics of 1 kHz measured over the frame,
and what the Python receiver decodes from the fixture with and without
subtracting them (blocks and reported SNR), for the tests in
``sim/tests/test_offair_weak70.py`` and ``modem/phy/tests/offair.rs``.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

from cairn_sim import frame, preamble
from cairn_sim.weak_fm_eval import link_for, seeded_payloads

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cut_offair_fixtures import FS, mono16, write_wav  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
LEAD_S = 0.1
TAIL_S = 0.05
# (fixture name, recording in CAPTURE_DIR, frame index in it, what it shows)
FRAMES = (
    ("w70_l2_m5", "70cm/l2_AtoB_m5.wav", 0,
     "MCS 5, lines 21-26 dB below the data: 6 of 41 blocks, all 41 with the lines removed"),
    ("w70_l4_m4", "70cm/l4_AtoB_m4.wav", 0,
     "MCS 4, the strongest lines (14-20 dB below the data): no block, most with the lines removed"),
    ("w70_t1f", "70cm/t1f_AtoB.wav", 0,
     "UI, MCS 2, 24 tones, one block, the strongest lines: a short frame (0.53 s)"),
)


def bits_hex(bits: np.ndarray) -> str:
    return bytes(np.packbits(np.asarray(bits, np.uint8))).hex()


def decode_entry(x: np.ndarray, link: frame.LinkState, start: int, cp: int, cancel: bool) -> dict:
    r = frame.receive_header_first(x, link, start=start, cp=cp, cancel_lines=cancel)
    return {"blocks_ok": int(r.block_ok.sum()), "snr_db": round(float(r.detail["snr_db"]), 2),
            "lines": [[k, round(p, 1)] for k, p in r.detail.get("lines", [])]}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("capture_dir", type=Path)
    ap.add_argument("--out", type=Path, default=ROOT / "fixtures" / "offair")
    a = ap.parse_args()
    index = {"source": "2026-11-08 three-band session, 70 cm A->B (K0JLB-2 receiving K0JLB-1)",
             "lead_s": LEAD_S, "tail_s": TAIL_S, "frames": []}
    for name, recording, which, note in FRAMES:
        q, x = mono16(a.capture_dir / recording)
        width, masked = (24, False) if Path(recording).name.startswith("t1") else (32, True)
        link = link_for(width, masked)
        d = preamble.detect(x)[which]
        r = frame.receive_header_first(x, link, start=int(d.start), cp=int(d.cp))
        cfg = r.config
        pay = seeded_payloads(1000 + which, cfg.n_blocks)
        lo = int(d.start) - int(LEAD_S * FS)
        hi = int(d.start) + cfg.n_samples + int(TAIL_S * FS)
        write_wav(a.out / f"{name}.wav", q[lo:hi])
        _, xf = mono16(a.out / f"{name}.wav")
        start = int(LEAD_S * FS)
        entry = {"wav": f"{name}.wav", "recording": recording, "frame": which, "note": note,
                 "capture_start_sample": int(d.start), "start_sample": start, "cp": int(d.cp),
                 "width": width, "mask": sorted(link.mask) if link.mask is not None else None,
                 "mask_id": link.mask_id, "type": int(r.header.frame_type), "mcs": cfg.mcs,
                 "n_blocks": cfg.n_blocks, "n_control": cfg.n_control, "seed": 1000 + which,
                 "header_message_hex": bits_hex(r.header.message_bits()),
                 "control_hex": [bits_hex(b[:240]) for b in r.control_bits],
                 "payload_hex": [bits_hex(p) for p in pay],
                 "python": {"lines_removed": decode_entry(xf, link, start, int(d.cp), True),
                            "lines_kept": decode_entry(xf, link, start, int(d.cp), False)}}
        index["frames"].append(entry)
        print(f"{name}.wav: {(hi - lo) / FS:.2f} s, {2 * (hi - lo) + 44} bytes, MCS {cfg.mcs}, "
              f"blocks {entry['python']['lines_kept']['blocks_ok']}/{cfg.n_blocks} -> "
              f"{entry['python']['lines_removed']['blocks_ok']} with the lines removed")
    (a.out / "weak70.json").write_text(json.dumps(index, indent=1) + "\n")


if __name__ == "__main__":
    main()
