"""``cairn-bench rx``: decode a recording offline and score it against the manifest.

Pipeline: read and resample to 48 kHz, scan the whole recording for preambles
(``preamble.detect``, any prefix, a +-300 ppm clock grid), match detections to
the manifest's frames by time (a voting search for the recording's offset, then
a linear fit that also gives the sound-card clock offset), optionally resample
the recording by that offset, decode every matched frame with
``frame.receive`` and measure it. Frames with no detection are reported as
missed, detections matching no frame as extra.
"""

from __future__ import annotations

import json

import numpy as np

from .. import frame, ofdm, params, preamble
from ..channel import resample as fractional_resample
from . import audio

FS = params.FS
PPM_GRID = (-300.0, -200.0, -100.0, 0.0, 100.0, 200.0, 300.0)
CORRECT_PPM_ABOVE = 25.0          # resample the recording when the fitted offset exceeds this
DESIGN_REGROWTH_DB = -30.0        # SPEC 5.4 / 7.1: clip-and-filter residue just above the band
CLIP_LEVEL = 0.999


# --- matching ---------------------------------------------------------------


def _fit(nominal: np.ndarray, found: np.ndarray) -> tuple[float, float]:
    """Least squares found = a + b * nominal (b = 1 for no clock offset)."""
    if len(nominal) >= 2 and np.ptp(nominal) > 1.0:
        b, a = np.polyfit(nominal, found, 1)
        return float(a), float(b)
    return float(found[0] - nominal[0]) if len(nominal) else 0.0, 1.0


def match_frames(det_t: np.ndarray, nominal_t: np.ndarray, *, tol_s: float = 0.3,
                 max_ppm: float = 600.0) -> tuple[dict[int, int], float, float]:
    """Assign detections (times, seconds) to manifest frames (nominal times).

    Returns ({detection: frame}, offset, slope). A first search tries every
    (detection, frame) pair as an anchor and keeps the offset that explains
    the most detections within ``tol_s`` plus the drift ``max_ppm`` allows.
    The offset and clock slope are then refined by fitting the matches, and
    each detection goes to the nearest unclaimed frame (earlier frames first).
    """
    if len(det_t) == 0 or len(nominal_t) == 0:
        return {}, 0.0, 1.0
    best = (-1, 0.0)
    for i in range(len(det_t)):
        for j in range(len(nominal_t)):
            o = det_t[i] - nominal_t[j]
            tol = tol_s + max_ppm * 1e-6 * np.abs(nominal_t - nominal_t[j])
            d = np.abs(det_t[:, None] - (o + nominal_t)[None, :])
            score = int(np.sum(np.any(d < tol[None, :], axis=1)))
            if score > best[0]:
                best = (score, o)
    a, b = best[1], 1.0
    pairs: dict[int, int] = {}
    for _ in range(3):
        pairs = {}
        taken: dict[int, float] = {}
        for i in np.argsort(det_t):
            pred = a + b * nominal_t
            j = int(np.argmin(np.abs(pred - det_t[i])))
            if abs(pred[j] - det_t[i]) < tol_s and j not in taken:
                pairs[int(i)] = j
                taken[j] = det_t[i]
        if not pairs:
            break
        di = sorted(pairs)
        a, b = _fit(nominal_t[[pairs[i] for i in di]], det_t[di])
        if not 1 - max_ppm * 1e-6 <= b <= 1 + max_ppm * 1e-6:
            a, b = float(np.median(det_t[di] - nominal_t[[pairs[i] for i in di]])), 1.0
    return pairs, a, b


# --- measurements -----------------------------------------------------------


def _tone_powers(x: np.ndarray, start: float, n_sym: int, width: int, cp: int, n_tones: int,
                 shifts=None) -> np.ndarray:
    return ofdm.demodulate(x, start, n_sym, n_tones, cp, active=width, shifts=shifts)


def _distortion(x: np.ndarray, cfg: frame.FrameConfig, tx: frame.TxFrame, res: frame.RxResult,
                gap: tuple[int, int] | None) -> dict:
    """Distortion indicators (SPEC 7.1, B.2): energy on silent tones and
    amplitude-dependent constellation error. See docs/bench-testing.md."""
    det = res.detail
    first = params.PREAMBLE_SYMBOLS
    d_first = first + cfg.header_symbols + cfg.probe_symbols
    n_sym = cfg.total_symbols
    w = cfg.width
    n_wide = w + 8
    pad = np.concatenate([x, np.zeros(400)])
    shifts = det["shifts"]
    tones = _tone_powers(pad, det["window_start"], n_sym, w, cfg.cp, n_wide, shifts)
    body = tones[d_first:]
    silent = sorted((set(range(n_wide)) - set(cfg.data_active_tones)))
    silent = [k for k in silent if k in cfg.masked or k >= w]
    p_act = float(np.mean(np.abs(body[:, list(cfg.data_active_tones)]) ** 2))
    p_oob = float(np.mean(np.abs(body[:, silent]) ** 2))
    out = {"oob_rel_db": float(10 * np.log10(max(p_oob / p_act, 1e-12))), "noise_rel_db": None,
           "oob_excess_db": None}
    if gap is not None and gap[1] - gap[0] >= 4 * params.N_USEFUL:
        n = min(40, (gap[1] - gap[0]) // params.N_USEFUL)
        g = _tone_powers(x, gap[0], n, w, 0, n_wide)
        p_n = float(np.mean(np.abs(g[:, silent]) ** 2))
        out["noise_rel_db"] = float(10 * np.log10(max(p_n / p_act, 1e-12)))
        out["oob_excess_db"] = float(10 * np.log10(max(p_oob - p_n, 1e-12 * p_act) / p_act))

    # Constellation error against the transmitted points.
    dt = list(cfg.data_tones)
    eq = det["eq"][det["d0"]:, dt]
    ideal = tx.grid[d_first:, dt] / cfg.data_gain
    err = eq - ideal
    out["evm_db"] = float(10 * np.log10(np.mean(np.abs(err) ** 2) / np.mean(np.abs(ideal) ** 2)))
    amp = np.abs(ideal)
    out["compression_db"] = None
    if cfg.rung.bits_per_symbol >= 4:
        lo, hi = np.quantile(amp, [1 / 3, 2 / 3])
        gains = []
        for m in (amp <= lo, amp >= hi):
            gains.append(float(np.real(np.sum(eq[m] * np.conj(ideal[m])) / np.sum(amp[m] ** 2))))
        if gains[0] > 0 and gains[1] > 0:
            out["compression_db"] = float(20 * np.log10(gains[1] / gains[0]))
    # Error in the symbols with the highest time-domain peaks against the lowest.
    # Clip-and-filter makes that ratio positive by design (the clipper cuts the
    # peakiest symbols), so the transmitted waveform itself is measured the same
    # way and the difference is what the channel added.
    def peaky(buf, start, sh, points):
        peaks = []
        for r in range(d_first, n_sym):
            s0 = int(round(start)) + cfg.stride * r + (int(sh[r]) if sh is not None else 0)
            peaks.append(np.max(np.abs(buf[s0:s0 + params.N_USEFUL])))
        peaks = np.array(peaks)
        g = np.sum(points * np.conj(ideal)) / np.sum(amp ** 2)
        e = np.mean(np.abs(points - g * ideal) ** 2, axis=1)
        q1, q3 = np.quantile(peaks, [0.25, 0.75])
        top, bot = e[peaks >= q3], e[peaks <= q1]
        if top.size < 4 or bot.size < 4:
            return None
        return float(10 * np.log10(np.mean(top) / max(np.mean(bot), 1e-30)))

    out["peaky_db"] = peaky(pad, det["window_start"], shifts, eq)
    clean = np.concatenate([tx.samples, np.zeros(400)])
    ref_pts = ofdm.demodulate(clean, cfg.cp, n_sym, w, cfg.cp, active=w)[d_first:, dt] / cfg.data_gain
    ref = peaky(clean, cfg.cp, None, ref_pts)
    out["peaky_design_db"] = ref
    out["peaky_excess_db"] = (out["peaky_db"] - ref) if out["peaky_db"] is not None and ref is not None else None
    # Indicative index: 3 dB steps above what clip-and-filter leaves by design (0 clean, 7 severe).
    steps = []
    ref_oob = out["oob_excess_db"] if out["oob_excess_db"] is not None else out["oob_rel_db"]
    steps.append((ref_oob - DESIGN_REGROWTH_DB) / 3.0)
    if out["compression_db"] is not None:
        steps.append(-out["compression_db"] / 0.5)        # 0.5 dB of compression per step
    if out["peaky_excess_db"] is not None:
        steps.append((out["peaky_excess_db"] - 3.0) / 3.0)  # 3 dB of quartile scatter is noise
    out["index"] = int(np.clip(np.ceil(max(steps) - 0.1), 0, 7))
    return out


def adaptation(snr7: float) -> dict:
    """Rung the SPEC 6.2 thresholds would give for a 7 dB-equivalent SNR.

    ``first_frame`` is the 10.1 rule for the first frame after CONNECT (the
    highest rung whose threshold is 3 dB below the measured SNR);
    ``supported`` is the highest rung whose threshold is at or below it. No
    Stage 2 hardware margin is added to either.
    """
    thr = [r.nominal_snr_db for r in params.RUNGS]
    sup = [m for m, t in enumerate(thr) if t <= snr7]
    ff = [m for m, t in enumerate(thr) if t + 3.0 <= snr7]
    return {"first_frame": ff[-1] if ff else 0, "first_frame_below_mcs0": not ff,
            "supported": sup[-1] if sup else None}


# --- analysis ---------------------------------------------------------------


def _detect(x: np.ndarray, rho: float) -> list[preamble.Detection]:
    return preamble.detect(x, ppm_grid=PPM_GRID, xcorr_threshold=rho)


def _tone_report(x: np.ndarray, manifest: dict, offset: float, slope: float) -> dict | None:
    t = manifest.get("level_tone")
    if not t:
        return None
    a = offset + slope * t["start_s"]
    n = t["duration_s"] * slope
    lo, hi = int((a + 0.15 * n) * FS), int((a + 0.85 * n) * FS)
    if lo < 0 or hi > len(x) or hi - lo < FS // 10:
        return {"found": False}
    seg = x[lo:hi]
    spec = np.abs(np.fft.rfft(seg * np.hanning(len(seg)))) ** 2
    f = np.fft.rfftfreq(len(seg), 1 / FS)
    band = (f > 950) & (f < 1050)
    peak_bin = np.flatnonzero(band)[np.argmax(spec[band])]
    near = np.abs(np.arange(len(spec)) - peak_bin) <= 4
    purity = 10 * np.log10(spec[near].sum() / max(spec[~near].sum(), 1e-30))
    rms = float(np.sqrt(np.mean(seg ** 2)))
    return {"found": bool(purity > 15 and rms > 1e-4), "freq_hz": float(f[peak_bin]),
            "rms_dbfs": audio.db(rms), "peak_dbfs": audio.db(float(np.abs(seg).max())),
            "purity_db": float(purity), "clipped_samples": int(np.sum(np.abs(seg) >= CLIP_LEVEL))}


def analyze(x: np.ndarray, manifest: dict, *, rho: float = preamble.XCORR_THRESHOLD,
            correct_ppm: bool = True, collect: dict | None = None) -> dict:
    """Run the whole pipeline on mono 48 kHz audio ``x``. ``collect`` (a dict), if
    given, receives per-frame ``RxResult`` and ``TxFrame`` for plotting."""
    frames_m = manifest["frames"]
    nominal = np.array([f["start_s"] for f in frames_m])
    dets = _detect(x, rho)
    det_t = np.array([d.start / FS for d in dets])
    pairs, off, slope = match_frames(det_t, nominal)
    ppm_global = None
    corrected = 0.0
    span = float(np.ptp(nominal[list(pairs.values())])) if len(pairs) >= 2 else 0.0
    if len(pairs) >= 2 and span >= 5.0:
        ppm_global = float((1.0 / slope - 1.0) * 1e6)
        if correct_ppm and abs(ppm_global) > CORRECT_PPM_ABOVE:
            # The recording is r[n] = x((1 + eps) n); r(n / (1 + eps)) restores x.
            x = fractional_resample(x, 1.0 / (1.0 + ppm_global * 1e-6))
            corrected = ppm_global
            dets = _detect(x, rho)
            det_t = np.array([d.start / FS for d in dets])
            pairs, off, slope = match_frames(det_t, nominal)

    rows = []
    claimed = {j: i for i, j in pairs.items()}
    for j, fm in enumerate(frames_m):
        row = {"index": fm["index"], "mcs": fm["mcs"], "width": fm["width"], "prefix_expected": fm["prefix"],
               "nominal_s": fm["start_s"], "n_blocks": fm["n_blocks"]}
        if j not in claimed:
            row.update(status="missed")
            rows.append(row)
            continue
        d = dets[claimed[j]]
        cfg = frame.FrameConfig(width=fm["width"], mcs=fm["mcs"], n_blocks=fm["n_blocks"], cp=fm["cp"],
                                mask=(frozenset(range(fm["width"])) - frozenset(fm["silenced"]))
                                if fm["silenced"] else None)
        tx = frame.build(cfg, np.random.default_rng(fm["seed"]))
        s0 = max(0, d.start - int(0.45 * FS))
        end = d.start + cfg.n_samples + 2000
        seg = x[s0:end]
        truncated = end > len(x)
        if truncated:
            seg = np.concatenate([seg, np.zeros(end - len(x))])
        res = frame.receive(seg, cfg, start=d.start - s0)
        prev_end = max((dets[i].start + 1 for i, k in pairs.items() if k < j), default=0)
        for i, k in pairs.items():
            if k == j - 1:
                prev_end = dets[i].start + frames_m[k]["n_samples"]
        gap_lo = max(prev_end + int(0.05 * FS), d.start - int(0.4 * FS))
        gap_hi = d.start - int(0.02 * FS)
        gap = (gap_lo - s0, gap_hi - s0) if gap_hi > gap_lo and gap_lo >= s0 else None
        dist = _distortion(seg, cfg, tx, res, gap)
        # ADR 0006: the channel SNR, rebuilt from the decoded frame (frame.channel_snr).
        snr7 = float(res.detail["snr_db"])
        snr = snr7 - (cfg.clip_db - 7.0)
        blocks_match = [bool(ok and np.array_equal(b, tb))
                        for ok, b, tb in zip(res.block_ok, res.block_bits, tx.block_bits)]
        bit_err = [int(np.sum(b != tb)) for b, tb in zip(res.block_bits, tx.block_bits)]
        frame_seg = x[d.start:d.start + cfg.n_samples]
        drift = res.detail["drift"]
        ppm_resid = float(-drift / cfg.stride * 1e6)
        row.update(
            status="decoded", time_s=d.start / FS, time_error_s=d.start / FS - (off + slope * fm["start_s"]),
            prefix_detected="long" if d.cp == params.CP_LONG else "short",
            prefix_ok=(d.cp == fm["cp"]), detect_rho=d.quality,
            header_ok=bool(res.header_ok),
            header_match=bool(res.header_ok and np.array_equal(res.header_bits, tx.header_bits)),
            blocks_ok=int(np.sum(blocks_match)), blocks_total=cfg.n_blocks,
            blocks_crc_ok=int(np.sum(res.block_ok)), block_ok=blocks_match, block_bit_errors=bit_err,
            snr_db=snr, snr7_db=snr7, snr_per_tone_db=[float(v) for v in res.detail["snr_per_tone_db"]],
            snr_fallback=bool(res.detail["snr_fallback"]),
            snr_known_symbols=int(res.detail["snr_known_symbols"]),
            snr_total_symbols=int(res.detail["snr_total_symbols"]),
            clock_ppm=ppm_resid + corrected, clock_ppm_residual=ppm_resid,
            distortion=dist, adaptation=adaptation(snr7),
            peak_dbfs=audio.db(float(np.abs(frame_seg).max())) if frame_seg.size else None,
            rms_dbfs=audio.db(float(np.sqrt(np.mean(frame_seg ** 2)))) if frame_seg.size else None,
            clipped_samples=int(np.sum(np.abs(frame_seg) >= CLIP_LEVEL)), truncated=bool(truncated))
        rows.append(row)
        if collect is not None:
            collect[fm["index"]] = (cfg, tx, res)

    extra = [{"time_s": dets[i].start / FS, "prefix": "long" if dets[i].cp == params.CP_LONG else "short",
              "rho": dets[i].quality} for i in range(len(dets)) if i not in pairs]
    tone = _tone_report(x, manifest, off, slope) if pairs else None
    ok = [r for r in rows if r["status"] == "decoded"]
    snrs = [r["snr7_db"] for r in ok]
    summary = {
        "frames_sent": len(rows), "detected": len(ok), "missed": [r["index"] for r in rows if r["status"] == "missed"],
        "extra_detections": len(extra), "headers_ok": sum(r["header_ok"] for r in ok),
        "frames_all_blocks_ok": sum(r["blocks_ok"] == r["blocks_total"] and r["header_ok"] for r in ok),
        "blocks_ok": sum(r["blocks_ok"] for r in ok), "blocks_sent": sum(r["n_blocks"] for r in rows),
        "snr7_db": ({"min": min(snrs), "mean": float(np.mean(snrs)), "max": max(snrs)} if snrs else None),
        "snr_fallback_frames": sum(r["snr_fallback"] for r in ok),
        "clock_ppm_global": ppm_global, "recording_resampled_by_ppm": corrected,
        "clock_ppm_frames": ({"min": min(r["clock_ppm"] for r in ok), "max": max(r["clock_ppm"] for r in ok)}
                             if ok else None),
        "time_offset_s": off if pairs else None,
        "input_clipped": any(r["clipped_samples"] > 0 for r in ok) or bool(tone and tone.get("clipped_samples")),
    }
    return {"summary": summary, "level_tone": tone, "frames": rows, "extra": extra}


def recording_info(fs_in: int, n_ch: int, channel_name: str, x: np.ndarray) -> dict:
    return {"sample_rate_in": fs_in, "channels": n_ch, "channel_used": channel_name,
            "duration_s": len(x) / FS, "peak_dbfs": audio.db(float(np.abs(x).max())) if len(x) else None,
            "rms_dbfs": audio.db(float(np.sqrt(np.mean(x ** 2)))) if len(x) else None}


def load(path: str, channel: str = "auto") -> tuple[np.ndarray, dict]:
    fs, data = audio.read_wav(path)
    mono, name = audio.to_mono(data, channel)
    x = audio.resample_to_fs(mono, fs, FS)
    return x, recording_info(fs, data.shape[1], name, x)


# --- text report ------------------------------------------------------------


def format_report(rep: dict) -> str:
    L = []
    rec = rep["recording"]
    L.append(f"Recording: {rec['duration_s']:.1f} s, {rec['sample_rate_in']} Hz in, {rec['channels']} channel(s) "
             f"[{rec['channel_used']}], peak {rec['peak_dbfs']:.1f} dBFS, rms {rec['rms_dbfs']:.1f} dBFS")
    t = rep["level_tone"]
    if t is not None:
        if t.get("found"):
            L.append(f"Level tone: {t['freq_hz']:.0f} Hz, peak {t['peak_dbfs']:.1f} dBFS, rms {t['rms_dbfs']:.1f} dBFS, "
                     f"purity {t['purity_db']:.0f} dB" + (f", CLIPPED ({t['clipped_samples']} samples)"
                                                          if t["clipped_samples"] else ""))
        else:
            L.append("Level tone: not found at the expected time")
    L.append("")
    L.append(" #  MCS  tones  prefix   time(s)  hdr  blocks  SNR7(dB)   clock(ppm)  EVM(dB)  dist  pick  note")
    for r in rep["frames"]:
        if r["status"] == "missed":
            L.append(f"{r['index']:2d}  {r['mcs']:3d}  {r['width']:5d}  {r['prefix_expected']:6s}  "
                     f"{'-':>7}  MISSED: no preamble detected near {r['nominal_s']:.2f} s (nominal)")
            continue
        d = r["distortion"]
        notes = []
        if not r["prefix_ok"]:
            notes.append(f"prefix detected {r['prefix_detected']}, sent {r['prefix_expected']}")
        if r["header_ok"] and not r["header_match"]:
            notes.append("header wrong")
        if r["blocks_crc_ok"] != r["blocks_ok"]:
            notes.append("CRC passed on wrong payload")
        if r["clipped_samples"]:
            notes.append("input clipped")
        if r["truncated"]:
            notes.append("recording ends inside frame")
        if r["snr_fallback"]:
            notes.append("SNR counts the designed clipping (too few known symbols)")
        if r["adaptation"]["first_frame_below_mcs0"]:
            notes.append("below MCS 0 + 3 dB")
        L.append(f"{r['index']:2d}  {r['mcs']:3d}  {r['width']:5d}  {r['prefix_detected']:6s}  {r['time_s']:7.2f}  "
                 f"{'ok' if r['header_ok'] else 'FAIL':>3}  {r['blocks_ok']:3d}/{r['blocks_total']:<3d}  "
                 f"{r['snr7_db']:8.1f}{'*' if r['snr_fallback'] else ' '} {r['clock_ppm']:10.0f}  {d['evm_db']:7.1f}  {d['index']:4d}  "
                 f"{r['adaptation']['first_frame']:4d}  {'; '.join(notes)}")
    L.append("")
    L.append("Distortion detail (indicative): out-of-band = energy on silent tones relative to active tones; "
             "compression = outer minus inner constellation gain (16-QAM and up); peaky = error in the highest-peak "
             "quartile of symbols over the lowest.")
    for r in rep["frames"]:
        if r["status"] != "decoded":
            continue
        d = r["distortion"]
        f = lambda v, u: "n/a" if v is None else f"{v:.1f} {u}"
        L.append(f"  frame {r['index']:2d}: out-of-band {d['oob_rel_db']:.1f} dB (noise floor {f(d['noise_rel_db'], 'dB')}, "
                 f"excess {f(d['oob_excess_db'], 'dB')}), compression {f(d['compression_db'], 'dB')}, "
                 f"peaky {f(d['peaky_db'], 'dB')} (design {f(d['peaky_design_db'], 'dB')}), level {r['peak_dbfs']:.1f} dBFS peak, "
                 f"rung picks: first-frame MCS {r['adaptation']['first_frame']}, "
                 f"highest supported {r['adaptation']['supported'] if r['adaptation']['supported'] is not None else 'none'}")
    s = rep["summary"]
    L.append("")
    L.append("Summary")
    L.append(f"  frames sent {s['frames_sent']}, detected {s['detected']}, missed {len(s['missed'])}"
             + (f" ({', '.join(map(str, s['missed']))})" if s["missed"] else "")
             + f", extra detections {s['extra_detections']}")
    L.append(f"  headers decoded {s['headers_ok']}/{s['detected']}, frames with every block OK "
             f"{s['frames_all_blocks_ok']}/{s['frames_sent']}, blocks OK {s['blocks_ok']}/{s['blocks_sent']} "
             f"(missed frames count as lost)")
    if s["snr7_db"]:
        v = s["snr7_db"]
        L.append(f"  SNR (7 dB-equivalent channel SNR): min {v['min']:.1f}, mean {v['mean']:.1f}, max {v['max']:.1f} dB"
                 + (f"; {s['snr_fallback_frames']} frame(s) marked * used the fallback that counts the designed "
                    f"clipping (reads low)" if s["snr_fallback_frames"] else ""))
    if s["clock_ppm_global"] is not None:
        L.append(f"  sound-card clock offset: {s['clock_ppm_global']:+.1f} ppm from frame timing"
                 + (f" (recording resampled to remove it)" if s["recording_resampled_by_ppm"] else ""))
    elif s["clock_ppm_frames"]:
        L.append(f"  clock offset per frame (pilots): {s['clock_ppm_frames']['min']:.0f} to {s['clock_ppm_frames']['max']:.0f} ppm")
    if s["input_clipped"]:
        L.append("  WARNING: recorded audio reaches full scale: lower the receive level")
    L.append("  Rung picks use the SPEC 6.2 thresholds only (first-frame rule of 10.1, 3 dB below the measured "
             "SNR); no Stage 2 hardware margin is applied.")
    return "\n".join(L)


def to_json(rep: dict) -> str:
    return json.dumps(rep, indent=1)


# --- plots ------------------------------------------------------------------


def plots(rep: dict, collect: dict, directory: str, constellation_frame: int | None = None) -> list[str]:
    import os

    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    os.makedirs(directory, exist_ok=True)
    out = []
    ok = [r for r in rep["frames"] if r["status"] == "decoded"]
    if ok:
        fig, ax = plt.subplots(figsize=(8, 4.5))
        for r in ok:
            tones = np.arange(len(r["snr_per_tone_db"]))
            ax.plot(params.FIRST_TONE_HZ + params.TONE_SPACING * tones,
                    np.array(r["snr_per_tone_db"]),
                    lw=1, label=f"#{r['index']} MCS {r['mcs']} x{r['width']}")
        ax.set_xlabel("tone frequency (Hz)")
        ax.set_ylabel("per-tone SNR, 7 dB-equivalent (dB)")
        ax.grid(alpha=0.3)
        if len(ok) <= 12:
            ax.legend(fontsize=7, ncol=2)
        fig.tight_layout()
        p = os.path.join(directory, "snr_per_tone.png")
        fig.savefig(p, dpi=120)
        plt.close(fig)
        out.append(p)
        pick = constellation_frame
        if pick is None:
            pick = max(ok, key=lambda r: (r["mcs"], r["blocks_ok"]))["index"]
        if pick in collect:
            cfg, tx, res = collect[pick]
            d = res.detail
            dt = list(cfg.data_tones)
            eq = d["eq"][d["d0"]:, dt].ravel()
            fig, ax = plt.subplots(figsize=(5.5, 5.5))
            ax.plot(eq.real, eq.imag, ".", ms=1.5, alpha=0.4)
            ideal = (tx.grid[params.PREAMBLE_SYMBOLS + cfg.header_symbols + cfg.probe_symbols:, dt]
                     / cfg.data_gain).ravel()
            ax.plot(np.unique(np.round(ideal, 6)).real, np.unique(np.round(ideal, 6)).imag, "r+", ms=6)
            ax.set_aspect("equal")
            ax.set_title(f"frame {pick}: MCS {cfg.mcs}, {cfg.width} tones (equalized)")
            ax.grid(alpha=0.3)
            fig.tight_layout()
            p = os.path.join(directory, f"constellation_{pick}.png")
            fig.savefig(p, dpi=120)
            plt.close(fig)
            out.append(p)
    return out
