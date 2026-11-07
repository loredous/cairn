"""Frame assembly and reception (SPEC 6, 8.1).

A frame is ``preamble (4) | header (7) | [probe (2)] | data blocks``. The
receiver finds the frame by cross-correlating with the known preamble
waveform (or takes a start from the SPEC 6.1 detector), estimates the
per-tone channel and noise from the two S2 symbols on the base tones and
from the width probe above them, tracks common phase and timing drift with
the continuous pilots, and decodes the header and every data block.

Tone mask (SPEC 5.4). ``FrameConfig.mask`` is the set of usable tones. It
applies to data symbols only: preamble, header and width probe never change,
so any station can decode the header and the probe measures every tone up to
the width. In a data symbol a masked tone carries nothing (a masked pilot 37
or 52 is simply absent), coded bits fill the remaining data tones in order,
and every active tone is scaled by ``FrameConfig.data_gain`` =
sqrt(width / active tones) so the symbol's total power is the same as an
unmasked one. The receiver divides that known factor back out.

Not yet modelled: control blocks.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from functools import cached_property

import numpy as np
from scipy import signal

from . import ldpc, mapping, ofdm, params, preamble
from .crc import append_crc

PILOT_C_INIT = 0x0B1D   # pilot BPSK sequence seed (ADR 0005 pilot-sequence)
PROBE_C_INIT = 0x0D0E   # width-probe BPSK sequence seed (ADR 0005 probe-sequence)
PAD_POSITION = 255      # scrambler position for padding bits (ADR 0005 scrambler-sequence)
HEADER_POSITION = 0     # scrambler position of the header; block i uses 1 + i
QPSK = 2


@dataclass(frozen=True)
class FrameConfig:
    width: int = params.BASE_TONES
    mcs: int = 0
    n_blocks: int = 1
    cp: int = params.CP_SHORT
    clip: bool = True
    clip_db: float | None = None  # clip level, dB above RMS; default: the rung's (params.clip_db)
    probe: bool | None = None   # width probe after the header; default: when width > 24
    mask: frozenset[int] | None = None  # usable tones (SPEC 5.4); None: every tone usable
    interleaver: str = "linear"         # mapping.INTERLEAVERS; "random" is an evaluation reference

    def __post_init__(self) -> None:
        if not 0 <= self.mcs < len(params.RUNGS):
            raise ValueError(f"no rung MCS {self.mcs}")
        if self.clip_db is None:
            # The whole frame (preamble, header, probe, data) is clipped at the
            # data rung's level, so every symbol shares one per-tone scale and
            # the S2 channel estimate stays valid for the data.
            object.__setattr__(self, "clip_db", params.clip_db(self.mcs))
        if not params.BASE_TONES <= self.width <= params.MAX_TONES:
            raise ValueError(f"width must be {params.BASE_TONES}-{params.MAX_TONES} tones")
        if self.width > params.BASE_TONES and self.probe is False:
            raise ValueError("tones above the base set need the width probe")
        if self.mask is not None:
            object.__setattr__(self, "mask", frozenset(int(k) for k in self.mask))
        if self.interleaver not in mapping.INTERLEAVERS:
            raise ValueError(f"unknown interleaver {self.interleaver!r}")
        params.masked_tones(self.width, self.mask)          # validates the mask

    @property
    def has_probe(self) -> bool:
        return self.width > params.BASE_TONES if self.probe is None else self.probe

    @property
    def probe_symbols(self) -> int:
        return params.PROBE_SYMBOLS if self.has_probe else 0

    @property
    def rung(self) -> params.Rung:
        return params.RUNGS[self.mcs]

    @property
    def stride(self) -> int:
        return params.N_USEFUL + self.cp

    @cached_property
    def base_data_tones(self) -> tuple[int, ...]:
        return params.data_tones_for(params.BASE_TONES)

    @cached_property
    def masked(self) -> frozenset[int]:
        """Tones below the width that data symbols leave silent."""
        return params.masked_tones(self.width, self.mask)

    @cached_property
    def data_tones(self) -> tuple[int, ...]:
        """Tones carrying coded bits in data symbols, lowest first."""
        return params.data_tones_for(self.width, self.masked)

    @cached_property
    def data_pilots(self) -> tuple[int, ...]:
        """Pilot tones present in data symbols (37 and 52 drop out when masked)."""
        return tuple(p for p in params.pilots_for(self.width) if p not in self.masked)

    @cached_property
    def data_active_tones(self) -> tuple[int, ...]:
        return tuple(sorted(self.data_tones + self.data_pilots))

    @property
    def data_gain(self) -> float:
        """Amplitude factor on every active tone of a data symbol: total
        power stays that of ``width`` unit-power tones whatever the mask."""
        return float(np.sqrt(self.width / len(self.data_active_tones)))

    @property
    def header_symbols(self) -> int:
        e = params.HEADER_CODE.coded_bits[0]
        return -(-e // (QPSK * len(self.base_data_tones)))

    @property
    def data_bits(self) -> int:
        return self.n_blocks * self.rung.coded_bits

    @property
    def data_symbols(self) -> int:
        per_symbol = self.rung.bits_per_symbol * len(self.data_tones)
        return -(-self.data_bits // per_symbol)

    @property
    def total_symbols(self) -> int:
        return params.PREAMBLE_SYMBOLS + self.header_symbols + self.probe_symbols + self.data_symbols

    @property
    def n_samples(self) -> int:
        return self.total_symbols * self.stride


def pilot_values(n_symbols: int, n_pilots: int) -> np.ndarray:
    """Pilot BPSK values (n_symbols, n_pilots) for the first n_pilots pilot tones.

    Pilot j of symbol s is 1 - 2 c(4 s + j), so a pilot's value depends only
    on the symbol and which pilot tone it is, not on how many are active.
    """
    slots = len(params.PILOT_TONES)
    c = mapping.gold_sequence(PILOT_C_INIT, n_symbols * slots)
    return (1.0 - 2.0 * c.astype(float)).reshape(n_symbols, slots)[:, :n_pilots]


def pilots_on(n_symbols: int, pilots: tuple[int, ...]) -> np.ndarray:
    """Pilot values (n_symbols, len(pilots)) for any subset of the pilot tones."""
    slots = [params.PILOT_TONES.index(p) for p in pilots]
    return pilot_values(n_symbols, len(params.PILOT_TONES))[:, slots]


def _fill_symbols(points: np.ndarray, n_sym: int, width: int, data_tones: tuple[int, ...],
                  pilots: tuple[int, ...], first_symbol: int) -> np.ndarray:
    g = np.zeros((n_sym, width), dtype=complex)
    g[:, list(data_tones)] = points.reshape(n_sym, len(data_tones))
    g[:, list(pilots)] = pilots_on(first_symbol + n_sym, pilots)[first_symbol:]
    return g


def probe_grid(width: int, first_symbol: int) -> np.ndarray:
    """Width-probe symbols (SPEC 5.4): known BPSK on every tone, pilots on the pilot tones.

    Tone k of probe symbol r is 1 - 2 c(56 r + k) (ADR 0005 probe-sequence),
    so a tone's value does not depend on the probe width.
    """
    c = mapping.gold_sequence(PROBE_C_INIT, params.PROBE_SYMBOLS * params.MAX_TONES)
    g = (1.0 - 2.0 * c.astype(float)).reshape(params.PROBE_SYMBOLS, params.MAX_TONES)[:, :width]
    g = g.astype(complex)
    pilots = params.pilots_for(width)
    g[:, list(pilots)] = pilot_values(first_symbol + params.PROBE_SYMBOLS, len(pilots))[first_symbol:]
    return g


def header_points(cfg: FrameConfig, header_bits: np.ndarray) -> np.ndarray:
    """The header's QPSK points on the base data tones, padding included."""
    hcode = ldpc.code_for(params.HEADER_CODE)
    e_h = params.HEADER_CODE.coded_bits[0]
    hcoded = hcode.rate_match(hcode.encode(header_bits[None])[0], e_h)
    hbits = mapping.interleave(mapping.scramble(hcoded, HEADER_POSITION), cfg.interleaver, HEADER_POSITION)
    h_res = cfg.header_symbols * len(cfg.base_data_tones) * QPSK
    hbits = np.concatenate([hbits, _pad_bits(h_res - hbits.size)])
    return mapping.modulate(hbits, QPSK)


def _pad_bits(n: int) -> np.ndarray:
    return mapping.scramble_sequence(PAD_POSITION, n) if n else np.zeros(0, dtype=np.uint8)


@dataclass
class TxFrame:
    config: FrameConfig
    samples: np.ndarray
    header_bits: np.ndarray        # (144,) incl. CRC
    block_bits: np.ndarray         # (n_blocks, 1024) incl. CRC
    grid: np.ndarray = field(repr=False)  # (total_symbols, width) tone values before clipping

    @property
    def preamble_samples(self) -> np.ndarray:
        return self.samples[: params.PREAMBLE_SYMBOLS * self.config.stride]


def build(cfg: FrameConfig, rng: np.random.Generator, header_msg: np.ndarray | None = None,
          payloads: np.ndarray | None = None) -> TxFrame:
    """Assemble a frame; random header and payloads unless given."""
    dcode = ldpc.code_for(params.DATA_CODE)
    rung = cfg.rung

    if header_msg is None:
        header_msg = rng.integers(0, 2, params.HEADER_INFO_BITS - 16, dtype=np.uint8)
    header_bits = append_crc(header_msg[None])[0]
    hpoints = header_points(cfg, header_bits)

    if payloads is None:
        payloads = rng.integers(0, 2, (cfg.n_blocks, params.DATA_PAYLOAD_BITS), dtype=np.uint8)
    block_bits = append_crc(payloads)
    coded = dcode.rate_match(dcode.encode(block_bits), rung.coded_bits)
    stream = np.concatenate([mapping.interleave(mapping.scramble(coded[i], 1 + i), cfg.interleaver, 1 + i)
                             for i in range(cfg.n_blocks)])
    d_res = cfg.data_symbols * len(cfg.data_tones) * rung.bits_per_symbol
    stream = np.concatenate([stream, _pad_bits(d_res - stream.size)])
    dpoints = mapping.modulate(stream, rung.bits_per_symbol)

    p_base = params.pilots_for(params.BASE_TONES)
    first_h = params.PREAMBLE_SYMBOLS
    hgrid = _fill_symbols(hpoints, cfg.header_symbols, cfg.width, cfg.base_data_tones, p_base, first_h)
    first_p = first_h + cfg.header_symbols
    pgrid = probe_grid(cfg.width, first_p) if cfg.has_probe else np.zeros((0, cfg.width), complex)
    first_d = first_p + cfg.probe_symbols
    dgrid = _fill_symbols(dpoints, cfg.data_symbols, cfg.width, cfg.data_tones,
                          cfg.data_pilots, first_d) * cfg.data_gain
    grid = np.concatenate([preamble.grid(cfg.width), hgrid, pgrid, dgrid])

    # Every symbol uses the same per-tone scale (SPEC 6.2), set by the link
    # width; a tone mask raises data symbols by the known cfg.data_gain.
    # Clip-and-filter projects each symbol back onto the tones it uses, so
    # clipping noise does not refill masked tones.
    scale = cfg.width
    segments = [(grid[:first_p], range(params.BASE_TONES)),
                (grid[first_p:first_d], range(cfg.width)),
                (grid[first_d:], cfg.data_active_tones)]
    out = []
    for g, tones in segments:
        if not len(g):
            continue
        x = ofdm.modulate(g, cfg.cp, active=scale)
        if cfg.clip:
            x = ofdm.clip_and_filter(x, cfg.width, np.array(list(tones)), cfg.cp,
                                     clip_db=cfg.clip_db, scale_tones=scale)
        out.append(x)
    return TxFrame(cfg, np.concatenate(out), header_bits, block_bits, grid)


# --- receiver ---------------------------------------------------------------


@dataclass
class RxResult:
    header_ok: bool
    header_bits: np.ndarray
    block_ok: np.ndarray            # (n_blocks,) bool
    block_bits: np.ndarray          # (n_blocks, 1024)
    start: int                      # detected frame start (sample index)
    snr_db: np.ndarray              # per tone, from S2 (base tones) and the probe
    iterations: np.ndarray
    # Receiver internals for measurement tools (bench rx): channel ``h`` and
    # ``noise`` per tone, equalized symbols ``eq`` (rows after the preamble),
    # ``d0`` (first data row of ``eq``), ``window_start``, ``shifts``, ``drift``.
    detail: dict = field(default_factory=dict, repr=False)


def header_noise_floor(pre: np.ndarray, h: np.ndarray, thermal: np.ndarray,
                       pilot_excess: np.ndarray) -> np.ndarray:
    """Noise variance the preamble and pilots predict for header symbols.

    ``pre`` is the four preamble symbols on the base tones (S1, S1, S2, S2),
    ``h`` the channel, ``thermal`` the S2 noise estimate and
    ``pilot_excess`` the pilots' distortion (relative to signal power) in
    the data symbols. Returned as variance of the equalized symbols.

    * Noise: the differences of both repeated pairs (two samples per tone,
      against one from S2 alone), smoothed by a linear fit over 11 tones.
      That follows a smooth tilt (a flat port's discriminator noise) but
      not the frame-to-frame scatter of a 7-tone average, which mis-weights
      tones in AWGN. Never below a quarter of the S2 estimate.
    * Distortion: the pilots' excess, averaged into one band-wide figure.
      Interpolated from pilot 3 to pilot 20 instead, as the data symbols
      use it, a distorted low band edge (the repeater's) raises every tone
      up to 20 and drowns the good ones; local distortion is left to the
      header's own measurement (``estimate_header_noise``).
    """
    raw = (np.abs(pre[0] - pre[1]) ** 2 + np.abs(pre[2] - pre[3]) ** 2) / 4
    smooth = np.maximum(signal.savgol_filter(raw, 11, 1, mode="interp"), thermal / 4)
    return smooth / np.abs(h) ** 2 + (float(np.mean(pilot_excess)) if pilot_excess.size else 0.0)


def estimate_header_noise(eq: np.ndarray, known_pilots: np.ndarray, floor: np.ndarray,
                          data_tones: list[int], pilot_tones: list[int],
                          margin: float = 2.0) -> np.ndarray:
    """Noise variance per base tone for the header's equalized symbols.

    ``floor`` is the variance expected from what the preamble and pilots
    measured (as variance of ``eq``). The header symbols themselves give a
    second, local measurement: QPSK has unit power, so mean |eq|^2 - 1 over
    the header's symbols is the error variance on a data tone, whatever its
    cause (noise, inter-symbol interference beyond the prefix, a limiter,
    channel-estimate error); on a pilot tone the known value gives it
    directly. Pooled over three neighbouring tones, it is noisy (each
    sample has variance 2v + v^2), so it raises the floor only by what it
    exceeds it with confidence: its value less ``margin`` standard
    deviations.
    """
    n = floor.size
    m = np.full(n, np.nan)
    m[data_tones] = np.mean(np.abs(eq[:, data_tones]) ** 2, axis=0) - 1
    m[pilot_tones] = np.mean(np.abs(eq[:, pilot_tones] - known_pilots) ** 2, axis=0)
    kernel = np.ones(3)
    count = np.convolve(np.ones(n), kernel, mode="same")
    pooled = np.convolve(m, kernel, mode="same") / count
    sd = np.sqrt((2 * floor + floor ** 2) / (count * eq.shape[0]))
    return np.maximum(floor, pooled - margin * sd)


def estimate_channel(y2: np.ndarray, y3: np.ndarray, s2: np.ndarray, window: int = 7,
                     second: np.ndarray | None = None) -> np.ndarray:
    """Per-tone channel from the two S2 symbols, smoothed across tones.

    The audio chain's response is smooth in frequency apart from a linear
    phase (the delay), so the estimate is de-rotated by its mean phase step,
    smoothed with a local quadratic fit over ``window`` tones, and rotated
    back. ``window`` < 3 returns the raw estimate.
    """
    raw = (y2 / s2 + y3 / (s2 if second is None else second)) / 2
    if window < 3:
        return raw
    k = np.arange(raw.size)
    step = np.angle(np.sum(raw[1:] * np.conj(raw[:-1])))
    flat = raw * np.exp(-1j * step * k)
    smooth = (signal.savgol_filter(flat.real, window, 2, mode="interp")
              + 1j * signal.savgol_filter(flat.imag, window, 2, mode="interp"))
    return smooth * np.exp(1j * step * k)


def estimate_noise(y2: np.ndarray, y3: np.ndarray, window: int = 7) -> np.ndarray:
    """Per-tone noise variance from the difference of the two S2 symbols."""
    raw = np.abs(y2 - y3) ** 2 / 2
    kernel = np.ones(window) / window
    return np.convolve(raw, kernel, mode="same") / np.convolve(np.ones(raw.size), kernel, mode="same")


def track_drift(pilot_obs: np.ndarray, pilots: tuple[int, ...], width: int, *,
                s_ref: float, window: int = 15) -> tuple[np.ndarray, float]:
    """Phase correction per symbol and tone from the continuous pilots.

    ``pilot_obs`` is (n_sym, n_pilots): received pilot values divided by the
    known pilot symbols. The channel itself is static, so only the change of
    pilot phase over the frame is used: each pilot's phase track gets its own
    intercept (which soaks up channel-estimate error) and the slopes are fit
    as alpha + beta * f, a common phase drift plus the timing drift a
    sample-clock offset causes. The correction is zero at ``s_ref``, the
    symbol position (relative to ``pilot_obs`` row 0) where the channel was
    estimated.

    Returns the correction (n_sym, width) and the timing drift in samples per
    symbol implied by beta (a delay of d samples turns tone bin f by
    -2 pi f d / N).
    """
    n_sym = pilot_obs.shape[0]
    kernel = np.ones(window) / window
    s = np.arange(n_sym, dtype=float)
    slopes = np.zeros(len(pilots))
    for j in range(len(pilots)):
        valid = ~np.isnan(pilot_obs[:, j])        # a contiguous run of rows
        rows = s[valid]
        k = kernel if rows.size >= kernel.size else np.ones(rows.size) / rows.size
        sm = np.convolve(pilot_obs[valid, j], k, mode="same")
        theta = np.unwrap(np.angle(sm))
        sc = rows - rows.mean()
        slopes[j] = (sc @ (theta - theta.mean())) / max(sc @ sc, 1e-12)
    f = params.BIN_OFFSET + np.array(pilots, dtype=float)
    beta, alpha = np.polyfit(f, slopes, 1) if len(pilots) > 1 else (0.0, slopes[0])
    all_f = params.BIN_OFFSET + np.arange(width)
    corr = np.exp(1j * (alpha + beta * all_f)[None, :] * (s - s_ref)[:, None])
    return corr, float(-beta * params.N_USEFUL / (2 * np.pi))


def _estimate(tones: np.ndarray, cfg: FrameConfig, smooth_window: int, phase_window: int,
              robust: bool = False):
    """Channel, noise and drift estimates from demodulated tones.

    ``tones`` has the data symbols already divided by ``cfg.data_gain``.
    Returns ``noise`` (noise plus distortion per tone, at the preamble's
    scale) and ``thermal``, its additive-noise part, which a data symbol
    raised by the mask gain sees reduced by ``data_gain ** 2``. With
    ``robust``, data symbols whose power shows a noise burst are left out of
    the pilots' phase tracking and distortion estimate.
    """

    b = params.BASE_TONES
    y2, y3 = tones[2, :b], tones[3, :b]
    h = np.zeros(cfg.width, dtype=complex)
    h[:b] = estimate_channel(y2, y3, preamble.s2(), smooth_window)
    noise = np.zeros(cfg.width)
    noise[:b] = estimate_noise(y2, y3)
    first = params.PREAMBLE_SYMBOLS
    hs = cfg.header_symbols
    if cfg.has_probe:
        p0 = first + hs
        known = probe_grid(cfg.width, p0)
        hp = estimate_channel(tones[p0], tones[p0 + 1], known[0], smooth_window, second=known[1])
        np_ = estimate_noise(tones[p0] / known[0], tones[p0 + 1] / known[1]) * np.abs(known[0]) ** 2
        h[b:] = hp[b:]
        noise[b:] = np_[b:]
    noise = np.maximum(noise, 1e-6 * np.mean(np.abs(h) ** 2))
    thermal = noise.copy()
    snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)

    body = tones[first:]
    pilots = cfg.data_pilots          # masked pilots are absent from data symbols
    pv = pilots_on(tones.shape[0], pilots)[first:]
    obs = body[:, list(pilots)] / pv
    obs[:hs, [j for j, p in enumerate(pilots) if p >= b]] = np.nan   # header: base pilots only
    d0 = hs + cfg.probe_symbols
    if robust and body.shape[0] > d0:
        # A burst hitting one symbol's pilots would turn the smoothed phase
        # track for the symbols around it: leave such symbols out.
        act = list(cfg.data_active_tones)
        power = np.mean(np.abs(body[d0:, act]) ** 2 / np.abs(h[act]) ** 2, axis=1)
        obs[d0:][_outliers(power)] = np.nan
    corr, drift = track_drift(obs, pilots, cfg.width, s_ref=2.5 - first, window=phase_window)

    # The S2 pair sees only noise: both symbols carry the same clipping
    # distortion, so it cancels in their difference. Data symbols also carry
    # distortion (clipping, a radio limiter) and inter-symbol interference,
    # which the pilots in the data symbols measure; add it, as a fraction of
    # signal power interpolated between pilots, to every tone.
    excess = np.zeros(len(pilots))
    if body.shape[0] > d0:
        hp = h[list(pilots)]
        resid = obs[d0:] / (hp[None, :] * corr[d0:, list(pilots)]) - 1
        r2 = np.abs(resid) ** 2
        r2 = r2[~np.isnan(r2).any(axis=1)]
        floor = noise[list(pilots)] / (np.abs(hp) ** 2 * cfg.data_gain ** 2)
        excess = np.maximum(np.mean(r2, axis=0) - floor, 0)
        noise = noise + np.interp(np.arange(cfg.width), pilots, excess) * np.abs(h) ** 2
        snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)
    return h, noise, thermal, excess, snr_db, corr, body, drift


def _outliers(m: np.ndarray, k_sigma: float = 6.0) -> np.ndarray:
    """Values more than ``k_sigma`` robust standard deviations above the median."""
    med = np.median(m)
    spread = 1.4826 * np.median(np.abs(m - med)) + 1e-9
    return m - med > k_sigma * spread


def burst_noise(eq: np.ndarray, nv: np.ndarray, h: np.ndarray) -> np.ndarray:
    """Extra noise variance (n_sym, n_tones) for symbols hit by a burst.

    ``eq`` and ``nv`` are equalized data symbols on their active tones and the
    noise variance assumed for them; ``h`` the channel on those tones. A
    symbol's mean |eq|^2 is 1 plus its noise; one well above the frame's
    median (``_outliers``) carries extra white noise of that excess power,
    which is spread over the tones as 1/|h|^2 (white noise before
    equalization) and added to their variance.
    """
    m = np.mean(np.abs(eq) ** 2 - nv, axis=1)
    excess = np.where(_outliers(m), m - np.median(m), 0.0)
    inv = 1 / np.abs(h) ** 2
    return excess[:, None] * (inv / inv.mean())[None, :]


def _data_noise(noise: np.ndarray, thermal: np.ndarray, gain: float) -> np.ndarray:
    """Noise plus distortion on data symbols scaled up by ``gain``: the
    additive part drops by gain ** 2 once the receiver divides the gain out;
    distortion scales with the signal and stays."""
    return np.maximum(noise - thermal * (1 - gain ** -2), noise / gain ** 2)


def find_start(rx: np.ndarray, reference: np.ndarray) -> int:
    """Frame start by cross-correlation with the known preamble waveform."""
    corr = signal.correlate(rx, reference, mode="valid", method="fft")
    return int(np.argmax(np.abs(corr)))


def receive(rx: np.ndarray, cfg: FrameConfig, *, reference: np.ndarray | None = None,
            start: int | None = None, advance: int | None = None,
            phase_window: int = 15, smooth_window: int = 7,
            decision_directed: bool = True, burst_aware: bool = True,
            header_noise: str = "measured") -> RxResult:
    """Decode one frame from real samples ``rx``.

    Give ``start`` (index of the frame's first sample) or ``reference`` (the
    preamble waveform) to locate it. ``advance`` moves the FFT window that
    many samples into the prefix, to absorb channel delay spread. With
    ``decision_directed``, the decoded header re-estimates each base tone's
    noise plus distortion before the data blocks are demapped. With
    ``burst_aware``, data symbols whose power shows a noise burst get their
    noise variance raised by it (``burst_noise``), so their LLRs are
    discounted instead of trusted (on by default; it never triggered on the
    AWGN, mic or repeater profiles in Stage 1). ``header_noise`` picks the header's
    per-tone noise: "measured" (``header_noise_floor`` raised by
    ``estimate_header_noise``) or "s2" (the S2 difference plus the pilots'
    excess interpolated across the band, as used for the data).
    """
    if header_noise not in ("s2", "measured"):
        raise ValueError(f"unknown header_noise {header_noise!r}")
    if start is None:
        if reference is None:
            reference = ofdm.modulate(preamble.grid(cfg.width), cfg.cp, active=cfg.width)
        start = find_start(rx, reference)
    advance = cfg.cp // 4 if advance is None else advance
    n_sym = cfg.total_symbols
    first = params.PREAMBLE_SYMBOLS
    window_start = start + cfg.cp - advance
    d_first = first + cfg.header_symbols + cfg.probe_symbols   # first data symbol
    tones = ofdm.demodulate(rx, window_start, n_sym, cfg.width, cfg.cp, active=cfg.width)
    tones[d_first:] /= cfg.data_gain                           # the mask's known power boost
    h, noise, thermal, excess, snr_db, corr, body, drift = _estimate(
        tones, cfg, smooth_window, phase_window, burst_aware)
    # Follow sample-clock drift: if the windows would wander by a sample or
    # more over the frame, move each symbol's window and estimate again.
    pos = np.arange(n_sym) - 2.5                      # symbols from the channel estimate
    shifts = np.round(drift * pos).astype(int)
    if np.any(shifts != 0):
        rx = np.concatenate([rx, np.zeros(max(int(shifts.max()), 0) + 1)])
        tones = ofdm.demodulate(rx, window_start, n_sym, cfg.width, cfg.cp, active=cfg.width,
                                shifts=shifts)
        # Undo each shift's known phase (moving the window d samples later
        # turns tone bin f by 2 pi f d / N), so the tracker still sees a
        # smooth drift; the window move itself keeps the prefix centred.
        f = params.BIN_OFFSET + np.arange(cfg.width)
        tones = tones * np.exp(-2j * np.pi * f[None, :] * shifts[:, None] / params.N_USEFUL)
        tones[d_first:] /= cfg.data_gain
        h, noise, thermal, excess, snr_db, corr, body, drift = _estimate(
            tones, cfg, smooth_window, phase_window, burst_aware)
    hs = cfg.header_symbols
    d0 = hs + cfg.probe_symbols
    eq = body / (h[None, :] * corr)
    nv = noise[None, :] / np.abs(h[None, :]) ** 2 * np.ones_like(body.real)
    nv[d0:] = (_data_noise(noise, thermal, cfg.data_gain) / np.abs(h) ** 2)[None, :]

    # Header.
    hcode = ldpc.code_for(params.HEADER_CODE)
    btones = list(cfg.base_data_tones)
    if header_noise == "measured":
        b = params.BASE_TONES
        floor = header_noise_floor(tones[:4, :b], h[:b], thermal[:b], excess)
        bp = list(params.pilots_for(b))
        known_p = pilots_on(first + hs, tuple(bp))[first:]
        nv[:hs, :b] = estimate_header_noise(eq[:hs, :b], known_p, floor, btones, bp)[None, :]
    hl = mapping.demodulate_llr(eq[:hs, btones], nv[:hs, btones], QPSK)
    e_h = params.HEADER_CODE.coded_bits[0]
    hl = mapping.descramble_llr(mapping.deinterleave(hl[:e_h], cfg.interleaver, HEADER_POSITION),
                                HEADER_POSITION)
    hres = ldpc.decode(hcode, hcode.rate_recover(hl, e_h), rows=hcode.rows_needed(hcode.tx_positions(e_h)))
    if hres.ok[0] and decision_directed:
        # The decoded header is a known signal on every base data tone: its
        # error per tone is noise plus distortion plus ISI, measured where
        # it happens rather than interpolated between two pilots.
        known = header_points(cfg, hres.bits[0]).reshape(hs, len(btones))
        err = np.mean(np.abs(eq[:hs, btones] - known) ** 2, axis=0)
        rel = np.interp(np.arange(params.BASE_TONES), btones, err)
        rel = np.convolve(rel, np.ones(3) / 3, mode="same") / np.convolve(
            np.ones(params.BASE_TONES), np.ones(3) / 3, mode="same")
        noise[:params.BASE_TONES] = np.maximum(rel * np.abs(h[:params.BASE_TONES]) ** 2,
                                               1e-6 * np.mean(np.abs(h) ** 2))
        snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)
        nv[d0:] = (_data_noise(noise, thermal, cfg.data_gain) / np.abs(h) ** 2)[None, :]

    # Data blocks.
    dcode = ldpc.code_for(params.DATA_CODE)
    rung = cfg.rung
    dt = list(cfg.data_tones)
    if burst_aware and body.shape[0] > d0:
        act = list(cfg.data_active_tones)
        nv[d0:, act] += burst_noise(eq[d0:, act], nv[d0:, act], h[act])
    dl = mapping.demodulate_llr(eq[d0:, dt], nv[d0:, dt], rung.bits_per_symbol)
    e = rung.coded_bits
    llrs = np.stack([mapping.descramble_llr(mapping.deinterleave(dl[i * e:(i + 1) * e], cfg.interleaver, 1 + i),
                                            1 + i)
                     for i in range(cfg.n_blocks)])
    dres = ldpc.decode(dcode, dcode.rate_recover(llrs, e), rows=dcode.rows_needed(dcode.tx_positions(e)))
    detail = {"h": h, "noise": noise, "thermal": thermal, "eq": eq, "d0": d0, "window_start": window_start,
              "shifts": shifts, "drift": drift}
    return RxResult(bool(hres.ok[0]), hres.bits[0], dres.ok, dres.bits, start, snr_db, dres.iterations, detail)
