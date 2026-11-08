"""Block error rate with and without the receiver's header channel
re-estimation, on the same frames.

    uv run --package cairn-sim python sim/scripts/header_channel_eval.py [--workers N] [--json PATH]
        [--mcs 0,1,...] [--widths 24,32] [--chans awgn,mic] [--ramps none,rx,tx] [--snrs a,b,...]

For each rung, width, channel (white noise, or the SPEC 14.2 ``mic`` FM
chain) and ramp (``level_eval``'s: none, or the capture-like ramp of the
radios' level at the start of a frame applied to signal and noise, ``rx``,
or before the noise, ``tx``), the 1% block-error threshold of the receiver
with the header re-estimating the base tones' channel
(``frame.receive(header_channel=True)``, the default) and without it
(the S2 pair's estimate alone), both with level tracking.

The method is ``level_eval``'s (``link_sim``: 16 blocks per frame, the
frame found by the SPEC 6.1 detector, at least 2000 blocks and 100 errors
per point, at most 20000, a point above 30% BLER after 480 blocks stops
early; a 1 dB step search from ``level_eval.START_DB`` and one point half a
step below the first under 1%, log-interpolated), with one change that
makes the pairing exact: every frame is decoded by both receivers, so they
see the same frames and noise, and the search steps on until both are under
1% (each receiver's threshold interpolated from the shared points). With
``--snrs``, BLER at those SNRs instead of thresholds.

``sim/results/header_channel.json`` holds ``--ramps none,rx`` at every rung
and ``--ramps tx --mcs 4,5,6`` (below MCS 4 the ramp ahead of the noise is a
detection problem, about 17 dB either way: ``level_eval``).
"""

from __future__ import annotations

import os

for _var in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "1")

import argparse  # noqa: E402
import json  # noqa: E402
import sys  # noqa: E402
import time  # noqa: E402
from concurrent.futures import ProcessPoolExecutor  # noqa: E402
from pathlib import Path  # noqa: E402

import numpy as np  # noqa: E402

from cairn_sim import frame, link_sim, params, preamble  # noqa: E402

sys.path.insert(0, str(Path(__file__).resolve().parent))
from level_eval import GUARD, START_DB, make_channel  # noqa: E402

VARIANTS = {"hdr": {"header_channel": True}, "s2": {"header_channel": False}}
TARGET = 0.01
STEP_DB = 1.0
MAX_DB = 40.0


def paired_point(mcs: int, snr_db: float, width: int, chan, rng: np.random.Generator, *,
                 blocks_per_frame: int = 16, min_blocks: int = 2000, min_errors: int = 100,
                 max_blocks: int = 20000, quick_bler: float = 0.3, quick_blocks: int = 480) -> dict:
    """``link_sim.bler_point`` for every receiver in ``VARIANTS`` on the same
    frames: run until each has ``min_blocks`` and ``min_errors`` (or
    ``max_blocks``), or every one is above ``quick_bler`` after ``quick_blocks``."""
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=blocks_per_frame, clip_db=params.clip_db(mcs),
                            cp=params.CP_SHORT)
    pad = np.zeros(GUARD)
    blocks, frames = 0, 0
    errors = {v: 0 for v in VARIANTS}
    header_errors = {v: 0 for v in VARIANTS}

    def done() -> bool:
        if blocks >= max_blocks:
            return True
        if blocks >= quick_blocks and all(errors[v] / blocks > quick_bler for v in VARIANTS):
            return True
        return blocks >= min_blocks and all(errors[v] >= min_errors for v in VARIANTS)

    while not done():
        tx = frame.build(cfg, rng)
        rx = chan(np.concatenate([pad, tx.samples, pad]), snr_db, width, rng)
        dets = sorted((d for d in preamble.detect(rx) if d.cp == cfg.cp and abs(d.start - GUARD) < cfg.stride),
                      key=lambda d: -d.quality)
        frames += 1
        blocks += blocks_per_frame
        if not dets:
            for v in VARIANTS:
                errors[v] += blocks_per_frame
            continue
        for v, kw in VARIANTS.items():
            r = frame.receive(rx, cfg, start=dets[0].start, measure_snr=False, **kw)
            good = r.block_ok & np.all(r.block_bits == tx.block_bits, axis=1)
            if not r.header_ok:
                good[:] = False
                header_errors[v] += 1
            errors[v] += int(np.sum(~good))
    return {"snr_db": snr_db, "blocks": blocks, "frames": frames,
            "block_errors": errors, "header_errors": header_errors}


def _points(pts: list[dict], v: str) -> list[link_sim.Point]:
    return [link_sim.Point(p["snr_db"], p["blocks"], p["block_errors"][v], p["frames"], p["header_errors"][v])
            for p in pts]


def paired_threshold(mcs: int, start_db: float, **kw) -> tuple[dict, list[dict]]:
    """``link_sim.threshold`` for every receiver on shared points."""
    pts = [paired_point(mcs, start_db, **kw)]

    def below(p: dict, v: str) -> bool:
        return p["block_errors"][v] / p["blocks"] < TARGET

    if any(below(pts[0], v) for v in VARIANTS):
        lo = start_db
        for _ in range(8):                      # started below for someone: step down until all are above
            lo -= STEP_DB
            pts.insert(0, paired_point(mcs, lo, **kw))
            if not any(below(pts[0], v) for v in VARIANTS):
                break
    snr = pts[-1]["snr_db"]
    while not all(below(pts[-1], v) for v in VARIANTS) and snr < MAX_DB:
        snr += STEP_DB
        pts.append(paired_point(mcs, snr, **kw))
    mids = set()
    for v in VARIANTS:
        first = next((p["snr_db"] for p in pts if below(p, v)), None)
        if first is not None:
            mids.add(first - STEP_DB / 2)
    pts += [paired_point(mcs, m, **kw) for m in sorted(mids)]
    pts.sort(key=lambda p: p["snr_db"])
    thr = {v: link_sim.interpolate_threshold(_points(pts, v), TARGET) for v in VARIANTS}
    return thr, pts


def run_job(job: dict) -> dict:
    t0 = time.time()
    rng = np.random.default_rng(job["seed"])
    kw = dict(width=job["width"], chan=make_channel(job["chan"], job["ramp"]), rng=rng)
    if job.get("snrs"):
        pts = [paired_point(job["mcs"], s, **kw) for s in job["snrs"]]
        return {**job, "points": pts, "seconds": round(time.time() - t0, 1)}
    thr, pts = paired_threshold(job["mcs"], job["start_db"], **kw)
    return {**job, "threshold_db": thr, "points": pts, "seconds": round(time.time() - t0, 1)}


def jobs(mcs: list[int], widths: list[int], chans: list[str], ramps: list[str], snrs: list[float] | None) -> list[dict]:
    out = []
    for c, chan in enumerate(chans):
        for r, ramp in enumerate(ramps):
            for width in widths:
                for m in mcs:
                    out.append(dict(mcs=m, width=width, chan=chan, ramp=ramp,
                                    seed=80000 + 1000 * c + 100 * r + 10 * m + width,
                                    start_db=START_DB[m] + (1.0 if chan != "awgn" else 0.0), snrs=snrs))
    return sorted(out, key=lambda j: (-j["mcs"], j["ramp"] == "none"))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--workers", type=int, default=None)
    ap.add_argument("--json", type=str, default=None)
    ap.add_argument("--mcs", type=str, default="0,1,2,3,4,5,6")
    ap.add_argument("--widths", type=str, default="24,32")
    ap.add_argument("--chans", type=str, default="awgn,mic")
    ap.add_argument("--ramps", type=str, default="none,rx")
    ap.add_argument("--snrs", type=str, default=None)
    a = ap.parse_args()
    js = jobs([int(v) for v in a.mcs.split(",")], [int(v) for v in a.widths.split(",")], a.chans.split(","),
              a.ramps.split(","), [float(v) for v in a.snrs.split(",")] if a.snrs else None)
    results = []
    with ProcessPoolExecutor(a.workers) as ex:
        for res in ex.map(run_job, js):
            results.append(res)
            if "threshold_db" in res:
                t = res["threshold_db"]
                d = (t["hdr"] - t["s2"]) if None not in t.values() else None
                what = f"hdr {t['hdr']} s2 {t['s2']} diff {d}"
            else:
                what = ", ".join(f"{p['snr_db']}: " + " ".join(f"{v} {p['block_errors'][v] / p['blocks']:.4f}"
                                                              for v in VARIANTS) for p in res["points"])
            print(f"mcs{res['mcs']} w{res['width']} {res['chan']:4} ramp {res['ramp']:4}: {what} "
                  f"({res['seconds']} s)", flush=True)
            if a.json:
                with open(a.json, "w") as f:
                    json.dump(results, f, indent=1)


if __name__ == "__main__":
    main()
