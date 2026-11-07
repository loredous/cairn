"""Interleaver evaluation (SPEC 6.4): structure under tone masks, and BLER
with a dead tone or a short noise burst.

SPEC 6.4 asks that each block's coded bits be spread across all active
tones and the symbols the block occupies, so a dead tone or a short burst
costs scattered bits rather than a run, and that the interleaver be defined
for every tone mask. Two measurements:

* ``structure``: where the coded bits of one block land. The bit-to-tone
  order fills the active data tones of each symbol in order, so the layout
  depends on a mask only through the number of active data tones ``D`` and
  on where the block starts in the stream. For each tone, and each run of
  one or two whole symbols, it counts the parity checks of the LDPC code
  that see two or more of the affected bits (a check with two unreliable
  inputs passes almost nothing), the smallest spacing between affected
  positions in codeword order, and how systematic bits are shared among
  the QAM bit positions (reliability classes).
* ``bler``: whole frames through AWGN with one tone dead in the channel but
  not masked (the receiver has only its channel and noise estimates to go
  on), or with a ~10 ms noise burst 20 dB above the signal in the data.

Both compare the provisional linear interleaver with a uniformly random
permutation, the idealised reference.
"""

from __future__ import annotations

from dataclasses import dataclass
from functools import cache

import numpy as np
from scipy import sparse

from . import channel, frame, ldpc, link_sim, mapping, ofdm, params

DATA = params.DATA_CODE


@cache
def _checks(e: int) -> sparse.csc_matrix:
    """Parity checks used when E bits are sent, against rate-matched position."""
    code = ldpc.code_for(DATA)
    rows = code.rows_needed(code.tx_positions(e))
    h = code.parity_check_matrix()
    z = code.z
    lifted = np.concatenate([np.arange(r * z, (r + 1) * z) for r in rows])
    return sparse.csc_matrix(h[np.ix_(lifted, code.tx_positions(e))].astype(np.int32))


def layout(e: int, bps: int, n_data: int, kind: str = "linear", key: int = 1,
           offset: int = 0) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """(symbol, tone slot, bit position in the QAM point) of each rate-matched bit.

    ``offset`` is the block's first bit in the frame's data stream; symbols
    are counted from the one that holds stream bit 0.
    """
    s = offset + mapping.interleaver(e, kind, key)
    point = s // bps
    return point // n_data, point % n_data, s % bps


@dataclass
class SetStats:
    bits: int
    checks_2plus: float      # fraction of used parity checks seeing >= 2 of the bits
    max_per_check: int
    min_gap: int             # smallest circular spacing in codeword (rate-matched) order


def set_stats(e: int, bad: np.ndarray) -> SetStats:
    hits = np.asarray(_checks(e)[:, np.flatnonzero(bad)].sum(axis=1)).ravel()
    idx = np.flatnonzero(bad)
    gaps = np.diff(np.concatenate([idx, idx[:1] + e])) if idx.size > 1 else np.array([e])
    return SetStats(int(idx.size), float(np.mean(hits >= 2)), int(hits.max(initial=0)), int(gaps.min()))


def structure(mcs: int, n_data: int, kind: str = "linear", key: int = 1, offset: int = 0) -> dict:
    """Worst and mean statistics over every tone and every one- or two-symbol run."""
    rung = params.RUNGS[mcs]
    e, bps = rung.coded_bits, rung.bits_per_symbol
    sym, slot, pos = layout(e, bps, n_data, kind, key, offset)
    out: dict = {}
    tone_sets = [slot == t for t in range(n_data)]
    syms = np.unique(sym)
    whole = [s for s in syms if np.sum(sym == s) == bps * n_data]        # symbols the block fills
    one = [sym == s for s in whole]
    two = [(sym == s) | (sym == s + 1) for s in whole if s + 1 in whole]
    for name, sets in (("tone", tone_sets), ("symbol", one), ("two_symbols", two)):
        if not sets:
            continue
        st = [set_stats(e, b) for b in sets]
        out[name] = {"bits": int(np.mean([x.bits for x in st])),
                     "checks_2plus_mean": float(np.mean([x.checks_2plus for x in st])),
                     "checks_2plus_worst": float(np.max([x.checks_2plus for x in st])),
                     "max_per_check": int(np.max([x.max_per_check for x in st])),
                     "min_gap": int(np.min([x.min_gap for x in st]))}
    # Reliability classes: share of the systematic bits in each class
    # (class 0 = the most reliable pair of bits of the QAM point).
    code = ldpc.code_for(DATA)
    systematic = code.tx_positions(e) < code.k
    cls = pos // 2
    n_cls = bps // 2
    share = np.array([np.mean(cls[systematic] == c) for c in range(n_cls)])
    out["systematic_share_by_class"] = share.round(3).tolist()
    return out


def structure_table(n_masks: int = 20, seed: int = 0, kinds=("linear", "random")) -> list[dict]:
    """``structure`` for random masks (widths 24-56, up to a third of the tones off)."""
    rng = np.random.default_rng(seed)
    rows = []
    for _ in range(n_masks):
        width = int(rng.integers(params.BASE_TONES, params.MAX_TONES + 1))
        free = [k for k in range(width) if k not in params.UNMASKABLE_TONES]
        off = set(rng.choice(free, int(rng.integers(0, width // 3 + 1)), replace=False).tolist())
        mask = frozenset(range(width)) - off
        n_data = len(params.data_tones_for(width, params.masked_tones(width, mask)))
        for mcs in range(len(params.RUNGS)):
            e = params.RUNGS[mcs].coded_bits
            block = int(rng.integers(0, 8))
            for kind in kinds:
                rows.append({"width": width, "masked": len(off), "n_data": n_data, "mcs": mcs,
                             "kind": kind, "block": block,
                             **structure(mcs, n_data, kind, key=1 + block, offset=block * e)})
    return rows


# --- impairments --------------------------------------------------------------


@dataclass
class DeadTone:
    """AWGN channel with one tone removed from every symbol of the frame.

    The tone's component is subtracted symbol by symbol (the frame starts
    ``guard`` samples into the input), so the tone is gone from preamble to
    data while its neighbours are untouched; the noise is set from the
    signal power before removal, so the SNR axis is the unimpaired one.
    The tone is drawn from ``tones`` for every frame.
    """
    tones: tuple[int, ...]
    cp: int = params.CP_SHORT
    guard: int = 400

    def __call__(self, x: np.ndarray, snr_db: float, width: int, rng: np.random.Generator) -> np.ndarray:
        stride = params.N_USEFUL + self.cp
        n_sym = (x.size - 2 * self.guard) // stride
        tone = int(rng.choice(self.tones))
        t = ofdm.demodulate(x, self.guard + self.cp, n_sym, tone + 1, self.cp, active=width)
        g = np.zeros((n_sym, tone + 1), dtype=complex)
        g[:, tone] = t[:, tone]
        y = x.copy()
        y[self.guard:self.guard + n_sym * stride] -= ofdm.modulate(g, self.cp, active=width)
        return channel.awgn(y, snr_db, width, rng, signal_power=float(np.mean(x ** 2)))


@dataclass
class Burst:
    """AWGN plus one burst of white noise ``excess_db`` above the signal's
    in-band power, ``length`` samples long, at a uniformly random place in
    the data symbols (the frame starts ``guard`` samples in)."""
    data_start: int          # sample offset of the first data symbol within the frame
    data_end: int
    length: int = 480        # 10 ms
    excess_db: float = 20.0
    guard: int = 400

    def __call__(self, x: np.ndarray, snr_db: float, width: int, rng: np.random.Generator) -> np.ndarray:
        p_sig = float(np.mean(x ** 2))
        y = channel.awgn(x, snr_db, width, rng, signal_power=p_sig)
        lo, hi = channel.occupied_band(width)
        var = p_sig * 10 ** (self.excess_db / 10) * (params.FS / 2) / (hi - lo)
        at = self.guard + int(rng.integers(self.data_start, self.data_end - self.length + 1))
        y[at:at + self.length] += rng.standard_normal(self.length) * np.sqrt(var)
        return y


def burst_for(cfg: frame.FrameConfig, **kw) -> Burst:
    d0 = (params.PREAMBLE_SYMBOLS + cfg.header_symbols + cfg.probe_symbols) * cfg.stride
    return Burst(d0, cfg.n_samples, **kw)


def bler(mcs: int, snr_db: float, *, width: int, impairment: str, kind: str, n_blocks: int = 2,
         rng: np.random.Generator, **kw) -> link_sim.Point:
    """BLER of rung ``mcs`` with an impairment: "none", "dead_tone" (a random
    data tone per frame) or "burst". ``kw`` goes to ``link_sim.bler_point``."""
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=n_blocks, interleaver=kind)
    if impairment == "none":
        chan = link_sim.awgn_channel
    elif impairment == "dead_tone":
        chan = DeadTone(cfg.data_tones, cfg.cp)
    elif impairment == "burst":
        chan = burst_for(cfg)
    else:
        raise ValueError(impairment)
    return link_sim.bler_point(mcs, snr_db, width=width, chan=chan, blocks_per_frame=n_blocks,
                               rng=rng, interleaver=kind, **kw)
