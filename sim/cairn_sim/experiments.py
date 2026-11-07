"""Stage 1 experiment runner: writes JSON results to sim/results/.

    uv run --package cairn-sim python -m cairn_sim.experiments awgn [--workers 4]
    uv run --package cairn-sim python -m cairn_sim.experiments clip
    uv run --package cairn-sim python -m cairn_sim.experiments channels
    uv run --package cairn-sim python -m cairn_sim.experiments interleaver
    uv run --package cairn-sim python -m cairn_sim.experiments header

Each experiment is a list of jobs run in parallel processes with fixed
seeds, so results are reproducible. A threshold job finds one rung's 1%
BLER threshold; a points job (``snrs`` given) measures BLER at fixed SNRs.
Worker processes are limited to one BLAS thread each, since the jobs
already run in parallel.

Unless a job sets ``clip_db``, frames are clipped at the rung's level
(``params.clip_db``). Thresholds stay average-referenced; add
``params.clip_offset_db(mcs)`` to put them on the 7 dB-clip scale.
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


def _point_row(p: link_sim.Point) -> dict:
    return asdict(p) | {"bler": p.bler, "header_fer": p.header_fer}


def run_job(job: dict) -> dict:
    t0 = time.time()
    rng = np.random.default_rng(job["seed"])
    if "impairment" in job:                   # interleaver comparison (SPEC 6.4)
        from . import interleaver_eval
        kw = {k: job[k] for k in ("min_blocks", "max_blocks", "min_errors", "quick_bler") if k in job}
        pts = [interleaver_eval.bler(job["mcs"], s, width=job["width"], impairment=job["impairment"],
                                     kind=job["interleaver"], n_blocks=job.get("blocks_per_frame", 2),
                                     rng=rng, rx_kw=job.get("rx_kw"), **kw)
               for s in job["snrs"]]
        return {**job, "points": [_point_row(p) for p in pts], "seconds": round(time.time() - t0, 1)}
    kw = dict(width=job["width"], clip_db=job.get("clip_db", params.clip_db(job["mcs"])),
              clip=job.get("clip", True), chan=_channel(job.get("profile")), rng=rng,
              blocks_per_frame=job.get("blocks_per_frame", 16),
              cp=params.CP_LONG if job.get("profile") == "repeater" else params.CP_SHORT,
              rx_kw=job.get("rx_kw"))
    if "snrs" in job:
        kw.update({k: job[k] for k in ("min_blocks", "max_blocks", "min_errors", "quick_bler") if k in job})
        pts = [link_sim.bler_point(job["mcs"], s, **kw) for s in job["snrs"]]
        return {**job, "points": [_point_row(p) for p in pts], "seconds": round(time.time() - t0, 1)}
    thr, pts = link_sim.threshold(job["mcs"], start_db=job["start_db"], step_db=job.get("step_db", 1.0),
                                  max_db=job.get("max_db", 40.0), **kw)
    return {**job, "threshold_db": thr, "points": [_point_row(p) for p in pts],
            "seconds": round(time.time() - t0, 1)}


def jobs_for(name: str) -> list[dict]:
    expected = {0: 2.0, 1: 5.0, 2: 8.0, 3: 11.0, 4: 15.0, 5: 18.0, 6: 22.0}   # at a 7 dB clip
    # At the rung's own clip level. The SNR axis is average-referenced and
    # MCS 4-6 clip 1-3 dB higher, so their thresholds sit lower on it.
    per_rung = {0: 2.0, 1: 5.0, 2: 8.0, 3: 11.0, 4: 14.0, 5: 17.0, 6: 20.0}
    jobs = []
    if name == "awgn":
        for width in (24, 56):
            for r in params.RUNGS:
                jobs.append({"experiment": name, "mcs": r.mcs, "width": width, "profile": "awgn",
                             "clip_db": params.clip_db(r.mcs), "start_db": per_rung[r.mcs],
                             "seed": 1000 + 10 * r.mcs + width})
    elif name == "clip":
        for clip_db in (7.0, 8.0, 9.0, 10.0, 11.0, 12.0):
            for mcs in (3, 4, 5, 6):
                if clip_db > 10 and mcs < 5:
                    continue             # MCS 3-4 are best at 7-8 dB already
                jobs.append({"experiment": name, "mcs": mcs, "width": 24, "profile": "awgn",
                             "clip_db": clip_db, "start_db": expected[mcs] - 1,
                             "seed": 2000 + 10 * mcs + int(clip_db)})
    elif name == "channels":
        for profile, width in (("mic", 24), ("flat12", 30), ("flat25", 56), ("repeater", 24)):
            for r in params.RUNGS:
                jobs.append({"experiment": name, "mcs": r.mcs, "width": width, "profile": profile,
                             "clip_db": params.clip_db(r.mcs), "start_db": per_rung[r.mcs] + 1,
                             "seed": 3000 + 10 * r.mcs + width})
    elif name == "interleaver":
        # SPEC 6.4: linear against a uniformly random permutation, with one
        # tone dead in the channel (not masked) or a 10 ms burst 20 dB above
        # the signal. 2 blocks per frame; the burst is received with the
        # burst-aware receiver (without it, burst LLRs swamp any interleaver).
        snrs = {("dead_tone", 2): [8.5, 9.5, 10.5], ("dead_tone", 4): [16.5, 17.5, 18.5],
                ("burst", 2): [9.0, 11.0], ("burst", 4): [18.0, 21.0],
                ("none", 2): [8.5, 9.5], ("none", 4): [16.5, 17.5]}
        for (imp, mcs), pts in snrs.items():
            for width in (24, 56):
                for i, kind in enumerate(("linear", "random")):
                    if imp == "none" and kind == "random":
                        continue
                    jobs.append({"experiment": name, "mcs": mcs, "width": width, "impairment": imp,
                                 "interleaver": kind, "snrs": pts, "blocks_per_frame": 2,
                                 "rx_kw": {"burst_aware": imp == "burst"},
                                 "min_blocks": 3000, "max_blocks": 6000, "min_errors": 100,
                                 "quick_bler": 0.95, "seed": 4000 + 100 * mcs + width + i})
    elif name == "header":
        # Header loss at MCS 0 (SPEC 6.3): 1000 short frames per point, so
        # only the header and detector matter, before and after the
        # receiver's header noise estimate (frame.receive header_noise).
        for profile, cp, snrs in (("repeater", params.CP_LONG, [4.0, 5.0, 6.0]),
                                  ("awgn", params.CP_SHORT, [3.0, 3.5, 4.0])):
            for hn in ("s2", "measured"):
                jobs.append({"experiment": name, "mcs": 0, "width": 24, "profile": profile,
                             "snrs": snrs, "blocks_per_frame": 2, "rx_kw": {"header_noise": hn},
                             "min_blocks": 2000, "max_blocks": 2000, "min_errors": 0, "quick_bler": 1.1,
                             "seed": 5000 + (cp == params.CP_LONG)})
    else:
        raise SystemExit(f"unknown experiment {name}")
    return jobs


EXPERIMENTS = ("awgn", "clip", "channels", "interleaver", "header")


def _key(job: dict) -> str:
    """What a job measures, not how it searches. Rows written before the
    per-rung clip carry no clip_db: they ran at the old fixed 7 dB."""
    skip = {"seed", "experiment", "threshold_db", "points", "seconds", "start_db", "step_db", "max_db"}
    full = {"clip_db": params.CLIP_DB_ABOVE_RMS, **job}
    return json.dumps({k: v for k, v in full.items() if k not in skip}, sort_keys=True)


def _complete(row: dict, target: float = 0.01) -> bool:
    """A stored result is final if it has a threshold or a genuine error floor
    (BLER above target at every SNR tried), not a search that never bracketed."""
    if "threshold_db" not in row:
        return True                      # a points job
    if row["threshold_db"] is not None:
        return True
    pts = sorted(row["points"], key=lambda p: p["snr_db"])
    return bool(pts) and pts[0]["bler"] >= target


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("experiment", choices=EXPERIMENTS)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--out", type=Path, default=RESULTS, help="directory for <experiment>.json")
    ap.add_argument("--resume", action="store_true",
                    help="keep finished rows of the existing results file; run only the rest")
    args = ap.parse_args(argv)
    args.out.mkdir(parents=True, exist_ok=True)
    jobs = jobs_for(args.experiment)
    out = args.out / f"{args.experiment}.json"
    results = []
    if args.resume and out.exists():
        done = {_key(r): r for r in json.loads(out.read_text()) if _complete(r)}
        results = [done[_key(j)] for j in jobs if _key(j) in done]
        jobs = [j for j in jobs if _key(j) not in done]
        print(f"resuming: {len(results)} rows kept, {len(jobs)} to run", flush=True)
    with ProcessPoolExecutor(args.workers) as pool:
        for res in pool.map(run_job, jobs):
            results.append(res)
            if "threshold_db" in res:
                print(f"{res['experiment']} mcs{res['mcs']} w{res['width']} {res.get('profile')} "
                      f"clip{res.get('clip_db', params.clip_db(res['mcs']))}: threshold "
                      f"{res['threshold_db']} dB ({res['seconds']} s)", flush=True)
            else:
                tag = {k: res[k] for k in ("profile", "impairment", "interleaver", "rx_kw") if k in res}
                pts = ", ".join(f"{p['snr_db']} dB: BLER {p['bler']:.4f} hdr {p['header_fer']:.4f} "
                                f"({p['blocks']} blk)" for p in res["points"])
                print(f"{res['experiment']} mcs{res['mcs']} w{res['width']} {tag}: {pts} "
                      f"({res['seconds']} s)", flush=True)
            out.write_text(json.dumps(results, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
