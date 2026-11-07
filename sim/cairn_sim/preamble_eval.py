"""Preamble detector measurements against SPEC 21.2 (``preamble`` row).

Targets: >= 99% detection at 6 dB audio SNR; <= 1 false alarm per 10 minutes
of open-squelch noise; prefix identified correctly >= 99.9%.

- ``detection_run``: a stream of frames separated by noise-only gaps, AWGN
  at a set audio SNR (``channel.awgn``, in-band definition), resampled for a
  sample-clock offset. Reports detection probability, prefix identification,
  timing error and false alarms in the gaps.
- ``false_alarm_run``: open-squelch noise (white, band-limited to the
  receive band, or f^2-tilted discriminator noise), optionally with a steady
  interferer mixed in: a 1000 Hz courtesy beep, a 1750 Hz tone burst, DTMF
  digits or CW keying.
- ``decode_run``: detect, then ``frame.receive(start=det.start)`` on whole
  frames, to check the start is good enough to decode.

``python -m cairn_sim.preamble_eval`` prints the full report.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass, field
from functools import cache

import numpy as np
from scipy import interpolate, signal

from . import channel, frame, params, preamble

FS = params.FS
RX_BAND_HZ = (300.0, 3000.0)      # receive audio band of a typical FM radio
BURST_SYMBOLS = 15                # preamble + header + 4 data symbols per test burst


# --- signal helpers ---------------------------------------------------------------


@cache
def _bursts(cp: int, n_variants: int = 4) -> tuple[np.ndarray, ...]:
    """A few transmitted bursts (preamble, header, first data symbols)."""
    rng = np.random.default_rng(1000 + cp)
    cfg = frame.FrameConfig(cp=cp, n_blocks=1)
    out = []
    for _ in range(n_variants):
        x = frame.build(cfg, rng).samples[: BURST_SYMBOLS * cfg.stride].copy()
        x.setflags(write=False)
        out.append(x)
    return tuple(out)


@cache
def _spline(cp: int, variant: int) -> interpolate.CubicSpline:
    x = _bursts(cp)[variant]
    pad = 16
    xp = np.concatenate([np.zeros(pad), x, np.zeros(pad)])
    return interpolate.CubicSpline(np.arange(-pad, x.size + pad), xp, extrapolate=False)


def place(stream: np.ndarray, spline: interpolate.CubicSpline, length: int,
          t0: float, ppm: float) -> None:
    """Add a burst that starts at (fractional) sample t0 of ``stream``.

    Sample n of the stream sees the burst at its time (n - t0)(1 + ppm 1e-6):
    the receiver's clock runs at (1 + ppm 1e-6) times the transmitter's.
    Cubic-spline interpolation of 16x oversampled audio is accurate to about
    -88 dB, far below any SNR of interest.
    """
    scale = 1 + ppm * 1e-6
    lo = int(np.floor(t0))
    hi = int(np.ceil(t0 + length / scale)) + 1
    n = np.arange(max(lo, 0), min(hi, stream.size))
    tau = (n - t0) * scale
    v = spline(tau)
    stream[n] += np.nan_to_num(v)


def resample_ppm(x: np.ndarray, ppm: float) -> np.ndarray:
    """y[n] = x(n (1 + ppm 1e-6)) by cubic-spline interpolation."""
    scale = 1 + ppm * 1e-6
    n = np.arange(int(np.floor((x.size - 1) / scale)) + 1)
    return interpolate.CubicSpline(np.arange(x.size), x)(n * scale)


@cache
def _repeater_sos() -> tuple[np.ndarray, np.ndarray]:
    radio = signal.butter(4, RX_BAND_HZ, btype="bandpass", fs=FS, output="sos")
    controller = signal.butter(4, (300.0, 2700.0), btype="bandpass", fs=FS, output="sos")
    return radio, controller


def repeater_chain(x: np.ndarray) -> np.ndarray:
    """A stand-in for the dispersive repeater audio path until ``channel``
    models it: two radio band-passes (300-3000 Hz, 8th-order Butterworth)
    and a controller band-pass (300-2700 Hz). A harsh case: group delay is
    5.7 ms at 350 Hz, 2.4 ms at 500 Hz and 0.7-0.9 ms from 1 to 2.65 kHz,
    so the lowest tones spread past even the long prefix."""
    radio, controller = _repeater_sos()
    return signal.sosfilt(controller, signal.sosfilt(radio, signal.sosfilt(radio, x)))


# --- detection --------------------------------------------------------------------


@dataclass
class DetectionResult:
    snr_db: float
    cp: int
    ppm: float
    n_frames: int
    detected: int
    prefix_correct: int
    false_alarms: int
    audio_s: float
    timing_err: np.ndarray = field(repr=False)   # detected start - true start, samples
    run_s: float = 0.0

    @property
    def p_detect(self) -> float:
        return self.detected / self.n_frames

    @property
    def prefix_rate(self) -> float:
        return self.prefix_correct / max(self.detected, 1)

    @property
    def max_abs_timing_err(self) -> float:
        return float(np.max(np.abs(self.timing_err))) if self.timing_err.size else float("nan")

    def summary(self) -> str:
        te = self.timing_err
        rms = float(np.sqrt(np.mean(te ** 2))) if te.size else float("nan")
        return (f"SNR {self.snr_db:5.1f} dB cp {self.cp:3d} {self.ppm:+6.0f} ppm: "
                f"Pd {self.p_detect:7.2%} ({self.detected}/{self.n_frames}), "
                f"prefix {self.prefix_rate:7.3%}, timing rms {rms:.2f} max {self.max_abs_timing_err:.1f}, "
                f"false alarms {self.false_alarms} in {self.audio_s:.0f} s")


def detection_run(snr_db: float, cp: int, ppm: float, n_frames: int, rng: np.random.Generator,
                  gap_s: tuple[float, float] = (0.05, 0.15), frames_per_segment: int = 200,
                  audio_filter: Callable[[np.ndarray], np.ndarray] | None = None,
                  **detector_kwargs) -> DetectionResult:
    """Frames at random fractional positions in AWGN; count detections.

    A frame counts as detected when a detection starts within one symbol of
    its true start; other detections are false alarms. Prefix and timing
    statistics are over detected frames. ``audio_filter`` (e.g.
    ``repeater_chain``) is applied to the signal before the noise; the SNR
    is then relative to the filtered signal's power, and timing errors
    include the filter's delay.
    """
    t_start = time.perf_counter()
    det = preamble.Detector(**detector_kwargs)
    variants = len(_bursts(cp))
    length = _bursts(cp)[0].size
    filt = audio_filter or (lambda v: v)
    p_sig = float(np.mean([np.mean(filt(b) ** 2) for b in _bursts(cp)]))
    stride = params.N_USEFUL + cp
    truths: list[float] = []
    dets: list[preamble.Detection] = []
    offset = 0                      # absolute index of the current segment
    remaining = n_frames
    while remaining > 0:
        k = min(frames_per_segment, remaining)
        remaining -= k
        gaps = rng.uniform(*gap_s, k) * FS
        starts = np.cumsum(gaps + length) - length + rng.uniform(0, 1, k)
        seg = np.zeros(int(starts[-1] + length + gap_s[1] * FS))
        for t0 in starts:
            place(seg, _spline(cp, int(rng.integers(variants))), length, t0, ppm)
        seg = channel.awgn(filt(seg), snr_db, params.BASE_TONES, rng, signal_power=p_sig)
        truths.extend(offset + starts)
        dets.extend(det.process(seg))
        offset += seg.size
    dets.extend(det.flush())

    truth = np.array(truths)
    detected = np.zeros(truth.size, dtype=bool)
    prefix_ok = 0
    errs = []
    false_alarms = 0
    for d in dets:
        i = int(np.argmin(np.abs(truth - d.start)))
        err = d.start - truth[i]
        if abs(err) <= stride and not detected[i]:
            detected[i] = True
            prefix_ok += d.cp == cp
            errs.append(err)
        else:
            false_alarms += 1
    return DetectionResult(snr_db, cp, ppm, truth.size, int(detected.sum()), prefix_ok,
                           false_alarms, offset / FS, np.array(errs),
                           time.perf_counter() - t_start)


def detection_sweep(snrs=range(0, 11), cps=(params.CP_SHORT, params.CP_LONG),
                    ppms=(-100.0, 0.0, 100.0), n_frames: int = 200, seed: int = 1,
                    **detector_kwargs) -> list[DetectionResult]:
    rng = np.random.default_rng(seed)
    return [detection_run(float(s), cp, ppm, n_frames, rng, **detector_kwargs)
            for s in snrs for cp in cps for ppm in ppms]


# --- decoding from the detected start -----------------------------------------------


@dataclass
class DecodeResult:
    snr_db: float
    cp: int
    ppm: float
    n_frames: int
    detected: int
    header_ok: int
    blocks_ok: int


def decode_run(snr_db: float, cp: int, ppm: float, n_frames: int, rng: np.random.Generator,
               mcs: int = 0, n_blocks: int = 1) -> DecodeResult:
    """Whole frames, one per capture: detect, then decode from det.start."""
    cfg = frame.FrameConfig(cp=cp, mcs=mcs, n_blocks=n_blocks)
    detected = header_ok = blocks_ok = 0
    for _ in range(n_frames):
        tx = frame.build(cfg, rng)
        lead = 2000 + rng.uniform(0, 1000)
        cap = np.zeros(int(lead + tx.samples.size / (1 + ppm * 1e-6)) + 3000)
        pad = 16
        xp = np.concatenate([np.zeros(pad), tx.samples, np.zeros(pad)])
        spl = interpolate.CubicSpline(np.arange(-pad, tx.samples.size + pad), xp, extrapolate=False)
        place(cap, spl, tx.samples.size, lead, ppm)
        rx = channel.awgn(cap, snr_db, params.BASE_TONES, rng,
                          signal_power=float(np.mean(tx.samples ** 2)))
        dets = [d for d in preamble.detect(rx) if abs(d.start - lead) <= cfg.stride]
        if not dets:
            continue
        detected += 1
        r = frame.receive(rx, cfg, start=dets[0].start, measure_snr=False)
        header_ok += r.header_ok
        blocks_ok += bool(r.block_ok.all())
    return DecodeResult(snr_db, cp, ppm, n_frames, detected, header_ok, blocks_ok)


# --- open-squelch noise and interferers --------------------------------------------


class NoiseSource:
    """Open-squelch receiver noise, generated in chunks with filter state.

    ``white``: white Gaussian noise through an 8th-order Butterworth band-pass
    over the receive band (300-3000 Hz). ``tilted``: the same after a first
    difference, whose |H|^2 = 4 sin^2(pi f / fs) ~ f^2 is the discriminator
    noise of a flat (unde-emphasized) port. Unit power at the output.
    """

    def __init__(self, kind: str, rng: np.random.Generator) -> None:
        if kind not in ("white", "tilted"):
            raise ValueError(kind)
        self.kind, self.rng = kind, rng
        self.sos = signal.butter(4, RX_BAND_HZ, btype="bandpass", fs=FS, output="sos")
        self.zi = np.zeros((self.sos.shape[0], 2))
        self.prev = 0.0
        self.gain = 1.0
        self.gain = 1.0 / np.sqrt(np.mean(self._raw(FS * 4) ** 2))
        self.zi[:] = 0
        self.prev = 0.0

    def _raw(self, n: int) -> np.ndarray:
        w = self.rng.standard_normal(n)
        if self.kind == "tilted":
            d = np.empty(n)
            d[0] = w[0] - self.prev
            d[1:] = np.diff(w)
            self.prev = w[-1]
            w = d
        y, self.zi = signal.sosfilt(self.sos, w, zi=self.zi)
        return y * self.gain

    def __call__(self, n: int) -> np.ndarray:
        return self._raw(n)


def _ramp_envelope(on: np.ndarray, ramp_ms: float) -> np.ndarray:
    if ramp_ms <= 0:
        return on.astype(float)
    n = max(int(ramp_ms * 1e-3 * FS), 1)
    win = np.hanning(2 * n + 1)
    win /= win.sum()
    return np.convolve(np.concatenate([on, on[:2 * n]]).astype(float), win, mode="same")[: on.size]


MORSE = {"D": "-..", "E": ".", "C": "-.-.", "A": ".-", "I": "..", "R": ".-.", "N": "-."}
DTMF_ROWS = (697.0, 770.0, 852.0, 941.0)
DTMF_COLS = (1209.0, 1336.0, 1477.0, 1633.0)


@cache
def _interferer_period(name: str) -> np.ndarray:
    """One period of a steady interferer at unit RMS while on (48 kHz)."""
    if name == "tone1000":            # repeater courtesy beep: 250 ms on, 750 ms off
        n = np.arange(FS)
        on = n < FS // 4
        return np.sqrt(2) * np.sin(2 * np.pi * 1000 * n / FS) * _ramp_envelope(on, 5)
    if name == "tone1750":            # 1750 Hz tone burst, continuous
        n = np.arange(FS)            # 1750 Hz completes 1750 cycles in 1 s
        return np.sqrt(2) * np.sin(2 * np.pi * 1750 * n / FS)
    if name == "dtmf":                # 16 digits, 100 ms on, 100 ms off
        seg = int(0.1 * FS)
        parts = []
        for r in DTMF_ROWS:
            for c in DTMF_COLS:
                n = np.arange(seg)
                tone = np.sin(2 * np.pi * r * n / FS) + np.sin(2 * np.pi * c * n / FS)
                parts += [tone, np.zeros(seg)]
        on = np.concatenate(parts)
        env = _ramp_envelope(np.tile(np.r_[np.ones(seg, bool), np.zeros(seg, bool)], 16), 2)
        return on * env
    if name == "cw":                  # "DE CAIRN" at 20 wpm, 800 Hz, hard keying
        dot = int(1.2 / 20 * FS)
        keys: list[bool] = []
        for word in ("DE", "CAIRN"):
            for ch in word:
                for el in MORSE[ch]:
                    keys += [True] * (1 if el == "." else 3) + [False]
                keys += [False] * 2
            keys += [False] * 4
        on = np.repeat(np.array(keys), dot)
        n = np.arange(on.size)
        return np.sqrt(2) * np.sin(2 * np.pi * 800 * n / FS) * on
    raise ValueError(name)


INTERFERERS = ("tone1000", "tone1750", "dtmf", "cw")


def interferer(name: str, start: int, n: int) -> np.ndarray:
    period = _interferer_period(name)
    return period[(start + np.arange(n)) % period.size]


@dataclass
class FalseAlarmResult:
    noise: str | None
    interferer: str | None
    tnr_db: float | None      # interferer power (while on) over noise power
    minutes: float
    false_alarms: int
    max_rho: float            # largest S1 correlation seen, gate or not
    max_score: float          # largest S1 correlation where the autocorrelation gate passed
    run_s: float

    @property
    def per_10min(self) -> float:
        return self.false_alarms / self.minutes * 10

    def summary(self) -> str:
        what = self.noise or "no noise"
        if self.interferer:
            what += f" + {self.interferer}" + (f" at {self.tnr_db:+.0f} dB" if self.tnr_db is not None else "")
        return (f"{what:32s}: {self.false_alarms} false alarms in {self.minutes:.1f} min "
                f"({self.per_10min:.2f} / 10 min); max rho {self.max_rho:.3f}, "
                f"max gated rho {self.max_score:.3f}; {self.run_s:.1f} s")


def false_alarm_run(minutes: float, noise: str | None = "white", interferer_name: str | None = None,
                    tnr_db: float | None = None, seed: int = 0, chunk_s: float = 30.0,
                    **detector_kwargs) -> FalseAlarmResult:
    """Count detections on signal-free audio.

    ``noise`` is "white", "tilted" or None (interferer alone). ``tnr_db`` is
    the interferer's power while on relative to the noise power (ignored
    without noise).
    """
    t0 = time.perf_counter()
    rng = np.random.default_rng(seed)
    src = NoiseSource(noise, rng) if noise else None
    amp = 1.0 if (tnr_db is None or src is None) else 10 ** (tnr_db / 20)
    det = preamble.Detector(**detector_kwargs)
    total = int(minutes * 60 * FS)
    chunk = int(chunk_s * FS)
    count = 0
    pos = 0
    while pos < total:
        n = min(chunk, total - pos)
        x = src(n) if src is not None else np.zeros(n)
        if interferer_name:
            x = x + amp * interferer(interferer_name, pos, n)
        count += len(det.process(x))
        pos += n
    count += len(det.flush())
    return FalseAlarmResult(noise, interferer_name, tnr_db if src else None, minutes, count,
                            det.max_rho, det.max_score, time.perf_counter() - t0)


# --- report -------------------------------------------------------------------------


def report(n_frames: int = 1000, fa_minutes: float = 10.0, tone_minutes: float = 2.0) -> None:
    """Print every measurement against the SPEC 21.2 targets (~10 minutes)."""
    t0 = time.perf_counter()
    print("S1 phase units (x 2 pi / 256):", preamble.S1_PHASE_UNITS)
    print("S2 c_init:", preamble.S2_C_INIT)
    for name, v in (("S1", preamble.s1()), ("S2", preamble.s2())):
        print(f"{name} PAPR: short {preamble.symbol_papr_db(v, params.CP_SHORT):.2f} dB, "
              f"long {preamble.symbol_papr_db(v, params.CP_LONG):.2f} dB")
    print(f"S1/S2 cross-correlation peak {preamble.cross_correlation_peak(preamble.s1(), preamble.s2()):.3f}")
    for cp in (params.CP_SHORT, params.CP_LONG):
        a, b = preamble._s1_reference(cp, 0.0), preamble._s1_reference(cp, 100.0)
        loss = -20 * np.log10(abs(np.vdot(a, b)) / np.linalg.norm(a) / np.linalg.norm(b))
        print(f"S1 reference decorrelation at 100 ppm, cp {cp}: {loss:.5f} dB")
    print(f"Thresholds: rho >= {preamble.XCORR_THRESHOLD}, M >= {preamble.AUTOCORR_THRESHOLD}, "
          f"ppm grid {preamble.PPM_GRID}")

    print("\nDetection vs SNR, AWGN (target >= 99% at 6 dB, prefix >= 99.9%):")
    for r in detection_sweep(n_frames=max(n_frames // 5, 50)):
        print(" ", r.summary())
    print(f"\nAt 6 dB, {n_frames} frames per case:")
    rng = np.random.default_rng(6)
    for cp in (params.CP_SHORT, params.CP_LONG):
        for ppm in (-100.0, 0.0, 100.0):
            r = detection_run(6.0, cp, ppm, n_frames, rng)
            print(" ", r.summary(), f"[{r.run_s:.1f} s]")
    print("\nThrough repeater_chain (dispersive stand-in):")
    for snr in (2.0, 4.0, 6.0):
        for cp in (params.CP_SHORT, params.CP_LONG):
            r = detection_run(snr, cp, 100.0, max(n_frames // 5, 50), rng, audio_filter=repeater_chain)
            print(" ", r.summary())

    print("\nFalse alarms (target <= 1 per 10 min):")
    for i, noise in enumerate(("white", "tilted")):
        r = false_alarm_run(fa_minutes, noise, seed=11 + i)
        print(" ", r.summary(), f"[{r.run_s / fa_minutes:.2f} s per audio minute]")
    print("Steady interferers:")
    seed = 100
    for name in INTERFERERS:
        print(" ", false_alarm_run(tone_minutes, None, name).summary())
        for noise in ("white", "tilted"):
            for tnr in (-3.0, 0.0, 3.0, 6.0, 10.0, 30.0):
                seed += 1
                print(" ", false_alarm_run(tone_minutes, noise, name, tnr, seed=seed).summary())

    print("\nDecoding from the detected start (MCS 0, 1 block, +100 ppm):")
    for snr in (6.0, 8.0, 10.0):
        for cp in (params.CP_SHORT, params.CP_LONG):
            d = decode_run(snr, cp, 100.0, 20, rng)
            print(f"  SNR {snr:.0f} dB cp {cp}: detected {d.detected}/{d.n_frames}, "
                  f"header ok {d.header_ok}, block ok {d.blocks_ok}")
    print(f"\nTotal {time.perf_counter() - t0:.0f} s")


if __name__ == "__main__":
    report()
