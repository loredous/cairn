"""Preamble sequences S1 and S2 and the preamble detector (SPEC 6.1, 7.2, 20.3).

Frame order: S1, S1, S2, S2, each a full symbol on the 24 base tones with
its own prefix (60 samples, or 120 in long-prefix/repeater mode). The repeat
makes the signal periodic at lag N + cp (540 or 600 samples) over the first
two symbols, which is what the autocorrelation detector looks for, and the
lag that peaks identifies the prefix.

S1 (Stage 1 choice, published as a test vector)
-----------------------------------------------
Constant-amplitude values s1[k] = exp(j 2 pi u[k] / 256), k = 0..23, with the
integers u[k] in ``S1_PHASE_UNITS``. They were chosen to minimize the PAPR of
the *real* transmitted symbol (prefix plus useful part, sampled at 48 kHz,
before clip-and-filter), taking the worse of the short-prefix window
(n = -60..479) and the long-prefix window (n = -120..479), where the symbol
is x[n] = Re sum_k s1[k] exp(j 2 pi (3.5 + k) n / 480) as ``ofdm.modulate``
builds it. ``search_s1`` is the search (run on aarch64 with NumPy 2.5.4 and
SciPy 1.18.1, it returned exactly these values):

1. Start points: Newman phases pi k^2 / 24, then ``S1_SEARCH_RESTARTS``
   uniformly random phase vectors from ``numpy.random.default_rng(S1_SEARCH_SEED)``.
2. From each start, L-BFGS on a smooth surrogate of the worst-window PAPR,
   (1/q) log(mean_short(y^q) + mean_long(y^q)) with y = x^2 / mean(x^2) over
   the window, for q = 4, 16, 64 in turn.
3. The best result is quantized to multiples of 2 pi / 256 and polished by
   coordinate descent on the integer grid (each u[k] moved by +-1..+-8 units,
   accepted if the exact worst-window PAPR drops), until no move helps.

No normalization follows: a common phase rotation or a linear phase (time
shift) changes the real symbol over the window, so both are part of the
search. Result: 2.75 dB (short prefix) and 2.80 dB (long prefix) at 48 kHz;
2.84 / 2.88 dB evaluated 4x oversampled (the reconstructed analog peak).
Newman phases alone give 4.95 dB, 2.96 dB after the descent. The search takes
about 80 s single-threaded; run it with OMP_NUM_THREADS=1 (multithreaded
BLAS on these small matrices is many times slower). ``S1_PHASE_UNITS`` is
normative; a re-run on another BLAS may land on a different optimum of
similar PAPR.

S2 (Stage 1 choice, published as a test vector)
-----------------------------------------------
BPSK values s2[k] = 1 - 2 c(k), k = 0..23, where c(n) is the TS 38.211 5.2.1
length-31 Gold sequence (Nc = 1600) with c_init = ``S2_C_INIT``, as
``mapping.gold_sequence`` computes it. ``search_s2`` reproduces the choice:
over every c_init in 0..65535 it computes the worst-window PAPR (as for S1)
and the peak normalized cross-correlation with S1 over all time shifts,
max_tau |sum_k conj(s1[k]) s2[k] exp(j 2 pi (3.5 + k) tau / 480)| / 24, and
takes the c_init with the lowest PAPR among those whose cross-correlation
peak is at most ``S2_MAX_XCORR`` = 0.40 (ties to the lowest cross-correlation,
then the lowest c_init). Result: c_init = 5237, PAPR 5.07 dB short prefix,
5.03 dB long,
cross-correlation peak 0.374 (0.126 at zero shift). Over all 65,536 segments
the PAPR ranges from 4.87 dB and the cross-correlation peak from 0.296, but
not together: the lowest-PAPR segment has a 0.43 peak, and the bound 0.30
admits a single segment at 6.9 dB. S2 is clip-and-filtered at 7 dB like any
symbol, and the detector only needs S2 well below its 0.55 S1 threshold, so
PAPR was favoured once the cross-correlation was under 0.4.

Detector
--------
``detect`` implements the SPEC 6.1 detector; see ``Detector``.
"""

from __future__ import annotations

from dataclasses import dataclass
from functools import cache

import numpy as np
from scipy import ndimage, optimize, signal

from . import mapping, params

N = params.N_USEFUL
K = params.BASE_TONES
PHASE_DENOM = 256

# --- chosen sequences -----------------------------------------------------------

S1_SEARCH_SEED = 20261106
S1_SEARCH_RESTARTS = 400
S1_PHASE_UNITS: tuple[int, ...] = (
    181, 160, 172, 183, 202, 203, 204, 79, 7, 184, 91, 108,
    195, 191, 29, 125, 88, 203, 26, 202, 223, 107, 168, 82,
)
S2_C_INIT = 5237
S2_MAX_XCORR = 0.40


def newman_phases(n: int = K) -> np.ndarray:
    k = np.arange(n)
    return np.pi * k ** 2 / n


def s1(n: int = K) -> np.ndarray:
    if n != K:
        raise ValueError(f"S1 is defined on the {K} base tones")
    return np.exp(2j * np.pi * np.array(S1_PHASE_UNITS) / PHASE_DENOM)


def s2(n: int = K) -> np.ndarray:
    if n != K:
        raise ValueError(f"S2 is defined on the {K} base tones")
    return 1.0 - 2.0 * mapping.gold_sequence(S2_C_INIT, n).astype(float)


def grid(width: int) -> np.ndarray:
    """The four preamble symbols as tone values (4, width); base tones only."""
    g = np.zeros((params.PREAMBLE_SYMBOLS, width), dtype=complex)
    g[0, :K] = g[1, :K] = s1()
    g[2, :K] = g[3, :K] = s2()
    return g


# --- symbol waveforms and PAPR ----------------------------------------------------


def tone_waveform(values: np.ndarray, t: np.ndarray) -> np.ndarray:
    """Analytic multisine sum_k values[k] exp(j 2 pi (3.5 + k) t / 480).

    ``t`` is in 48 kHz samples relative to the start of the useful part and
    may be fractional or negative; over t = -cp .. 479 the real part is the
    transmitted symbol (up to ``ofdm.modulate``'s scale sqrt(2 / active)).
    """
    f = params.BIN_OFFSET + np.arange(len(values))
    return np.exp(2j * np.pi * np.multiply.outer(np.asarray(t, float), f) / N) @ np.asarray(values)


def symbol_papr_db(values: np.ndarray, cp: int) -> float:
    """PAPR of the real symbol (prefix plus useful part, 48 kHz) in dB."""
    x = tone_waveform(values, np.arange(-cp, N)).real
    return float(10 * np.log10(np.max(x ** 2) / np.mean(x ** 2)))


def worst_papr_db(values: np.ndarray) -> float:
    return max(symbol_papr_db(values, params.CP_SHORT), symbol_papr_db(values, params.CP_LONG))


def cross_correlation_peak(a: np.ndarray, b: np.ndarray) -> float:
    """max over shifts of |sum conj(a) b exp(j 2 pi (3.5 + k) tau / 480)| / n.

    The tones sit on half bins, so the magnitude is periodic in tau with
    period 480; 8x oversampling of the shift is ample for a 2.4 kHz band.
    """
    tau = np.arange(0, N, 0.125)
    return float(np.max(np.abs(tone_waveform(np.conj(a) * b, tau))) / len(a))


# --- S1 search --------------------------------------------------------------------


class _PaprModel:
    """Real symbol over the long window with the short window as a subset."""

    def __init__(self) -> None:
        n = np.arange(-params.CP_LONG, N)
        th = 2 * np.pi * np.outer(n, params.BIN_OFFSET + np.arange(K)) / N
        self.c, self.s = np.cos(th), np.sin(th)
        self.windows = (n >= -params.CP_SHORT, np.ones(n.size, dtype=bool))

    def worst_db(self, phi: np.ndarray) -> float:
        x = self.c @ np.cos(phi) - self.s @ np.sin(phi)
        return max(float(10 * np.log10(np.max(x[w] ** 2) / np.mean(x[w] ** 2))) for w in self.windows)

    def surrogate(self, phi: np.ndarray, q: float) -> tuple[float, np.ndarray]:
        cphi, sphi = np.cos(phi), np.sin(phi)
        x = self.c @ cphi - self.s @ sphi
        dx = -self.c * sphi - self.s * cphi
        total, grad = 0.0, np.zeros(K)
        for w in self.windows:
            xw, dxw = x[w], dx[w]
            p = np.mean(xw ** 2)
            dp = 2 * np.mean(xw[:, None] * dxw, axis=0)
            y = xw ** 2 / p
            dy = 2 * xw[:, None] * dxw / p - (xw ** 2)[:, None] * dp[None, :] / p ** 2
            total += np.mean(y ** q)
            grad += np.mean(q * (y ** (q - 1))[:, None] * dy, axis=0)
        return float(np.log(total) / q), grad / (q * total)

    def descend(self, phi: np.ndarray) -> np.ndarray:
        for q in (4, 16, 64):
            phi = optimize.minimize(self.surrogate, phi, args=(q,), jac=True, method="L-BFGS-B",
                                    options={"maxiter": 400}).x
        return phi


def search_s1(seed: int = S1_SEARCH_SEED, restarts: int = S1_SEARCH_RESTARTS,
              verbose: bool = False) -> tuple[tuple[int, ...], float]:
    """Re-run the S1 search; returns (phase units u[k], worst-window PAPR dB)."""
    model = _PaprModel()
    rng = np.random.default_rng(seed)
    starts = [newman_phases()] + [rng.uniform(0, 2 * np.pi, K) for _ in range(restarts)]
    best_phi, best = None, np.inf
    for i, start in enumerate(starts):
        phi = model.descend(start)
        v = model.worst_db(phi)
        if v < best:
            best_phi, best = phi, v
            if verbose:
                print(f"start {i}: {v:.3f} dB")
    u = np.round(np.mod(best_phi, 2 * np.pi) / (2 * np.pi) * PHASE_DENOM).astype(int) % PHASE_DENOM

    def score(units: np.ndarray) -> float:
        return model.worst_db(2 * np.pi * units / PHASE_DENOM)

    cur = score(u)
    improved = True
    while improved:
        improved = False
        for k in range(K):
            for step in (1, -1, 2, -2, 3, -3, 4, -4, 6, -6, 8, -8):
                trial = u.copy()
                trial[k] = (trial[k] + step) % PHASE_DENOM
                v = score(trial)
                if v < cur - 1e-9:
                    u, cur, improved = trial, v, True
    return tuple(int(v) for v in u), cur


# --- S2 search --------------------------------------------------------------------


def _gold_all(length: int, c_inits: np.ndarray) -> np.ndarray:
    """38.211 Gold sequence c(0..length-1) for many c_init at once (rows)."""
    nc = 1600
    total = nc + length
    x1 = np.zeros(total + 31, dtype=np.uint8)
    x1[0] = 1
    for n in range(total):
        x1[n + 31] = x1[n + 3] ^ x1[n]
    # x2 is linear over GF(2) in its 31 initial bits: one response per bit.
    basis = np.zeros((31, length), dtype=np.uint8)
    for i in range(31):
        x2 = np.zeros(total + 31, dtype=np.uint8)
        x2[i] = 1
        for n in range(total):
            x2[n + 31] = x2[n + 3] ^ x2[n + 2] ^ x2[n + 1] ^ x2[n]
        basis[i] = x2[nc:nc + length]
    bits = ((c_inits[:, None] >> np.arange(31)[None, :]) & 1).astype(np.int64)
    return ((bits @ basis.astype(np.int64)) % 2).astype(np.uint8) ^ x1[None, nc:nc + length]


def search_s2(s1_values: np.ndarray | None = None, max_xcorr: float = S2_MAX_XCORR,
              n_inits: int = 1 << 16) -> tuple[int, float, float]:
    """Re-run the S2 search; returns (c_init, worst-window PAPR dB, xcorr peak)."""
    a = s1() if s1_values is None else s1_values
    inits = np.arange(n_inits)
    vals = 1.0 - 2.0 * _gold_all(K, inits).astype(float)              # (n_inits, 24)
    papr = np.full(n_inits, -np.inf)
    for cp in (params.CP_SHORT, params.CP_LONG):
        n = np.arange(-cp, N)
        th = 2 * np.pi * np.outer(n, params.BIN_OFFSET + np.arange(K)) / N
        x = vals @ np.cos(th).T                                          # BPSK: real part only
        papr = np.maximum(papr, 10 * np.log10(np.max(x ** 2, 1) / np.mean(x ** 2, 1)))
    tau = np.arange(0, N, 0.125)
    f = params.BIN_OFFSET + np.arange(K)
    e = np.exp(2j * np.pi * np.outer(f, tau) / N)                        # (24, n_tau)
    xc = np.max(np.abs((vals * np.conj(a)[None, :]) @ e), axis=1) / K
    ok = xc <= max_xcorr
    if not ok.any():
        raise ValueError("no c_init meets the cross-correlation bound")
    order = np.lexsort((inits, np.round(xc, 9), np.round(papr, 9)))
    best = next(i for i in order if ok[i])
    return int(best), float(papr[best]), float(xc[best])


# --- detector ---------------------------------------------------------------------

DECIM = 6                       # 48 kHz -> 8 kHz complex (analytic) processing
FS_D = params.FS // DECIM
_LH = 6 * 192 + 1               # front-end FIR length at 48 kHz (24 ms)
_Q = (_LH - 1) // DECIM
_MF = 8192                      # decimated FFT block
_BAND_CENTER_HZ = 1500.0        # 300-2700 Hz passband
_HALF_BAND_HZ = 1290.0          # lowpass prototype cutoff (-6 dB at 210 / 2790 Hz)

# Thresholds, from preamble_eval measurements. On open-squelch noise the
# false-alarm count falls roughly 10x per 0.05 of rho (white noise, rho
# alone: 324, 21 and 1 per 10 minutes at 0.40, 0.45 and 0.50) and the M gate
# removes a further ~30x; a steady tone mixed with noise lifts the largest
# gated rho to about 0.47 in 3 minutes. rho >= 0.55 with M >= 0.45 leaves
# that worst case well under 1 per 10 minutes, while the dispersive
# ``preamble_eval.repeater_chain`` (which costs ~0.2 of rho) still detects
# every frame at 4 dB. Detection at 6 dB on AWGN alone has >= 4 dB to spare.
XCORR_THRESHOLD = 0.55          # normalized S1 cross-correlation (rho)
AUTOCORR_THRESHOLD = 0.45       # Schmidl-Cox plateau metric (M)
# SPEC 6.1 sample-clock grid. At +-100 ppm the S1 reference decorrelates by
# well under 0.01 dB, so the grid buys nothing measurable at this tolerance;
# it is kept because SPEC 6.1 asks for it and costs only extra correlations.
PPM_GRID = (-100.0, 0.0, 100.0)
PREFIXES = (params.CP_SHORT, params.CP_LONG)


@cache
def _front_end_response() -> np.ndarray:
    h = signal.firwin(_LH, _HALF_BAND_HZ, window=("kaiser", 6.8), fs=params.FS)
    k = np.arange(_LH) - (_LH - 1) / 2
    hc = 2 * h * np.exp(2j * np.pi * _BAND_CENTER_HZ * k / params.FS)
    resp = np.fft.fft(hc, DECIM * _MF)[:_MF]
    resp.setflags(write=False)
    return resp


def analytic_decimate(x: np.ndarray) -> np.ndarray:
    """Band-limit real 48 kHz audio to ~300-2700 Hz, keep the positive
    frequencies and decimate by 6: z[m] ~ analytic x at sample 6 m.

    A linear-phase 1153-tap FIR (Kaiser, flat over 300-2700 Hz, ~70 dB
    stopband below 120 Hz, so DC and hum, and above 2880 Hz) applied by
    overlap-save; its delay is removed, so z lines up with the input. Bins
    above 4 kHz are dropped rather than folded.
    """
    x = np.asarray(x, dtype=float)
    n_out = -(-x.size // DECIM)
    c = (_LH - 1) // 2
    nf = DECIM * _MF
    step = DECIM * (_MF - _Q)
    xp = np.zeros(c + DECIM * n_out + nf + step)
    xp[c:c + x.size] = x
    resp = _front_end_response()
    out = np.empty(n_out, dtype=complex)
    for s in range(0, DECIM * n_out, step):
        seg = np.fft.rfft(xp[s:s + nf])[:_MF] * resp
        zz = np.fft.ifft(seg)[_Q:] / DECIM
        m0 = s // DECIM
        take = min(_MF - _Q, n_out - m0)
        out[m0:m0 + take] = zz[:take]
    return out


def _stride(cp: int) -> int:
    return N + cp


@cache
def _s1_reference(cp: int, ppm: float) -> np.ndarray:
    """Analytic S1, S1 (with prefixes) at 8 kHz, stretched by ppm."""
    stride = _stride(cp)
    m = np.arange(2 * stride // DECIM)
    t = DECIM * m * (1 + ppm * 1e-6)
    sym = np.floor(t / stride)
    local = t - sym * stride - cp
    ref = tone_waveform(s1(), local)
    ref.setflags(write=False)
    return ref


@cache
def _s2_reference(cp: int) -> np.ndarray:
    """Analytic S2, S2 (with prefixes) at 48 kHz."""
    stride = _stride(cp)
    t = np.arange(2 * stride)
    ref = tone_waveform(s2(), t % stride - cp)
    ref.setflags(write=False)
    return ref


def _moving_sum(v: np.ndarray, length: int) -> np.ndarray:
    """out[u] = sum(v[u : u + length]) for u = 0 .. len(v) - length."""
    cs = np.concatenate([[0], np.cumsum(v)])
    return cs[length:] - cs[:-length]


@dataclass(frozen=True)
class Detection:
    start: int          # 48 kHz index of the first sample of the first S1 prefix
    cp: int             # detected prefix length (60 or 120)
    quality: float      # normalized S1 cross-correlation rho, 0..1
    autocorr: float     # Schmidl-Cox plateau metric M at the detected lag, 0..1
    snr_db: float       # rough in-band SNR estimate, rho^2 / (1 - rho^2)
    ppm: float          # sample-clock grid point that matched best
    coarse_start: float  # start from the 8 kHz S1 correlation, before S2 refinement


class Detector:
    """SPEC 6.1 preamble detector, streaming.

    Processing, per prefix hypothesis cp in (60, 120), lag L = 480 + cp:

    1. Front end: band-limit to 300-2700 Hz, keep the analytic signal and
       decimate to 8 kHz (``analytic_decimate``). Lags and prefixes become
       90/100 and 10/20 samples; nothing else depends on the rate.
    2. Autocorrelation (Schmidl-Cox): P(u) = sum_{m<80} z[u+m] conj(z[u+m+L/6])
       and R(u) = half the energy of both windows; the plateau metric
       M(u) = |sum_{i=0..cp/6} P(u+i)| / sum_{i=0..cp/6} R(u+i) averages over
       the cp-long plateau a repeated symbol produces. M is ~1 for any
       periodic signal (a steady tone included) and ~S/(S+N) for a preamble.
    3. Cross-correlation with the stored S1, S1 waveform (two symbols with
       prefixes) for each sample-clock offset in ``ppm_grid``:
       rho(u) = |<z[u:u+2L/6], ref>| / (||z[u:...]|| ||ref||). A tone or DTMF
       pair carries at most ~2/24 of S1's energy, so rho stays below ~0.3.
    4. A detection is a local maximum (over +-3 symbol periods) of
       max_{cp, ppm} rho where rho >= ``xcorr_threshold`` and, for the same
       cp, M >= ``autocorr_threshold``. The prefix is the hypothesis with
       the larger rho; the wrong one matches at most one of its two symbols.
    5. Fine timing: at 48 kHz, the analytic S2, S2 waveform is correlated
       against the input over +-12 samples around the coarse position; the
       envelope peak (parabolic interpolation) gives the start.

    Detections carry absolute 48 kHz sample indices across ``process`` calls.
    """

    _HISTORY = 4096       # samples kept before the owned region (filter, NMS)
    _LOOKAHEAD = 8192     # samples needed after a start (preamble, filter, NMS, S2)

    def __init__(self, xcorr_threshold: float = XCORR_THRESHOLD,
                 autocorr_threshold: float = AUTOCORR_THRESHOLD,
                 ppm_grid: tuple[float, ...] = PPM_GRID,
                 prefixes: tuple[int, ...] = PREFIXES, fine_search: int = 12) -> None:
        self.xcorr_threshold = xcorr_threshold
        self.autocorr_threshold = autocorr_threshold
        self.ppm_grid = tuple(ppm_grid)
        self.prefixes = tuple(prefixes)
        self.fine_search = fine_search
        self._buf = np.zeros(0)
        self._buf_start = 0       # absolute index of _buf[0]
        self._owned = 0           # first absolute start not yet evaluated
        self.max_rho = 0.0        # largest rho seen (margin to the threshold)
        self.max_score = 0.0      # largest rho where the autocorrelation gate passed

    def process(self, samples: np.ndarray) -> list[Detection]:
        self._buf = np.concatenate([self._buf, np.asarray(samples, dtype=float)])
        return self._run(final=False)

    def flush(self) -> list[Detection]:
        self._buf = np.concatenate([self._buf, np.zeros(self._LOOKAHEAD)])
        dets = self._run(final=True)
        self._buf = np.zeros(0)
        return dets

    def _run(self, final: bool) -> list[Detection]:
        end = self._buf_start + self._buf.size
        own_hi = end - self._LOOKAHEAD
        if own_hi <= self._owned:
            return []
        stats: dict = {}
        found = scan(self._buf, self.xcorr_threshold, self.autocorr_threshold,
                     self.ppm_grid, self.prefixes, self.fine_search, stats)
        if stats:
            self.max_rho = max(self.max_rho, stats["max_rho"])
            self.max_score = max(self.max_score, stats["max_score"])
        out = []
        for d in found:
            a = d.start + self._buf_start
            if self._owned <= a < own_hi:
                out.append(Detection(a, d.cp, d.quality, d.autocorr, d.snr_db, d.ppm,
                                     d.coarse_start + self._buf_start))
        self._owned = own_hi
        keep_from = own_hi - self._HISTORY - self._buf_start
        if keep_from > 0:
            self._buf = self._buf[keep_from:]
            self._buf_start += keep_from
        return out


def scan(x: np.ndarray, xcorr_threshold: float = XCORR_THRESHOLD,
         autocorr_threshold: float = AUTOCORR_THRESHOLD,
         ppm_grid: tuple[float, ...] = PPM_GRID, prefixes: tuple[int, ...] = PREFIXES,
         fine_search: int = 12, stats: dict | None = None) -> list[Detection]:
    """One-shot detector over a whole buffer (``Detector`` streams it).

    ``stats``, if given, receives the per-position metrics (8 kHz arrays
    ``rho``/``autocorr`` per prefix) and their maxima: ``max_rho`` (rho
    regardless of the autocorrelation gate) and ``max_score`` (rho where the
    gate passed), for measuring the margin to the thresholds.
    """
    z = analytic_decimate(x)
    # Noise floor: 60 dB below the buffer's mean level. Without it, digital
    # silence (or rounding residue after a loud passage) divides 0 by 0.
    floor = max(1e-6 * float(np.mean(np.abs(z) ** 2)), 1e-30)
    lags = {cp: _stride(cp) // DECIM for cp in prefixes}
    n_d = N // DECIM
    span = 2 * max(lags.values())
    n_u = z.size - span - n_d
    if n_u <= 0:
        return []
    zz = np.abs(z) ** 2
    score = np.zeros(n_u)
    best_cp = np.zeros(n_u, dtype=int)
    best_ppm = np.zeros(n_u)
    per_cp = {}
    for cp in prefixes:
        lag = lags[cp]
        plateau = cp // DECIM + 1
        p = _moving_sum(z[:-lag] * np.conj(z[lag:]), n_d)
        r = 0.5 * (_moving_sum(zz[:-lag], n_d) + _moving_sum(zz[lag:], n_d))
        r_sum = np.maximum(_moving_sum(r, plateau)[:n_u], 0) + plateau * n_d * floor
        m = np.abs(_moving_sum(p, plateau)[:n_u]) / r_sum
        energy = np.maximum(_moving_sum(zz, 2 * lag)[:n_u], 0) + 2 * lag * floor
        rho = np.zeros(n_u)
        rho_ppm = np.zeros(n_u)
        for ppm in ppm_grid:
            ref = _s1_reference(cp, ppm)
            c = signal.oaconvolve(z, np.conj(ref[::-1]), mode="valid")[:n_u]
            val = np.abs(c) / np.sqrt(energy * np.sum(np.abs(ref) ** 2))
            better = val > rho
            rho = np.where(better, val, rho)
            rho_ppm = np.where(better, ppm, rho_ppm)
        per_cp[cp] = (rho, m)
        gated = np.where(m >= autocorr_threshold, rho, 0.0)
        upd = gated > score
        score = np.where(upd, gated, score)
        best_cp = np.where(upd, cp, best_cp)
        best_ppm = np.where(upd, rho_ppm, best_ppm)
    if stats is not None:
        stats.update({"rho": {cp: v[0] for cp, v in per_cp.items()},
                      "autocorr": {cp: v[1] for cp, v in per_cp.items()},
                      "max_rho": max(float(v[0].max()) for v in per_cp.values()),
                      "max_score": float(score.max())})

    # +-3 symbol periods: the frame's own S2 pair (2 periods later) also
    # repeats and must fall inside the suppression window.
    window = 2 * 3 * max(lags.values()) + 1
    peak = ndimage.maximum_filter1d(score, window, mode="constant")
    cand = np.flatnonzero((score >= xcorr_threshold) & (score == peak))
    dets = []
    last = -window
    for u in cand:
        if u - last < window // 2:      # equal-valued neighbours: keep the first
            continue
        last = u
        cp = int(best_cp[u])
        rho, m = per_cp[cp]
        # Sub-sample coarse timing from the rho peak.
        if 0 < u < n_u - 1:
            y0, y1, y2 = rho[u - 1], rho[u], rho[u + 1]
            den = y0 - 2 * y1 + y2
            frac = 0.5 * (y0 - y2) / den if den < 0 else 0.0
        else:
            frac = 0.0
        coarse = DECIM * (u + float(np.clip(frac, -0.5, 0.5)))
        start = _fine_timing(x, coarse, cp, fine_search)
        if start is None:
            continue
        q = float(rho[u])
        snr = 10 * np.log10(q * q / max(1 - q * q, 1e-6))
        dets.append(Detection(start, cp, q, float(m[u]), float(snr), float(best_ppm[u]), coarse))
    return dets


def _fine_timing(x: np.ndarray, coarse: float, cp: int, search: int) -> int | None:
    stride = _stride(cp)
    ref = _s2_reference(cp)
    centre = int(round(coarse)) + 2 * stride
    lo = centre - search
    hi = lo + 2 * search + ref.size
    if lo < 0 or hi > x.size:
        return None
    win = np.lib.stride_tricks.sliding_window_view(x[lo:hi], ref.size)
    env = np.abs(win @ np.conj(ref))
    i = int(np.argmax(env))
    frac = 0.0
    if 0 < i < env.size - 1:
        y0, y1, y2 = env[i - 1], env[i], env[i + 1]
        den = y0 - 2 * y1 + y2
        if den < 0:
            frac = float(np.clip(0.5 * (y0 - y2) / den, -0.5, 0.5))
    return int(round(lo + i + frac)) - 2 * stride


def detect(samples: np.ndarray, **kwargs) -> list[Detection]:
    """Detect every preamble in ``samples`` (real 48 kHz audio).

    Keyword arguments go to ``Detector``. Each detection's ``start`` is the
    first sample of the first S1 symbol's prefix, as ``frame.receive(start=...)``
    expects.
    """
    det = Detector(**kwargs)
    chunk = 1 << 21
    out = []
    for i in range(0, len(samples), chunk):
        out += det.process(samples[i:i + chunk])
    return out + det.flush()


if __name__ == "__main__":
    u, v = search_s1(verbose=True)
    print("S1_PHASE_UNITS =", u, f"worst-window PAPR {v:.3f} dB")
    vals = np.exp(2j * np.pi * np.array(u) / PHASE_DENOM)
    print("S2:", search_s2(vals))
