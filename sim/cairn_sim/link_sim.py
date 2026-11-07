"""Link-level simulation (SPEC 21.2 ``link_sim``, 14.2 outputs).

- ``bler_point`` / ``bler_curve``: block error rate of a rung against audio
  SNR, through any channel, by building and receiving whole frames.
- ``threshold``: the audio SNR at a target block error rate (1% by default,
  the SPEC 6.2 column), by interpolating log BLER between bracketing points.
- ``Budget``: the SPEC 7.4 / Appendix A time budget: blocks per frame, cycle
  time and net throughput for a rung, width, frame length and TXDelay.
"""

from __future__ import annotations

import math
from collections.abc import Callable
from dataclasses import dataclass, field

import numpy as np

from . import channel, frame, params, preamble

Channel = Callable[[np.ndarray, float, int, np.random.Generator], np.ndarray]


def awgn_channel(x: np.ndarray, snr_db: float, width: int, rng: np.random.Generator) -> np.ndarray:
    return channel.awgn(x, snr_db, width, rng)


@dataclass
class Point:
    snr_db: float
    blocks: int
    block_errors: int
    frames: int
    header_errors: int
    missed_frames: int = 0

    @property
    def bler(self) -> float:
        return self.block_errors / max(self.blocks, 1)

    @property
    def header_fer(self) -> float:
        return self.header_errors / max(self.frames, 1)


def bler_point(mcs: int, snr_db: float, *, width: int = 24, chan: Channel = awgn_channel,
               blocks_per_frame: int = 16, min_blocks: int = 2000, min_errors: int = 100,
               max_blocks: int = 20000, clip_db: float | None = None,
               clip: bool = True, rng: np.random.Generator | None = None,
               guard: int = 400, quick_bler: float = 0.3, quick_blocks: int = 480,
               detector: bool = True, cp: int = params.CP_SHORT,
               mask: frozenset[int] | None = None, rx_kw: dict | None = None,
               **frame_kw) -> Point:
    """Run frames until ``min_blocks`` and ``min_errors`` (or ``max_blocks``) are reached.

    Points with BLER above ``quick_bler`` after ``quick_blocks`` stop early.

    ``clip_db`` defaults to the rung's clip level (``params.clip_db``). The
    SNR axis stays average-referenced: add ``params.clip_offset_db(mcs)``
    to put a result on the 7 dB-clip (peak-limited) scale. ``mask`` is the
    usable-tone set (SPEC 5.4); other ``frame_kw`` go to ``FrameConfig``
    and ``rx_kw`` to ``frame.receive``.

    The frame is found by the SPEC 6.1 detector (``detector=True``) or, for
    comparison, by cross-correlation with the known preamble waveform. A
    missed preamble or a failed header counts every block of the frame as
    an error, as a real receiver could not decode them.
    """
    rng = rng or np.random.default_rng()
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=blocks_per_frame, clip=clip, clip_db=clip_db,
                            cp=cp, mask=mask, **frame_kw)
    pt = Point(snr_db, 0, 0, 0, 0)
    pad = np.zeros(guard)
    while pt.blocks < max_blocks and (pt.blocks < min_blocks or pt.block_errors < min_errors):
        if pt.blocks >= quick_blocks and pt.bler > quick_bler:
            break                        # far above any target: no need for more
        tx = frame.build(cfg, rng)
        rx = chan(np.concatenate([pad, tx.samples, pad]), snr_db, width, rng)
        if detector:
            # The channel delays the frame by up to a few ms; accept the
            # strongest detection within one symbol of the true start.
            dets = sorted((d for d in preamble.detect(rx)
                           if d.cp == cfg.cp and abs(d.start - guard) < cfg.stride),
                          key=lambda d: -d.quality)
            if not dets:
                pt.frames += 1
                pt.missed_frames += 1
                pt.blocks += blocks_per_frame
                pt.block_errors += blocks_per_frame
                continue
            r = frame.receive(rx, cfg, start=dets[0].start, **(rx_kw or {}))
        else:
            r = frame.receive(rx, cfg, reference=tx.preamble_samples, **(rx_kw or {}))
        good = r.block_ok & np.all(r.block_bits == tx.block_bits, axis=1)
        if not r.header_ok:
            good[:] = False
            pt.header_errors += 1
        pt.frames += 1
        pt.blocks += blocks_per_frame
        pt.block_errors += int(np.sum(~good))
    return pt


def bler_curve(mcs: int, snrs: list[float], **kw) -> list[Point]:
    return [bler_point(mcs, s, **kw) for s in snrs]


def interpolate_threshold(points: list[Point], target: float = 0.01) -> float | None:
    """SNR where BLER crosses ``target``, interpolating log10(BLER) linearly."""
    pts = sorted(points, key=lambda p: p.snr_db)
    for lo, hi in zip(pts, pts[1:]):
        if lo.bler >= target >= hi.bler:
            if hi.bler == 0:
                b_hi = 0.5 / hi.blocks          # one-half error: a conservative stand-in
            else:
                b_hi = hi.bler
            if lo.bler == b_hi:
                return lo.snr_db
            f = (math.log10(lo.bler) - math.log10(target)) / (math.log10(lo.bler) - math.log10(b_hi))
            return lo.snr_db + f * (hi.snr_db - lo.snr_db)
    return None


def threshold(mcs: int, *, start_db: float, step_db: float = 1.0, target: float = 0.01,
              max_db: float = 45.0, max_down_steps: int = 8,
              **kw) -> tuple[float | None, list[Point]]:
    """Step up from ``start_db`` until BLER falls below target, then refine at half steps.

    If the first point is already below target, step down (at most
    ``max_down_steps`` times) until one is above it, so the result is
    always bracketed.
    """
    points: list[Point] = []
    snr = start_db
    while snr <= max_db:
        pt = bler_point(mcs, snr, **kw)
        points.append(pt)
        if pt.bler < target:
            break
        snr += step_db
    else:
        return None, points
    if len(points) == 1:                 # started below threshold: step down until above it
        lo = start_db
        for _ in range(max_down_steps):
            lo -= step_db
            points.insert(0, bler_point(mcs, lo, **kw))
            if points[0].bler >= target:
                break
        else:
            return None, points
    mid = points[-1].snr_db - step_db / 2
    points.append(bler_point(mcs, mid, **kw))
    return interpolate_threshold(points, target), sorted(points, key=lambda p: p.snr_db)


# --- time budget (SPEC 7.3, 7.4, Appendix A) ---------------------------------


def symbol_s(cp: int = params.CP_SHORT) -> float:
    return (params.N_USEFUL + cp) / params.FS


@dataclass
class Budget:
    mcs: int
    width: int = 24
    frame_s: float = 4.0
    txdelay_s: float = 0.150
    txtail_s: float = 0.010
    peer_turnaround_s: float = 0.250     # SPEC 7.4: peer turnaround plus its TXDelay
    cp: int = params.CP_SHORT
    mask: frozenset[int] | None = None   # usable tones (SPEC 5.4); None: all
    blocks_override: int | None = field(default=None, repr=False)

    @property
    def coded_bits_per_symbol(self) -> int:
        tones = params.data_tones_for(self.width, params.masked_tones(self.width, self.mask))
        return params.RUNGS[self.mcs].bits_per_symbol * len(tones)

    @property
    def symbols_per_block(self) -> float:
        return params.RUNGS[self.mcs].coded_bits / self.coded_bits_per_symbol

    @property
    def fixed_symbols(self) -> int:
        return params.PREAMBLE_SYMBOLS + 7   # preamble + header

    @property
    def block_symbols_available(self) -> int:
        usable = self.frame_s - self.txdelay_s - self.txtail_s
        return int(usable / symbol_s(self.cp) + 1e-9) - self.fixed_symbols

    @property
    def blocks(self) -> int:
        if self.blocks_override is not None:
            return self.blocks_override
        return int(self.block_symbols_available * self.coded_bits_per_symbol
                   // params.RUNGS[self.mcs].coded_bits)

    @property
    def frame_time_s(self) -> float:
        symbols = self.fixed_symbols + self.blocks * self.symbols_per_block
        return self.txdelay_s + symbols * symbol_s(self.cp) + self.txtail_s

    @property
    def ack_time_s(self) -> float:
        return (params.PREAMBLE_SYMBOLS + 7 + 12) * symbol_s(self.cp)   # preamble, header, 1 control block

    @property
    def cycle_s(self) -> float:
        return self.frame_time_s + self.peer_turnaround_s + self.ack_time_s

    @property
    def net_bps(self) -> float:
        return self.blocks * params.DATA_PAYLOAD_BITS / self.cycle_s

    @property
    def raw_bps(self) -> float:
        rung = params.RUNGS[self.mcs]
        return (params.DATA_PAYLOAD_BITS / rung.coded_bits * self.coded_bits_per_symbol
                / symbol_s(self.cp))


def default_frame_s(mcs: int) -> float:
    """SPEC 10.2 / Appendix A: 2 s frames at rungs 0-1, 4 s otherwise."""
    return 2.0 if mcs <= 1 else 4.0
