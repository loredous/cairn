"""Probe-extent detection (ADR 0007 ``broadcast-width-from-probe``) against SNR.

    uv run --package cairn-sim python -m cairn_sim.extent_eval [--trials 200] [--workers 4]

For each channel (AWGN at the frame's width; the mic FmChannel, whose
transmit and receive low-pass filters roll off above 3 kHz) and each width,
UI broadcasts at MCS 0 with real headers go through the channel and
``frame.probe_extent`` estimates the width from the probe (given the true
start, so detection and header decoding are measured apart). Reported per
SNR: width errors in ``trials`` frames, and the worst decision margins at
the edge (the last probed tone over its threshold, the threshold over the
first tone beyond), in dB; with ``--decode``, the data's block error rate
at the true width and header first (``frame.receive`` without a
configuration: width from the probe, neighbours tried when nothing
decodes), for comparison. The MCS 0 data threshold is 3.5 dB in
AWGN (SPEC 6.2); through the mic path's roll-off the upper tones need more.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ProcessPoolExecutor

import numpy as np

from . import channel, frame, link_sim, params
from .fields import Address, FrameType, Header

AWGN_WIDTHS = (25, 26, 28, 32, 40, 48, 55, 56)
MIC_WIDTHS = (25, 26, 27, 28, 29, 30)
SNRS = (-3.0, -1.0, 0.0, 1.0, 2.0, 3.5, 6.0, 10.0, 20.0, None)


def trial(profile: str, width: int, snr: float | None, n_data: int, rng: np.random.Generator,
          mcs: int = 0, decode: bool = False) -> tuple[frame.ProbeExtent, int, int]:
    """One broadcast through the channel: its probe extent, and with
    ``decode`` the data blocks that fail when decoded at the true width and
    when decoded header first, the width from the probe (else 0, 0)."""
    hdr = Header(FrameType.UI, Address("N0CALL", 1), Address.broadcast_address(), sequence=7,
                 mcs=mcs, n_data=n_data)
    tx = frame.build_frame(hdr, link=frame.LinkState(width=width), rng=rng)
    lead = 2000 + int(rng.integers(0, 540))
    x = np.concatenate([np.zeros(lead), tx.samples, np.zeros(4000)])
    if profile == "awgn":
        y = x if snr is None else link_sim.awgn_channel(x, snr, width, rng)
    else:
        y = channel.FmChannel(profile=profile, snr_db=snr, width=width)(x, rng)
    # The mic filters' delay is well inside the prefix: the true start serves.
    e = frame.probe_extent(y, lead, params.CP_SHORT, tx.header)
    failed = failed_hf = 0
    if decode:
        r = frame.receive(y, tx.config, start=lead, measure_snr=False)
        failed = int(np.sum(~r.block_ok))
        h = frame.receive(y, start=lead, measure_snr=False)
        failed_hf = n_data if h.header is None else int(np.sum(~h.block_ok))
    return e, failed, failed_hf


def _point(args):
    profile, width, snr, trials, n_data, seed, decode = args
    rng = np.random.default_rng(seed)
    errors, inside, outside, got, failed, failed_hf = 0, np.inf, np.inf, {}, 0, 0
    for _ in range(trials):
        e, f, fh = trial(profile, width, snr, n_data, rng, decode=decode)
        failed += f
        failed_hf += fh
        got[e.width] = got.get(e.width, 0) + 1
        errors += e.width != width
        a, b = e.margin_db()
        inside, outside = min(inside, a), min(outside, b)
    n = trials * n_data
    return profile, width, snr, trials, errors, inside, outside, got, failed / n, failed_hf / n


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--trials", type=int, default=100)
    ap.add_argument("--blocks", type=int, default=3, help="data blocks per frame (MCS 0)")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--profiles", default="awgn,mic")
    ap.add_argument("--decode", action="store_true", help="also decode the data at the true width (BLER)")
    a = ap.parse_args()
    jobs = []
    for prof in a.profiles.split(","):
        widths = AWGN_WIDTHS if prof == "awgn" else MIC_WIDTHS
        for w in widths:
            for i, snr in enumerate(SNRS):
                jobs.append((prof, w, snr, a.trials, a.blocks, 1000 * w + i, a.decode))
    with ProcessPoolExecutor(a.workers) as ex:
        res = list(ex.map(_point, jobs))
    print(f"{'channel':8s} {'width':>5s} {'SNR':>6s} {'errors':>9s} {'in_dB':>7s} {'out_dB':>7s} "
          f"{'BLER':>7s} {'BLER_hf':>7s}  widths_found")
    for prof, w, snr, n, err, i, o, got, bler, bler_hf in res:
        s = "clean" if snr is None else f"{snr:+.1f}"
        b = f"{bler:7.3f} {bler_hf:7.3f}" if a.decode else "      -       -"
        print(f"{prof:8s} {w:5d} {s:>6s} {err:4d}/{n:<4d} {i:7.1f} {o:7.1f} {b}  {dict(sorted(got.items()))}")


if __name__ == "__main__":
    main()
