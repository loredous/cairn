"""The weak 70 cm link of 2026-11-08: what impaired it, and the model of it.

    uv run --package cairn-sim python -m cairn_sim.weak_fm_eval captures CAPTURE_DIR
    uv run --package cairn-sim python -m cairn_sim.weak_fm_eval calibrate CAPTURE_DIR
    uv run --package cairn-sim python -m cairn_sim.weak_fm_eval canceller CAPTURE_DIR
    uv run --package cairn-sim python -m cairn_sim.weak_fm_eval fm_reported [--workers N]

``CAPTURE_DIR`` is the session folder ``2026-11-08-3band`` (``70cm/``,
``125cm/``, ``2m/``: 48 kHz recordings of two-frame DATA bursts at MCS 4-6,
32 tones with tones 0-2 masked, sent by ``cairn-modem tx --type data`` with
the default payload seed, so frame i carries the random payloads of seed
1000 + i and every transmitted symbol can be rebuilt).

``captures`` decodes every frame with the Python receiver, rebuilds what
was sent, and measures the impairment four ways: per data cell (the error
against the sent symbol: per-tone SNR, the error's tail against a complex
Gaussian, its spread over symbols), in the time domain (the residual after
a least-squares FIR fit of the sent waveform to the received one: its
kurtosis, how much of its energy sits in the loudest 1% of 1 ms windows,
and how much louder it is when the sent waveform is large), the lines at
harmonics of 1 kHz in the carrier lead-in, the data and the carrier tail,
and the frame decoded again with those lines (fitted with the sent signal
known) subtracted. Writes ``results/weak_fm_captures.json``.

``calibrate`` runs each 70 cm A->B setup's frames through the model
(``channel.FmChannel``, mic profile, with its measured ``Comb``; the
Gaussian floor set to what the capture measures with the lines removed)
and compares the reported SNR, the per-tone SNR and the blocks decoded.
Writes ``results/weak_fm_calibration.json``.

``canceller`` decodes every captured frame with and without the receivers'
line canceller (``frame.remove_lines``): ``results/weak_fm_canceller.json``.
``fm_reported`` needs no captures: on the FM threshold model it records what
the receiver reports and each rung's BLER at each CNR, for the adaptation
rules (``results/weak_fm_reported.json``). The BLER sweeps on the models are
``experiments`` ``fm_cnr`` and ``comb``.
"""

from __future__ import annotations

import os

for _var in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "1")   # fm_reported runs one process per core

import argparse  # noqa: E402
import json  # noqa: E402
from pathlib import Path  # noqa: E402

import numpy as np  # noqa: E402
from scipy import signal  # noqa: E402

from . import channel, frame, params, preamble  # noqa: E402
from .bench.audio import read_wav  # noqa: E402
from .crc import append_crc  # noqa: E402

FS = params.FS
RESULTS = Path(__file__).resolve().parents[1] / "results"
HARMONICS = 4
LEVEL_SKIP_S = 0.15          # the radios' level ramp at the start of a transmission
# The 70 cm A->B setups (sim/findings.md): the first session, then a new
# antenna on A, another radio on A, that radio with the antenna moved.
RUNS_70CM = ("l", "l2", "l3", "l4")


def load(path: Path) -> np.ndarray:
    _, x = read_wav(path)
    return np.asarray(x).reshape(len(x), -1)[:, 0].astype(float)


def link_for(width: int, masked: bool) -> frame.LinkState:
    """The listener's link state of the captures: 32 tones, tones 0-2 masked
    under mask ID 1 (``--mask 0-2 --mask-id 1``)."""
    if not masked:
        return frame.LinkState(width=width, cap=width)
    return frame.LinkState(width=width, cap=width, mask=frozenset(range(3, width)), mask_id=1)


def seeded_payloads(seed: int, n: int) -> np.ndarray:
    """``cairn-modem tx``'s random payloads for ``seed`` (header bits drawn first)."""
    rng = np.random.default_rng(seed)
    rng.integers(0, 2, params.HEADER_INFO_BITS - 16, dtype=np.uint8)
    return rng.integers(0, 2, n * params.DATA_PAYLOAD_BITS, dtype=np.uint8).reshape(n, params.DATA_PAYLOAD_BITS)


def band(x: np.ndarray, lo: float, hi: float) -> np.ndarray:
    return signal.sosfiltfilt(signal.butter(8, [lo, hi], "bandpass", fs=FS, output="sos"), x)


# --- per-cell errors ------------------------------------------------------------


def sent_frame(r: frame.RxResult, payloads: np.ndarray) -> frame.TxFrame:
    cfg = r.config
    return frame.rebuild(cfg, r.header_bits, append_crc(payloads), r.control_bits if cfg.n_control else None)


def cell_errors(r: frame.RxResult, sent: frame.TxFrame) -> tuple[np.ndarray, np.ndarray]:
    """(sent data symbols, equalised minus sent), data rows x data tones."""
    cfg = r.config
    dt = list(cfg.data_tones)
    xs = sent.grid[cfg.first_data:, dt] / cfg.data_gain
    return xs, r.detail["eq"][r.detail["d0"]:, dt] - xs


def cell_stats(xs: np.ndarray, err: np.ndarray) -> dict:
    p = np.abs(err) ** 2
    pn = p / np.mean(p, 0, keepdims=True)          # each tone by its own error power
    sym = np.mean(pn, 1)
    blocks10 = [float(10 * np.log10(np.mean(sym[i:i + 10]))) for i in range(0, len(sym) - 9, 10)]
    return {"snr_known_db": float(10 * np.log10(np.mean(np.abs(xs) ** 2) / np.mean(p))),
            "tone_snr_db": [round(float(v), 1) for v in 10 * np.log10(np.mean(np.abs(xs) ** 2, 0) / np.mean(p, 0))],
            # complex Gaussian: |e|^2 / mean is Exp(1), P(> 10) = 4.5e-5
            "cells_over_10x": float(np.mean(pn > 10)),
            # spread of each symbol's error over its tones; iid Gaussian: 1/sqrt(tones)
            "symbol_cv": float(np.std(sym) / np.mean(sym)), "symbol_cv_gaussian": float(1 / np.sqrt(p.shape[1])),
            "ten_symbol_spread_db": float(np.std(blocks10))}


# --- time domain ------------------------------------------------------------------


def fir_residual(x: np.ndarray, start: int, sent: np.ndarray, taps: int = 192, pre: int = 48):
    """Least-squares FIR (``taps`` long, ``pre`` of them ahead) from the sent
    waveform to the received one over the frame after the level ramp:
    (received, fit, residual, sent), aligned."""
    skip = int(LEVEL_SKIP_S * FS)
    n = len(sent) - skip - 200
    idx = np.arange(skip, skip + n)
    cols = []
    for k in range(taps):
        j = idx - (k - pre)
        v = np.zeros(n)
        ok = (j >= 0) & (j < len(sent))
        v[ok] = sent[j[ok]]
        cols.append(v)
    m = np.column_stack(cols + [np.ones(n)])
    y = x[start + skip:start + skip + n]
    coef, *_ = np.linalg.lstsq(m, y, rcond=None)
    fit = m @ coef
    return y, fit, y - fit, sent[idx]


def harmonic_fit(res: np.ndarray, f0: float = 1000.0, n_h: int = HARMONICS) -> tuple[np.ndarray, np.ndarray]:
    """The lines at k f0 in ``res`` (least squares, constant amplitude): the
    fitted waveform and each harmonic's amplitude."""
    t = np.arange(len(res)) / FS
    m = np.column_stack([f(2 * np.pi * k * f0 * t) for k in range(1, n_h + 1) for f in (np.cos, np.sin)])
    c, *_ = np.linalg.lstsq(m, res, rcond=None)
    return m @ c, np.hypot(c[0::2], c[1::2])


def residual_stats(rb: np.ndarray, sent: np.ndarray) -> dict:
    win = 48
    m = len(rb) // win
    rp = np.mean(rb[:m * win].reshape(m, win) ** 2, 1)
    sa = np.sqrt(np.mean(sent[:m * win].reshape(m, win) ** 2, 1))
    q = np.quantile(sa, [0.25, 0.75])
    return {"kurtosis": float(np.mean(rb ** 4) / np.mean(rb ** 2) ** 2),
            "top1pct_energy": float(np.sort(rp)[::-1][:max(1, m // 100)].sum() / rp.sum()),
            "envelope_db": float(10 * np.log10(np.mean(rp[sa > q[1]]) / np.mean(rp[sa < q[0]])))}


def line_levels(seg: np.ndarray, f0: float = 1000.0) -> dict | None:
    """dBFS of each harmonic line of ``seg`` and of the 300-3500 Hz rest."""
    n = (len(seg) // 4800) * 4800
    if n < 4800:
        return None
    s = seg[:n] - np.mean(seg[:n])
    p = 2 * np.abs(np.fft.rfft(s)) ** 2 / n ** 2
    f = np.fft.rfftfreq(n, 1 / FS)
    lines = np.zeros(len(f), bool)
    out = {}
    for k in range(1, HARMONICS + 1):
        i = int(round(k * f0 * n / FS))
        out[f"{k}k"] = round(float(10 * np.log10(p[i - 1:i + 2].sum() + 1e-20)), 1)
        lines[i - 2:i + 3] = True
    inband = (f >= 300) & (f <= 3500)
    rest = p[inband & ~lines].sum() * inband.sum() / (inband & ~lines).sum()
    out["rest"] = round(float(10 * np.log10(rest + 1e-20)), 1)
    return out


# --- one capture ------------------------------------------------------------------


def analyse_frame(x: np.ndarray, r: frame.RxResult, seed: int, link: frame.LinkState) -> dict:
    cfg = r.config
    pay = seeded_payloads(seed, cfg.n_blocks)
    sent = sent_frame(r, pay)
    match = all(np.array_equal(r.block_bits[k, :params.DATA_PAYLOAD_BITS], pay[k]) for k in np.flatnonzero(r.block_ok))
    xs, err = cell_errors(r, sent)
    hi = params.band_high_hz(cfg.width)
    y, fit, res, tx = fir_residual(x, r.start, sent.samples)
    comb, amp = harmonic_fit(res)
    sig = float(np.mean(band(fit, 300, hi) ** 2))
    rb, rb_nc = band(res, 300, hi), band(res - comb, 300, hi)
    offset = params.clip_offset_db(cfg.mcs)
    end = r.start + len(sent.samples)
    # The frame again with the lines subtracted (fitted with the sent signal known).
    xc = x.copy()
    s0 = r.start + int(LEVEL_SKIP_S * FS)
    xc[s0:s0 + len(comb)] -= comb
    rc = frame.receive_header_first(xc, link, start=r.start, cp=cfg.cp, cancel_lines=False)
    xs_c, err_c = cell_errors(rc, sent_frame(rc, pay))
    return {
        "mcs": cfg.mcs, "width": cfg.width, "blocks": int(cfg.n_blocks), "blocks_ok": int(r.block_ok.sum()),
        "payloads_match": bool(match), "snr_reported_db": float(r.detail["snr_db"]),
        "data_dbfs": float(10 * np.log10(sig)),
        "cells": cell_stats(xs, err),
        "residual_snr_db": float(10 * np.log10(sig / np.mean(rb ** 2))),
        "residual": residual_stats(rb, tx),
        # line powers against the data's in-band power, and on the 7 dB-equivalent scale
        "lines_dbc": [float(10 * np.log10(a * a / 2 / sig)) for a in amp],
        "lines_dbc_7db": [float(10 * np.log10(a * a / 2 / sig)) - offset for a in amp],
        "lines_dbfs": {"lead_in": line_levels(x[r.start - int(0.26 * FS):r.start - int(0.02 * FS)]),
                       "data": line_levels(res),
                       "tail": line_levels(x[end + int(0.05 * FS):end + int(0.45 * FS)])},
        "lines_removed": {"blocks_ok": int(rc.block_ok.sum()), "snr_reported_db": float(rc.detail["snr_db"]),
                          "residual_snr_db": float(10 * np.log10(sig / np.mean(rb_nc ** 2))),
                          "residual": residual_stats(rb_nc, tx), "cells": cell_stats(xs_c, err_c)},
        "line_wander_db": line_wander(res, sig),
    }


def line_wander(res: np.ndarray, sig: float, win_s: float = 0.25) -> list[float]:
    """Standard deviation over 0.25 s windows of each line's level (dB)."""
    w = int(win_s * FS)
    lv = []
    for i in range(0, len(res) - w + 1, w):
        _, amp = harmonic_fit(res[i:i + w])
        lv.append(20 * np.log10(amp + 1e-12))
    return [float(v) for v in np.std(np.array(lv), axis=0)]


def analyse_capture(path: Path) -> list[dict]:
    x = load(path)
    width, masked = (24, False) if path.name.startswith("t1") else (32, True)
    link = link_for(width, masked)
    out = []
    for i, d in enumerate(preamble.detect(x)):
        r = frame.receive_header_first(x, link, start=int(d.start), cp=int(d.cp), cancel_lines=False)
        if r.config is None or not r.header_ok or not r.config.n_blocks:
            continue
        out.append({"frame": i, **analyse_frame(x, r, 1000 + i, link)})
    return out


CAPTURE_FILES = ([f"70cm/{run}_AtoB_m{m}.wav" for run in RUNS_70CM for m in (4, 5, 6)]
                 + [f"70cm/l_BtoA_m{m}.wav" for m in (4, 5, 6)]
                 + [f"{b}/l_{d}_m{m}.wav" for b in ("125cm", "2m") for d in ("AtoB", "BtoA") for m in (4, 5, 6)])


def run_combs(rows: dict) -> dict[str, channel.Comb]:
    """Each 70 cm A->B setup's lines, 7 dB-equivalent scale, mean over its frames."""
    out = {}
    for run in RUNS_70CM:
        frames = [f for name, fs in rows.items() if name.startswith(f"70cm/{run}_AtoB") for f in fs]
        lv = np.mean([f["lines_dbc_7db"] for f in frames], axis=0)
        wander = float(np.mean([np.mean(f["line_wander_db"][1:3]) for f in frames]))
        out[run] = channel.Comb(tuple(round(float(v), 1) for v in lv), wander_db=round(wander, 1))
    return out


def captures(capture_dir: Path, out: Path) -> dict:
    rows = {}
    for name in CAPTURE_FILES:
        p = capture_dir / name
        if p.exists():
            rows[name] = analyse_capture(p)
            for f in rows[name]:
                print(f"{name} #{f['frame']}: MCS {f['mcs']} {f['blocks_ok']}/{f['blocks']} "
                      f"SNR {f['snr_reported_db']:.1f}, lines {np.round(f['lines_dbc'], 1)} dBc, "
                      f"kurtosis {f['residual']['kurtosis']:.1f}; lines removed: "
                      f"{f['lines_removed']['blocks_ok']}/{f['blocks']} SNR "
                      f"{f['lines_removed']['snr_reported_db']:.1f}", flush=True)
    result = {"frames": rows, "combs": {k: vars(v) for k, v in run_combs(rows).items()}}
    out.write_text(json.dumps(result, indent=1) + "\n")
    return result


# --- calibration ------------------------------------------------------------------


def simulate_frame(mcs: int, n_blocks: int, comb: channel.Comb | None, floor_db: float | None, seed: int,
                   width: int = 32, profile: str = "mic_wide",
                   drive_db: float = channel.MIC_WIDE_DRIVE_DB, cancel: bool = False) -> tuple[frame.RxResult, dict]:
    """One frame as the captures sent it (``width`` tones, tones 0-2 masked),
    through ``profile`` at ``drive_db`` with ``comb`` (7 dB-equivalent
    levels) and a Gaussian floor at audio SNR ``floor_db`` (average-
    referenced; None: none), decoded (with the line canceller if
    ``cancel``) and measured as a capture. The default is the mic chain as
    these radios pass it (to 4.25 kHz), driven as hard as reproduces the
    strong links' distortion."""
    rng = np.random.default_rng(seed)
    link = link_for(width, True)
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=n_blocks, mask=link.mask)
    tx = frame.build(cfg, rng)
    lead = int(0.3 * FS)
    x = np.concatenate([np.zeros(lead), tx.samples, np.zeros(int(0.3 * FS))])
    ch = channel.FmChannel(profile, snr_db=floor_db, drive_db=drive_db, width=width,
                           comb=None if comb is None else comb.scaled(params.clip_offset_db(mcs)))
    y = ch(x, rng)
    r = frame.receive(y, cfg, start=lead, cancel_lines=cancel)
    xs = tx.grid[cfg.first_data:, list(cfg.data_tones)] / cfg.data_gain
    err = r.detail["eq"][r.detail["d0"]:, list(cfg.data_tones)] - xs
    return r, cell_stats(xs, err)


def _summary(caps: list[dict]) -> dict:
    return {"blocks_ok": [c["blocks_ok"] for c in caps],
            "snr_reported_db": [round(c["snr_reported_db"], 1) for c in caps],
            "snr_known_db": [round(c["cells"]["snr_known_db"], 1) for c in caps],
            "tone_snr_db": np.round(np.mean([c["cells"]["tone_snr_db"] for c in caps], 0), 1).tolist()}


def _sim_summary(sims: list) -> dict:
    return {"blocks_ok": [int(r.block_ok.sum()) for r, _ in sims],
            "snr_reported_db": [round(float(r.detail["snr_db"]), 1) for r, _ in sims],
            "snr_known_db": [round(s["snr_known_db"], 1) for _, s in sims],
            "tone_snr_db": np.round(np.mean([s["tone_snr_db"] for _, s in sims], 0), 1).tolist()}


def calibrate(capture_rows: dict, out: Path, frames: int = 4, cancelled: list[dict] | None = None) -> dict:
    """Each 70 cm setup and the 1.25 m reference through the model. Nothing
    is fitted to the 70 cm decode results: the comb is the lines measured in
    the capture, the distortion is the mic_wide drive (set on the strong
    links), and the Gaussian floor is whatever brings the model's known-
    symbol SNR without the comb to the capture's with its lines removed
    (none when the distortion alone already reaches it). Both are decoded
    without the line canceller, and again with it (against ``cancelled``,
    the ``canceller`` command's rows)."""
    by_file: dict[str, list[dict]] = {}
    for c in cancelled or []:
        by_file.setdefault(c["file"], []).append(c["lines_removed"])
    combs = run_combs(capture_rows)
    result = {"combs": {k: vars(v) for k, v in combs.items()}, "drive_db": channel.MIC_WIDE_DRIVE_DB, "runs": []}
    shape = {4: 33, 5: 41, 6: 49}
    dist = {mcs: float(np.mean([simulate_frame(mcs, nb, None, None, 6000 + 10 * mcs + k)[1]["snr_known_db"]
                                for k in range(2)])) for mcs, nb in shape.items()}
    cases = [(run, f"70cm/{run}_AtoB") for run in RUNS_70CM] + [("1.25m", "125cm/l_AtoB")]
    for run, prefix in cases:
        for mcs, nb in shape.items():
            caps = capture_rows[f"{prefix}_m{mcs}.wav"]
            target = float(np.mean([c["lines_removed"]["cells"]["snr_known_db"] for c in caps]))
            excess = 10 ** (-target / 10) - 10 ** (-dist[mcs] / 10)
            floor = float(-10 * np.log10(excess)) if excess > 0 else None
            comb = combs.get(run)
            sims = [simulate_frame(mcs, nb, comb, floor, 7000 + 10 * mcs + k) for k in range(frames)]
            row = {"run": run, "mcs": mcs, "blocks": nb, "comb": None if comb is None else comb.levels_dbc,
                   "floor_audio_snr_db": None if floor is None else round(floor, 1),
                   "known_snr_lines_removed_db": round(target, 1), "known_snr_distortion_db": round(dist[mcs], 1),
                   "capture": _summary(caps), "model": _sim_summary(sims)}
            ct, mt = np.array(row["capture"]["tone_snr_db"]), np.array(row["model"]["tone_snr_db"])
            row["tone_snr_rms_diff_db"] = round(float(np.sqrt(np.mean((ct - mt) ** 2))), 2)
            if f"{prefix}_m{mcs}.wav" in by_file:
                sims_c = [simulate_frame(mcs, nb, comb, floor, 7000 + 10 * mcs + k, cancel=True) for k in range(frames)]
                cc = by_file[f"{prefix}_m{mcs}.wav"]
                row["with_canceller"] = {
                    "capture": {"blocks_ok": [c["blocks_ok"] for c in cc], "snr_reported_db": [c["snr_db"] for c in cc]},
                    "model": {k: v for k, v in _sim_summary(sims_c).items() if k in ("blocks_ok", "snr_reported_db")}}
            result["runs"].append(row)
            wc = row.get("with_canceller", {"capture": {}, "model": {}})
            print(f"{run} MCS {mcs}: capture {row['capture']['blocks_ok']} SNR {row['capture']['snr_reported_db']} | "
                  f"model {row['model']['blocks_ok']} SNR {row['model']['snr_reported_db']} | "
                  f"floor {row['floor_audio_snr_db']} | per-tone rms diff {row['tone_snr_rms_diff_db']} dB | "
                  f"with the canceller: capture {wc['capture'].get('blocks_ok')} model {wc['model'].get('blocks_ok')}",
                  flush=True)
    out.write_text(json.dumps(result, indent=1) + "\n")
    return result


def canceller(capture_dir: Path, out: Path) -> list[dict]:
    """Every captured frame decoded as the listener did, with and without
    the receiver's line canceller (``frame.remove_lines``)."""
    rows = []
    for name in CAPTURE_FILES + [f"70cm/{t}_AtoB.wav" for t in ("t1", "t1d", "t1e", "t1f")]:
        p = capture_dir / name
        if not p.exists():
            continue
        x = load(p)
        width, masked = (24, False) if p.name.startswith("t1") else (32, True)
        link = link_for(width, masked)
        for i, d in enumerate(preamble.detect(x)):
            res = {c: frame.receive_header_first(x, link, start=int(d.start), cp=int(d.cp), cancel_lines=c)
                   for c in (False, True)}
            if res[False].config is None:
                continue
            row = {"file": name, "frame": i, "mcs": res[False].config.mcs, "blocks": int(res[False].block_ok.size)}
            for c, r in res.items():
                row["lines_removed" if c else "lines_kept"] = {
                    "blocks_ok": int(r.block_ok.sum()), "snr_db": round(float(r.detail["snr_db"]), 2),
                    "lines": [[k, round(v, 1)] for k, v in r.detail.get("lines", [])]}
            rows.append(row)
            print(f"{name} #{i}: MCS {row['mcs']} {row['lines_kept']['blocks_ok']}/{row['blocks']} "
                  f"SNR {row['lines_kept']['snr_db']} -> {row['lines_removed']['blocks_ok']} "
                  f"SNR {row['lines_removed']['snr_db']}, lines {row['lines_removed']['lines']}", flush=True)
    out.write_text(json.dumps(rows, indent=1) + "\n")
    return rows


FM_REPORTED_CNRS = (4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 14.0, 16.0, 18.0, 20.0)


def _fm_reported_point(job: tuple[int, int, float, int]) -> dict:
    mcs, width, cnr, frames = job
    rng = np.random.default_rng(9000 + 100 * mcs + int(10 * cnr) + width)
    mask = frozenset(range(3, width)) if width > 24 else None
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=16, mask=mask)
    ch = channel.FmChannel("mic_wide", cnr_db=cnr, width=width)
    snrs, bad, headers = [], 0, 0
    for _ in range(frames):
        tx = frame.build(cfg, rng)
        x = np.concatenate([np.zeros(400), tx.samples, np.zeros(400)])
        r = frame.receive(ch(x, rng), cfg, start=400)
        ok = r.block_ok & np.all(r.block_bits == tx.block_bits, axis=1) & r.header_ok
        bad += int(np.sum(~ok))
        headers += int(not r.header_ok)
        snrs.append(float(r.detail["snr_db"]))
    return {"mcs": mcs, "width": width, "cnr_db": cnr, "frames": frames, "bler": bad / (16 * frames),
            "header_fer": headers / frames, "snr_reported_db": float(np.median(snrs)),
            "snr_reported_p10_db": float(np.percentile(snrs, 10)), "snr_reported_p90_db": float(np.percentile(snrs, 90))}


def fm_reported(out: Path, frames: int = 40, workers: int = 8) -> list[dict]:
    """What the receiver reports (ADR 0006 SNR, 7 dB-equivalent) and each
    rung's BLER on an FM link at each CNR: whether the adaptation rules,
    which see only the reported SNR, would ask for a rung that fails near
    the FM threshold."""
    from concurrent.futures import ProcessPoolExecutor
    jobs = [(m, w, c, frames) for w in (24, 32) for m in range(7) for c in FM_REPORTED_CNRS]
    with ProcessPoolExecutor(workers) as pool:
        rows = list(pool.map(_fm_reported_point, jobs))
    for r in rows:
        print(f"w{r['width']} MCS {r['mcs']} CNR {r['cnr_db']:4.1f}: reported {r['snr_reported_db']:5.1f} dB "
              f"({r['snr_reported_p10_db']:.1f}-{r['snr_reported_p90_db']:.1f}), BLER {r['bler']:.3f}", flush=True)
    out.write_text(json.dumps(rows, indent=1) + "\n")
    return rows


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("command", choices=("captures", "calibrate", "canceller", "fm_reported"))
    ap.add_argument("capture_dir", type=Path, nargs="?")
    ap.add_argument("--out", type=Path, default=RESULTS)
    ap.add_argument("--workers", type=int, default=8)
    a = ap.parse_args(argv)
    if a.command == "fm_reported":
        fm_reported(a.out / "weak_fm_reported.json", workers=a.workers)
        return 0
    cap_json = a.out / "weak_fm_captures.json"
    if a.command == "canceller":
        canceller(a.capture_dir, a.out / "weak_fm_canceller.json")
        return 0
    if a.command == "captures" or not cap_json.exists():
        rows = captures(a.capture_dir, cap_json)["frames"]
    else:
        rows = json.loads(cap_json.read_text())["frames"]
    if a.command == "calibrate":
        canc = a.out / "weak_fm_canceller.json"
        calibrate(rows, a.out / "weak_fm_calibration.json",
                  cancelled=json.loads(canc.read_text()) if canc.exists() else None)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
