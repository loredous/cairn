"""Block error rate with and without the receiver's level tracking, through
the radios' level ramp at the start of a frame.

    uv run --package cairn-sim python sim/scripts/level_eval.py [--workers N] [--json PATH]
        [--mcs 0,1,...] [--widths 24,32] [--chans awgn,mic] [--ramps none,tx,rx] [--track 1,0]
        [--snrs a,b,...]

For each rung, width, channel (white noise, or the SPEC 14.2 ``mic`` FM
chain) and ramp, the 1% block-error threshold (``link_sim.threshold``, 16
blocks per frame, the frame found by the SPEC 6.1 detector) of the receiver
with level tracking (``frame.receive(track_levels=True)``) and without it,
on the same frames and noise (the same seed). With ``--snrs``, BLER at those
SNRs instead of thresholds.

Ramps (a frequency-flat gain from the first preamble sample):

* ``none``: no ramp.
* ``tx``: the gain applied before the channel (the radio's transmit audio
  AGC, ahead of its limiter; the noise is not ramped).
* ``rx``: the gain applied to the received signal and noise alike (as the
  captures behave, whose noise is mostly the radios' own clipping).
* ``tx14``, ``rx14``: the same with PR #28's milder model (``short_frame_eval``).

The ramp is ``1 - (1 - g0) exp(-t / tau)``: by default g0 = -30 dB and
tau = 40 ms, which leaves the S2 pair 4.9 dB low (the 2026-11-07 captures:
3.3-4.0 dB A to B, 5.4-6.3 dB B to A, fitted with tau 30-35 and 43-52 ms) and
the first data symbols 0.3 dB low; ``*14`` is -14 dB and 19 ms (S2 1.4 dB
low). The SNR is the steady-state audio SNR.
"""

from __future__ import annotations

import os

for _var in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "1")

import argparse  # noqa: E402
import json  # noqa: E402
import time  # noqa: E402
from concurrent.futures import ProcessPoolExecutor  # noqa: E402

import numpy as np  # noqa: E402

from cairn_sim import channel, link_sim, params  # noqa: E402

RAMPS = {"none": None, "tx": (-30.0, 0.040), "rx": (-30.0, 0.040), "tx14": (-14.0, 0.019), "rx14": (-14.0, 0.019)}
GUARD = 400              # link_sim.bler_point's default guard: the frame starts here
START_DB = {0: 2.0, 1: 5.0, 2: 8.0, 3: 11.0, 4: 14.0, 5: 17.0, 6: 20.0}   # experiments.jobs_for("awgn")


def ramp_gain(n: int, start: int, g0_db: float, tau_s: float) -> np.ndarray:
    """1 - (1 - g0) exp(-t / tau) from sample ``start`` (1 before it: the guard is silent anyway)."""
    t = np.maximum(np.arange(n) - start, 0) / params.FS
    return 1 - (1 - 10 ** (g0_db / 20)) * np.exp(-t / tau_s)


def make_channel(chan: str, ramp: str):
    """A ``link_sim`` channel: ``chan`` with ``ramp`` (see the module docstring)."""
    model = RAMPS[ramp]

    def run(x, snr_db, width, rng):
        p = float(np.mean(x ** 2))               # steady-state reference: the unramped frame
        if model is not None and ramp.startswith("tx"):
            x = x * ramp_gain(x.size, GUARD, *model)
        if chan == "awgn":
            y = channel.awgn(x, snr_db, width, rng, signal_power=p)
        else:
            y = channel.FmChannel(profile=chan, snr_db=snr_db, width=width)(x, rng)
        if model is not None and ramp.startswith("rx"):
            y = y * ramp_gain(y.size, GUARD, *model)
        return y
    return run


def run_job(job: dict) -> dict:
    t0 = time.time()
    rng = np.random.default_rng(job["seed"])
    kw = dict(width=job["width"], clip_db=params.clip_db(job["mcs"]), chan=make_channel(job["chan"], job["ramp"]),
              rng=rng, blocks_per_frame=16, cp=params.CP_SHORT, rx_kw={"track_levels": bool(job["track"])})
    if job.get("snrs"):
        pts = [link_sim.bler_point(job["mcs"], s, **kw) for s in job["snrs"]]
        return {**job, "points": [dict(snr_db=p.snr_db, blocks=p.blocks, block_errors=p.block_errors,
                                       frames=p.frames, header_errors=p.header_errors, bler=p.bler) for p in pts],
                "seconds": round(time.time() - t0, 1)}
    thr, pts = link_sim.threshold(job["mcs"], start_db=job["start_db"], step_db=1.0, max_db=40.0, **kw)
    return {**job, "threshold_db": thr,
            "points": [dict(snr_db=p.snr_db, blocks=p.blocks, block_errors=p.block_errors, frames=p.frames,
                            header_errors=p.header_errors, bler=p.bler) for p in pts],
            "seconds": round(time.time() - t0, 1)}


def jobs(mcs: list[int], widths: list[int], chans: list[str], ramps: list[str], tracks: list[int],
         snrs: list[float] | None) -> list[dict]:
    out = []
    for c, chan in enumerate(chans):
        for r, ramp in enumerate(ramps):
            for width in widths:
                for m in mcs:
                    seed = 70000 + 1000 * c + 100 * r + 10 * m + width     # shared by both receivers
                    start = START_DB[m] + (1.0 if chan != "awgn" else 0.0)
                    for track in tracks:
                        out.append(dict(mcs=m, width=width, chan=chan, ramp=ramp, track=track, seed=seed,
                                        start_db=start, snrs=snrs))
    # Longest first (high rungs with a ramp and no tracking search furthest).
    return sorted(out, key=lambda j: (-(j["ramp"] != "none") * (1 - j["track"]), -j["mcs"]))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--workers", type=int, default=None)
    ap.add_argument("--json", type=str, default=None)
    ap.add_argument("--mcs", type=str, default="0,1,2,3,4,5,6")
    ap.add_argument("--widths", type=str, default="24,32")
    ap.add_argument("--chans", type=str, default="awgn,mic")
    ap.add_argument("--ramps", type=str, default="none,tx,rx")
    ap.add_argument("--track", type=str, default="1,0")
    ap.add_argument("--snrs", type=str, default=None)
    a = ap.parse_args()
    js = jobs([int(v) for v in a.mcs.split(",")], [int(v) for v in a.widths.split(",")], a.chans.split(","),
              a.ramps.split(","), [int(v) for v in a.track.split(",")],
              [float(v) for v in a.snrs.split(",")] if a.snrs else None)
    results = []
    with ProcessPoolExecutor(a.workers) as ex:
        for res in ex.map(run_job, js):
            results.append(res)
            what = (f"threshold {res['threshold_db']}" if "threshold_db" in res
                    else ", ".join(f"{p['snr_db']}: {p['bler']:.4f}" for p in res["points"]))
            print(f"mcs{res['mcs']} w{res['width']} {res['chan']:4} ramp {res['ramp']:4} track {res['track']}: "
                  f"{what} ({res['seconds']} s)", flush=True)
            if a.json:
                with open(a.json, "w") as f:
                    json.dump(results, f, indent=1)


if __name__ == "__main__":
    main()
