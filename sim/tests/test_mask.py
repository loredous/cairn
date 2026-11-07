"""Tone masks (SPEC 5.4): data symbols only, constant power, clip-and-filter projection."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, ofdm, params
from cairn_sim.link_sim import Budget

# (width, tones switched off): low and high edges, interior runs, masked pilots.
MASKS = [
    (24, {0, 1}),
    (24, {5, 6, 13, 23}),
    (30, {10, 11, 12, 28, 29}),
    (41, {37, 38, 39, 40}),                       # pilot 37 masked
    (56, {0, 37, 52, 53, 54, 55}),                # both upper pilots masked
    (56, set(range(24, 56))),                     # a 24-tone link probed at 56
]


def usable(width: int, off: set[int]) -> frozenset[int]:
    return frozenset(range(width)) - off


def _data_start(cfg: frame.FrameConfig) -> int:
    return params.PREAMBLE_SYMBOLS + cfg.header_symbols + cfg.probe_symbols


@pytest.mark.parametrize("width,off", MASKS)
def test_masked_frame_loops_back(width, off):
    rng = np.random.default_rng(width + len(off))
    for mcs in (0, 2, 4, 5):
        cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=2, mask=usable(width, off))
        assert set(cfg.data_tones).isdisjoint(off) and set(cfg.data_pilots).isdisjoint(off)
        tx = frame.build(cfg, rng)
        r = frame.receive(tx.samples, cfg, start=0)
        assert r.header_ok and r.block_ok.all(), (width, mcs)
        assert np.array_equal(r.block_bits, tx.block_bits)


@pytest.mark.parametrize("width,off", [MASKS[1], MASKS[4]])
def test_masked_frame_decodes_in_noise(width, off):
    rng = np.random.default_rng(3)
    cfg = frame.FrameConfig(width=width, mcs=2, n_blocks=4, mask=usable(width, off))
    tx = frame.build(cfg, rng)
    rx = channel.awgn(np.concatenate([np.zeros(300), tx.samples, np.zeros(300)]), 14.0, width, rng)
    r = frame.receive(rx, cfg, reference=tx.preamble_samples)
    assert r.header_ok and r.block_ok.all()


def test_preamble_header_and_probe_ignore_the_mask():
    off = {0, 5, 6, 30, 37}
    a = frame.build(frame.FrameConfig(width=40, mcs=3, n_blocks=2), np.random.default_rng(5))
    cfg = frame.FrameConfig(width=40, mcs=3, n_blocks=2, mask=usable(40, off))
    b = frame.build(cfg, np.random.default_rng(5))
    d0 = _data_start(cfg)
    assert np.array_equal(a.grid[:d0], b.grid[:d0])
    assert np.array_equal(a.samples[:d0 * cfg.stride], b.samples[:d0 * cfg.stride])
    assert np.all(b.grid[d0:, sorted(off)] == 0)


@pytest.mark.parametrize("width,off", MASKS)
def test_symbol_power_is_unchanged(width, off):
    cfg = frame.FrameConfig(width=width, mcs=3, n_blocks=3, mask=usable(width, off))
    tx = frame.build(cfg, np.random.default_rng(1))
    d0 = _data_start(cfg)
    per_symbol = np.sum(np.abs(tx.grid[d0:]) ** 2, axis=1)
    # Unit-power data points and pilots on the active tones, raised by data_gain.
    assert cfg.data_gain ** 2 * len(cfg.data_active_tones) == pytest.approx(width)
    assert per_symbol.mean() == pytest.approx(width, rel=0.03)
    # The same on air, after clip-and-filter, against an unmasked frame.
    ref_cfg = frame.FrameConfig(width=width, mcs=3, n_blocks=3)
    ref = frame.build(ref_cfg, np.random.default_rng(1))
    p = np.mean(tx.samples.reshape(-1, cfg.stride)[d0:] ** 2)
    p_ref = np.mean(ref.samples.reshape(-1, cfg.stride)[_data_start(ref_cfg):] ** 2)
    assert 10 * np.log10(p / p_ref) == pytest.approx(0, abs=0.3)


def test_masked_tones_after_clip_and_filter():
    """Clip-and-filter projects data symbols onto the unmasked tones only.

    Without clipping a masked tone is exactly silent. With it, the only
    energy left on a masked tone is the regrowth of SPEC 7.1's final clip,
    which lands equally on every tone outside the kept set: masked tones sit
    no higher than the band edge above the top tone, at -30 dB for a 7 dB
    clip and >= 40 dB down from 9 dB (MCS 5) up.
    """
    width, off = 30, {5, 6, 13, 25}
    rng = np.random.default_rng(1)
    expect = {0: 28.0, 2: 28.0, 4: 33.0, 5: 40.0, 6: 40.0}
    for mcs, floor_db in expect.items():
        for clip in (False, True):
            cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=12, mask=usable(width, off), clip=clip)
            tx = frame.build(cfg, rng)
            t = ofdm.demodulate(tx.samples, cfg.cp, cfg.total_symbols, width + 6, cfg.cp, active=width)
            p = np.abs(t[_data_start(cfg):]) ** 2
            act = p[:, list(cfg.data_active_tones)].mean()
            masked = p[:, sorted(off)].mean(axis=0) / act
            if not clip:
                assert masked.max() < 1e-12
                continue
            edge = p[:, width:].mean() / act
            assert 10 * np.log10(masked.max()) < -floor_db, mcs
            assert 10 * np.log10(masked.max() / edge) < 1.5, mcs


def test_pilots_3_and_20_cannot_be_masked():
    for pilot in (3, 20):
        with pytest.raises(ValueError):
            frame.FrameConfig(width=30, mask=usable(30, {pilot}))
    frame.FrameConfig(width=56, mask=usable(56, {37, 52}))          # these may be
    with pytest.raises(ValueError):
        frame.FrameConfig(width=24, mask=frozenset({3, 20}))         # no data tone left
    # A 56-bit mask report applies at any width: tones above the width are ignored.
    assert frame.FrameConfig(width=24, mask=frozenset(range(56))).masked == frozenset()


def test_masked_pilot_is_absent_and_drift_still_tracked():
    cfg = frame.FrameConfig(width=56, mcs=2, n_blocks=30, mask=usable(56, {37, 52}))   # 3.3 s: 13 samples of drift
    assert cfg.data_pilots == (3, 20)
    rng = np.random.default_rng(9)
    tx = frame.build(cfg, rng)
    rx = channel.resample(np.concatenate([np.zeros(200), tx.samples, np.zeros(400)]), 1 + 80e-6)
    r = frame.receive(rx, cfg, reference=tx.preamble_samples)
    assert r.header_ok and r.block_ok.all()


@pytest.mark.parametrize("width,off", MASKS)
def test_budget_counts_fewer_bits_per_symbol(width, off):
    full = Budget(3, width)
    masked = Budget(3, width, mask=usable(width, off))
    n_data_off = len([k for k in off if k not in params.pilots_for(width)])
    assert full.coded_bits_per_symbol - masked.coded_bits_per_symbol == 4 * n_data_off
    assert masked.blocks <= full.blocks
    cfg = frame.FrameConfig(width=width, mcs=3, n_blocks=masked.blocks, mask=usable(width, off))
    assert cfg.data_symbols <= masked.block_symbols_available
