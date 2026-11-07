"""Stage 1 experiment runner: writes JSON results to sim/results/.

    uv run --package cairn-sim python -m cairn_sim.experiments awgn [--workers 4]
    uv run --package cairn-sim python -m cairn_sim.experiments clip
    uv run --package cairn-sim python -m cairn_sim.experiments channels

Each experiment is a list of jobs (one rung threshold each) run in parallel
processes with fixed seeds, so results are reproducible. Worker processes
are limited to one BLAS thread each, since the jobs already run in parallel.
"""

from __future__ import annotations

import os

for _var in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "1")

import argparse  # noqa: E402
import json
import time
from concurrent.futures import ProcessPoolExecutor
from dataclasses import asdict
from pathlib import Path

import numpy as np

from . import link_sim, params

RESULTS = Path(__file__).resolve().parents[1] / "results"


def _channel(profile: str | None):
    if profile in (None, "awgn"):
        return link_sim.awgn_channel
    from . import channel  # the FM chain (channel.FmChannel), when available

    def run(x, snr_db, width, rng, _p=profile):
        return channel.FmChannel(profile=_p, snr_db=snr_db, width=width)(x, rng)
    return run


def run_job(job: dict) -> dict:
    t0 = time.time()
    rng = np.random.default_rng(job["seed"])
    kw = dict(width=job["width"], clip_db=job.get("clip_db", params.CLIP_DB_ABOVE_RMS),
              clip=job.get("clip", True), chan=_channel(job.get("profile")), rng=rng,
              blocks_per_frame=job.get("blocks_per_frame", 16),
              cp=params.CP_LONG if job.get("profile") == "repeater" else params.CP_SHORT)
    thr, pts = link_sim.threshold(job["mcs"], start_db=job["start_db"], step_db=job.get("step_db", 1.0),
                                  max_db=job.get("max_db", 40.0), **kw)
    return {**job, "threshold_db": thr, "points": [asdict(p) | {"bler": p.bler} for p in pts],
            "seconds": round(time.time() - t0, 1)}


def jobs_for(name: str) -> list[dict]:
    expected = {0: 2.0, 1: 5.0, 2: 8.0, 3: 11.0, 4: 15.0, 5: 18.0, 6: 22.0}
    jobs = []
    if name == "awgn":
        for width in (24, 56):
            for r in params.RUNGS:
                jobs.append({"experiment": name, "mcs": r.mcs, "width": width, "profile": "awgn",
                             "start_db": expected[r.mcs], "seed": 1000 + 10 * r.mcs + width})
    elif name == "clip":
        for clip_db in (7.0, 8.0, 9.0, 10.0):
            for mcs in (3, 4, 5, 6):
                jobs.append({"experiment": name, "mcs": mcs, "width": 24, "profile": "awgn",
                             "clip_db": clip_db, "start_db": expected[mcs] - 1,
                             "seed": 2000 + 10 * mcs + int(clip_db)})
    elif name == "channels":
        for profile, width in (("mic", 24), ("flat12", 30), ("flat25", 56), ("repeater", 24)):
            for r in params.RUNGS:
                jobs.append({"experiment": name, "mcs": r.mcs, "width": width, "profile": profile,
                             "start_db": expected[r.mcs] + 1, "seed": 3000 + 10 * r.mcs + width})
    else:
        raise SystemExit(f"unknown experiment {name}")
    return jobs


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("experiment", choices=["awgn", "clip", "channels"])
    ap.add_argument("--workers", type=int, default=4)
    args = ap.parse_args(argv)
    RESULTS.mkdir(exist_ok=True)
    jobs = jobs_for(args.experiment)
    out = RESULTS / f"{args.experiment}.json"
    results = []
    with ProcessPoolExecutor(args.workers) as pool:
        for res in pool.map(run_job, jobs):
            results.append(res)
            print(f"{res['experiment']} mcs{res['mcs']} w{res['width']} {res.get('profile')} "
                  f"clip{res.get('clip_db', 7.0)}: threshold {res['threshold_db']} dB "
                  f"({res['seconds']} s)", flush=True)
            out.write_text(json.dumps(results, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
