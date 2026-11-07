"""Block error rate of the Stage 1 simulator at one rung and SNR, in AWGN,
for the BLER cross-check with the Rust port (``examples/bler.rs``).

    uv run --package cairn-sim python modem/tools/bler_python.py MCS SNR_DB [--blocks N] [--seed S]

Uses ``link_sim.bler_point`` with the SPEC 6.1 detector (16 blocks per frame,
400-sample guards; a missed preamble or failed header counts every block of
the frame as an error) and prints one JSON line.
"""

from __future__ import annotations

import argparse
import json

import numpy as np

from cairn_sim import link_sim


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("mcs", type=int)
    ap.add_argument("snr_db", type=float)
    ap.add_argument("--width", type=int, default=24)
    ap.add_argument("--blocks", type=int, default=6400)
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    pt = link_sim.bler_point(a.mcs, a.snr_db, width=a.width, min_blocks=a.blocks, max_blocks=a.blocks,
                             min_errors=0, quick_bler=2.0, rng=np.random.default_rng(a.seed))
    print(json.dumps({"mcs": a.mcs, "width": a.width, "snr_db": a.snr_db, "blocks": pt.blocks,
                      "block_errors": pt.block_errors, "frames": pt.frames,
                      "header_errors": pt.header_errors, "missed_frames": pt.missed_frames,
                      "bler": pt.bler}))


if __name__ == "__main__":
    main()
