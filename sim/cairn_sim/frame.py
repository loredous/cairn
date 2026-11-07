"""Frame assembly and reception (SPEC 6, 8.1).

A frame is ``preamble (4) | header (7) | data blocks``. The thin-slice
receiver here finds the frame by cross-correlating with the known preamble
waveform, estimates the per-tone channel and noise from the two S2 symbols,
tracks common phase and timing drift with the continuous pilots, and decodes
the header and every data block.

Not yet modelled (later Stage 1 work): control blocks, the width probe (so
widths above 24 tones), the blind dual-lag detector of SPEC 6.1, and
decision-directed re-estimation.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from functools import cached_property

import numpy as np
from scipy import signal

from . import ldpc, mapping, ofdm, params, preamble
from .crc import append_crc

PILOT_C_INIT = 0x0B1D   # provisional pilot BPSK sequence seed
PAD_POSITION = 255      # provisional scrambler position for padding bits
HEADER_POSITION = 0     # scrambler position of the header; block i uses 1 + i
QPSK = 2


@dataclass(frozen=True)
class FrameConfig:
    width: int = params.BASE_TONES
    mcs: int = 0
    n_blocks: int = 1
    cp: int = params.CP_SHORT
    clip: bool = True

    def __post_init__(self) -> None:
        if self.width != params.BASE_TONES:
            raise NotImplementedError("widths above 24 tones need the width probe (not yet modelled)")

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
    def data_tones(self) -> tuple[int, ...]:
        return params.data_tones_for(self.width)

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
        return params.PREAMBLE_SYMBOLS + self.header_symbols + self.data_symbols

    @property
    def n_samples(self) -> int:
        return self.total_symbols * self.stride


def pilot_values(n_symbols: int, n_pilots: int) -> np.ndarray:
    c = mapping.gold_sequence(PILOT_C_INIT, n_symbols * n_pilots)
    return (1.0 - 2.0 * c.astype(float)).reshape(n_symbols, n_pilots)


def _fill_symbols(points: np.ndarray, n_sym: int, width: int, data_tones: tuple[int, ...],
                  pilots: tuple[int, ...], first_symbol: int) -> np.ndarray:
    g = np.zeros((n_sym, width), dtype=complex)
    g[:, list(data_tones)] = points.reshape(n_sym, len(data_tones))
    pv = pilot_values(first_symbol + n_sym, len(pilots))[first_symbol:]
    g[:, list(pilots)] = pv
    return g


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
    hcode = ldpc.code_for(params.HEADER_CODE)
    dcode = ldpc.code_for(params.DATA_CODE)
    rung = cfg.rung

    if header_msg is None:
        header_msg = rng.integers(0, 2, params.HEADER_INFO_BITS - 16, dtype=np.uint8)
    header_bits = append_crc(header_msg[None])[0]
    e_h = params.HEADER_CODE.coded_bits[0]
    hcoded = hcode.rate_match(hcode.encode(header_bits[None])[0], e_h)
    hbits = mapping.interleave(mapping.scramble(hcoded, HEADER_POSITION))
    h_res = cfg.header_symbols * len(cfg.base_data_tones) * QPSK
    hbits = np.concatenate([hbits, _pad_bits(h_res - hbits.size)])
    hpoints = mapping.modulate(hbits, QPSK)

    if payloads is None:
        payloads = rng.integers(0, 2, (cfg.n_blocks, params.DATA_PAYLOAD_BITS), dtype=np.uint8)
    block_bits = append_crc(payloads)
    coded = dcode.rate_match(dcode.encode(block_bits), rung.coded_bits)
    stream = np.concatenate([mapping.interleave(mapping.scramble(coded[i], 1 + i))
                             for i in range(cfg.n_blocks)])
    d_res = cfg.data_symbols * len(cfg.data_tones) * rung.bits_per_symbol
    stream = np.concatenate([stream, _pad_bits(d_res - stream.size)])
    dpoints = mapping.modulate(stream, rung.bits_per_symbol)

    p_base = params.pilots_for(params.BASE_TONES)
    first_h = params.PREAMBLE_SYMBOLS
    hgrid = _fill_symbols(hpoints, cfg.header_symbols, cfg.width, cfg.base_data_tones, p_base, first_h)
    dgrid = _fill_symbols(dpoints, cfg.data_symbols, cfg.width, cfg.data_tones,
                          params.pilots_for(cfg.width), first_h + cfg.header_symbols)
    grid = np.concatenate([preamble.grid(cfg.width), hgrid, dgrid])

    # Every symbol uses the same per-tone scale (SPEC 6.2), set by the link width.
    scale = cfg.width
    segments = [(grid[: first_h + cfg.header_symbols], range(params.BASE_TONES)),
                (dgrid, range(cfg.width))]
    out = []
    for g, tones in segments:
        x = ofdm.modulate(g, cfg.cp, active=scale)
        if cfg.clip:
            x = ofdm.clip_and_filter(x, cfg.width, np.array(list(tones)), cfg.cp, scale_tones=scale)
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
    snr_db: np.ndarray              # per base tone, from S2
    iterations: np.ndarray


def estimate_channel(y2: np.ndarray, y3: np.ndarray, s2: np.ndarray, window: int = 7) -> np.ndarray:
    """Per-tone channel from the two S2 symbols, smoothed across tones.

    The audio chain's response is smooth in frequency apart from a linear
    phase (the delay), so the estimate is de-rotated by its mean phase step,
    smoothed with a local quadratic fit over ``window`` tones, and rotated
    back. ``window`` < 3 returns the raw estimate.
    """
    raw = (y2 + y3) / 2 / s2
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
                s_ref: float, window: int = 15) -> np.ndarray:
    """Phase correction per symbol and tone from the continuous pilots.

    ``pilot_obs`` is (n_sym, n_pilots): received pilot values divided by the
    known pilot symbols. The channel itself is static, so only the change of
    pilot phase over the frame is used: each pilot's phase track gets its own
    intercept (which soaks up channel-estimate error) and the slopes are fit
    as alpha + beta * f, a common phase drift plus the timing drift a
    sample-clock offset causes. The correction is zero at ``s_ref``, the
    symbol position (relative to ``pilot_obs`` row 0) where the channel was
    estimated.
    """
    n_sym = pilot_obs.shape[0]
    kernel = np.ones(window) / window
    sm = np.stack([np.convolve(pilot_obs[:, j], kernel, mode="same") for j in range(len(pilots))], 1)
    theta = np.unwrap(np.angle(sm), axis=0)
    s = np.arange(n_sym, dtype=float)
    sc = s - s.mean()
    slopes = (sc @ (theta - theta.mean(axis=0))) / max(sc @ sc, 1e-12)  # per pilot
    f = params.BIN_OFFSET + np.array(pilots, dtype=float)
    beta, alpha = np.polyfit(f, slopes, 1) if len(pilots) > 1 else (0.0, slopes[0])
    all_f = params.BIN_OFFSET + np.arange(width)
    return np.exp(1j * (alpha + beta * all_f)[None, :] * (s - s_ref)[:, None])


def find_start(rx: np.ndarray, reference: np.ndarray) -> int:
    """Frame start by cross-correlation with the known preamble waveform."""
    corr = signal.correlate(rx, reference, mode="valid", method="fft")
    return int(np.argmax(np.abs(corr)))


def receive(rx: np.ndarray, cfg: FrameConfig, *, reference: np.ndarray | None = None,
            start: int | None = None, advance: int | None = None,
            phase_window: int = 15, smooth_window: int = 7) -> RxResult:
    """Decode one frame from real samples ``rx``.

    Give ``start`` (index of the frame's first sample) or ``reference`` (the
    preamble waveform) to locate it. ``advance`` moves the FFT window that
    many samples into the prefix, to absorb channel delay spread.
    """
    if start is None:
        if reference is None:
            reference = ofdm.modulate(preamble.grid(cfg.width), cfg.cp, active=cfg.width)
        start = find_start(rx, reference)
    advance = cfg.cp // 4 if advance is None else advance
    n_sym = cfg.total_symbols
    tones = ofdm.demodulate(rx, start + cfg.cp - advance, n_sym, cfg.width, cfg.cp, active=cfg.width)

    b = params.BASE_TONES
    y2, y3 = tones[2, :b], tones[3, :b]
    h = np.zeros(cfg.width, dtype=complex)
    h[:b] = estimate_channel(y2, y3, preamble.s2(), smooth_window)
    n0 = estimate_noise(y2, y3)
    n0 = np.maximum(n0, 1e-6 * np.mean(np.abs(h[:b]) ** 2))
    noise = np.full(cfg.width, np.mean(n0))
    noise[:b] = n0
    snr_db = 10 * np.log10(np.abs(h[:b]) ** 2 / n0)

    first = params.PREAMBLE_SYMBOLS
    body = tones[first:]
    pilots = params.pilots_for(cfg.width)
    pv = pilot_values(n_sym, len(pilots))[first:]
    corr = track_drift(body[:, list(pilots)] / pv, pilots, cfg.width,
                       s_ref=2.5 - first, window=phase_window)
    eq = body / (h[None, :] * corr)
    nv = noise[None, :] / np.abs(h[None, :]) ** 2 * np.ones_like(body.real)

    # Header.
    hcode = ldpc.code_for(params.HEADER_CODE)
    hs = cfg.header_symbols
    btones = list(cfg.base_data_tones)
    hl = mapping.demodulate_llr(eq[:hs, btones], nv[:hs, btones], QPSK)
    e_h = params.HEADER_CODE.coded_bits[0]
    hl = mapping.descramble_llr(mapping.deinterleave(hl[:e_h]), HEADER_POSITION)
    hres = ldpc.decode(hcode, hcode.rate_recover(hl, e_h), rows=hcode.rows_needed(hcode.tx_positions(e_h)))

    # Data blocks.
    dcode = ldpc.code_for(params.DATA_CODE)
    rung = cfg.rung
    dt = list(cfg.data_tones)
    dl = mapping.demodulate_llr(eq[hs:, dt], nv[hs:, dt], rung.bits_per_symbol)
    e = rung.coded_bits
    llrs = np.stack([mapping.descramble_llr(mapping.deinterleave(dl[i * e:(i + 1) * e]), 1 + i)
                     for i in range(cfg.n_blocks)])
    dres = ldpc.decode(dcode, dcode.rate_recover(llrs, e), rows=dcode.rows_needed(dcode.tx_positions(e)))
    return RxResult(bool(hres.ok[0]), hres.bits[0], dres.ok, dres.bits, start, snr_db, dres.iterations)
