"""Clock-drift error and channel-SNR bias of short and long frames.

    uv run --package cairn-sim python sim/scripts/short_frame_eval.py [--trials N] [--jobs J] [--json PATH]

Frames: header only (a PROBE), header plus one control block (an ACK or
SOUND), one MCS 2 data block with a control block, and 13 MCS 2 blocks with
a control block (a 4 s frame), at 24 tones. Channels: white noise and the
SPEC 14.2 ``mic`` FM chain, each at several SNRs and sample-clock offsets of
0 and +-100 ppm, with and without the "onset" ramp the 2026-11-07
over-the-air captures show (a frequency-flat gain rising from -14 dB with a
19 ms time constant from the first preamble sample), applied before the
noise ("tx") or to signal and noise alike ("rx", as the captures behave: the
noise there is mostly the radios' clipping, which follows the level). For
each point: the
fraction of frames fully decoded, the clock-offset error (reported minus
true, ppm: mean, standard deviation, worst), and the reported 7 dB-equivalent
SNR (mean and its difference from the true SNR for white noise, from the
long frame for the FM chain).
"""

from __future__ import annotations

import argparse
import importlib
import itertools
import json
from concurrent.futures import ProcessPoolExecutor

import numpy as np

from cairn_sim import channel, frame, params

FRAMES = {
    "hdr": dict(mcs=0, n_blocks=0, n_control=0),
    "ctrl": dict(mcs=0, n_blocks=0, n_control=1),
    "1blk": dict(mcs=2, n_blocks=1, n_control=1),
    "long": dict(mcs=2, n_blocks=13, n_control=1),
}
ONSET_DB = -14.0       # gain at the first preamble sample (the captures: -13 to -22 dB)
ONSET_TAU_S = 0.019    # time constant of the rise
GUARD = 600
RX = frame        # the receiver evaluated (--module)


def onset(n: int, start: int) -> np.ndarray:
    """Gain ramp 1 - (1 - g0) exp(-t / tau) from sample ``start``."""
    t = np.maximum(np.arange(n) - start, 0) / params.FS
    g = 1 - (1 - 10 ** (ONSET_DB / 20)) * np.exp(-t / ONSET_TAU_S)
    g[: start] = 0.0
    return g


def one(args):
    kind, chan, snr, ppm, ramp, seed = args
    rng = np.random.default_rng(seed)
    cfg = frame.FrameConfig(width=24, **FRAMES[kind])
    tx = frame.build(cfg, rng)
    x = np.concatenate([np.zeros(GUARD), tx.samples, np.zeros(GUARD)])
    if ramp == "tx":
        x = x * onset(x.size, GUARD)
    off = cfg.clip_db - 7.0
    if chan == "awgn":
        # Noise referenced to the steady-state frame power: the true SNR is
        # what the data (or the frame's last segment) sees.
        last = cfg.first_data if cfg.n_blocks else (cfg.first_control if cfg.n_control else params.PREAMBLE_SYMBOLS)
        p = float(np.mean(tx.samples[last * cfg.stride:] ** 2))
        y = channel.resample(x, 1 + ppm * 1e-6) if ppm else x
        y = channel.awgn(y, snr - off, 24, rng, signal_power=p)
    else:
        y = channel.FmChannel(chan, snr_db=snr - off, sco_ppm=ppm, width=24)(x, rng)
    start = int(round(GUARD * (1 + ppm * 1e-6)))
    if ramp == "rx":
        y = y * onset(y.size, start)
    r = RX.receive(y, cfg, start=start)
    ok = bool(r.header_ok and r.block_ok.all() and np.all(r.control_ok))
    rep_ppm = -r.detail["drift"] / cfg.stride * 1e6
    return dict(kind=kind, chan=chan, snr=snr, ppm=ppm, ramp=ramp, ok=ok, ppm_err=rep_ppm - ppm,
                snr_rep=float(r.detail["snr_db"]), fallback=bool(r.detail["snr_fallback"]))


def summarize(rows):
    out = []
    key = lambda r: (r["chan"], r["ramp"], r["snr"], r["ppm"], r["kind"])  # noqa: E731
    rows = sorted(rows, key=key)
    groups = {k: list(g) for k, g in itertools.groupby(rows, key=key)}
    for (chan, ramp, snr, ppm, kind), g in groups.items():
        e = np.array([r["ppm_err"] for r in g])
        s = np.array([r["snr_rep"] for r in g])
        long = groups.get((chan, ramp, snr, ppm, "long"))
        s_long = float(np.mean([r["snr_rep"] for r in long])) if long else float("nan")
        out.append(dict(chan=chan, ramp=ramp, snr=snr, ppm=ppm, kind=kind, n=len(g),
                        decoded=float(np.mean([r["ok"] for r in g])),
                        ppm_err_mean=float(e.mean()), ppm_err_sd=float(e.std()),
                        ppm_err_max=float(np.abs(e).max()),
                        snr_mean=float(s.mean()), snr_bias_true=float(s.mean() - snr),
                        snr_bias_long=float(s.mean() - s_long),
                        fallback=float(np.mean([r["fallback"] for r in g]))))
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--trials", type=int, default=20)
    ap.add_argument("--jobs", type=int, default=None)
    ap.add_argument("--json", type=str, default=None)
    ap.add_argument("--snrs", type=str, default="8,13,20,30")
    ap.add_argument("--chans", type=str, default="awgn,mic")
    ap.add_argument("--ramps", type=str, default="none,tx,rx")
    ap.add_argument("--module", type=str, default="cairn_sim.frame",
                    help="receiver module to evaluate (e.g. a copy of an earlier frame.py)")
    a = ap.parse_args()
    global RX
    RX = importlib.import_module(a.module)
    snrs = [float(v) for v in a.snrs.split(",")]
    jobs = []
    seed = 1000
    for chan in a.chans.split(","):
        for ramp in a.ramps.split(","):
            for snr in snrs:
                for ppm in (-100.0, 0.0, 100.0):
                    for kind in FRAMES:
                        for _ in range(a.trials):
                            seed += 1
                            jobs.append((kind, chan, snr, ppm, ramp, seed))
    with ProcessPoolExecutor(a.jobs) as ex:
        rows = list(ex.map(one, jobs, chunksize=4))
    summary = summarize(rows)
    print(f"{'chan':5} {'ramp':4} {'snr':>4} {'ppm':>5} {'kind':5} {'dec':>5} {'err':>7} {'sd':>6} {'max':>6} "
          f"{'SNR':>5} {'-true':>6} {'-long':>6} {'fb':>4}")
    for s in summary:
        print(f"{s['chan']:5} {s['ramp']:4} {s['snr']:4.0f} {s['ppm']:+5.0f} {s['kind']:5} "
              f"{s['decoded']:5.2f} {s['ppm_err_mean']:+7.1f} {s['ppm_err_sd']:6.1f} {s['ppm_err_max']:6.1f} "
              f"{s['snr_mean']:5.1f} {s['snr_bias_true']:+6.2f} {s['snr_bias_long']:+6.2f} {s['fallback']:4.2f}")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(dict(summary=summary, rows=rows), f, indent=1)


if __name__ == "__main__":
    main()
