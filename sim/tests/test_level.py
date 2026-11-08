"""Level tracking (frame.track_level): the radios' level ramp at the start of
a frame, which the 2026-11-07 over-the-air captures show leaving the S2
channel estimate 3-6 dB low, and a preamble louder than the data."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, params

GUARD = 400


def _ramp(n: int, g0_db: float = -30.0, tau_s: float = 0.040) -> np.ndarray:
    """The capture-like ramp of sim/scripts/level_eval.py: 1 - (1 - g0) exp(-t / tau)
    from the frame's first sample (S2 4.9 dB low)."""
    t = np.maximum(np.arange(n) - GUARD, 0) / params.FS
    return 1 - (1 - 10 ** (g0_db / 20)) * np.exp(-t / tau_s)


def _run(mcs: int, width: int, snr: float, seed: int, *, ramp: str | None, blocks: int = 4, scale_after=None):
    """A frame through white noise with a ramp on signal and noise ("rx") or
    before the noise ("tx"); ``scale_after`` scales everything after the
    preamble. Returns (tracked, untracked, sent)."""
    rng = np.random.default_rng(seed)
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=blocks, clip_db=params.clip_db(mcs))
    tx = frame.build(cfg, rng)
    x = np.concatenate([np.zeros(GUARD), tx.samples, np.zeros(GUARD)])
    p = float(np.mean(x ** 2))
    if scale_after is not None:
        x[GUARD + params.PREAMBLE_SYMBOLS * cfg.stride:] *= scale_after
    if ramp == "tx":
        x = x * _ramp(x.size)
    y = channel.awgn(x, snr, width, rng, signal_power=p) if snr is not None else x
    if ramp == "rx":
        y = y * _ramp(y.size)
    on = frame.receive(y, cfg, start=GUARD, measure_snr=False)
    off = frame.receive(y, cfg, start=GUARD, measure_snr=False, track_levels=False)
    return on, off, tx


def _good(r: frame.RxResult, tx: frame.TxFrame) -> int:
    return int(np.sum(r.block_ok & np.all(r.block_bits == tx.block_bits, axis=1)))


def _data_gain_db(r: frame.RxResult, tx: frame.TxFrame) -> float:
    """The equalised data symbols' common gain on the sent ones, dB."""
    cfg = r.config
    d0, dt = cfg.first_data, list(cfg.data_tones)
    e = r.detail["eq"][d0 - params.PREAMBLE_SYMBOLS:, dt]
    xs = tx.grid[d0:, dt] / cfg.data_gain
    return float(20 * np.log10(np.sum(np.real(np.conj(xs) * e)) / np.sum(np.abs(xs) ** 2)))


@pytest.mark.parametrize("mcs,snr", [(4, 18.0), (5, 21.0)])
def test_ramp_64qam(mcs, snr):
    # 2 dB above the rung's 1% threshold: with the ramp, the S2 estimate is
    # 4.9 dB low and untracked 64-QAM decodes nothing; tracked, everything.
    for seed in range(3):
        on, off, tx = _run(mcs, 24, snr, 100 * mcs + seed, ramp="rx")
        assert _good(on, tx) == 4, (mcs, seed)
        assert _good(off, tx) == 0, (mcs, seed)
        lv = on.detail["level"]
        assert lv.applied and lv.settle < 0
        assert 20 * np.log10(lv.segments[-1]) == pytest.approx(4.9, abs=0.6)


def test_ramp_before_the_noise_and_a_wide_frame():
    # The ramp ahead of the noise (the S2 pair then also measures the channel
    # 4.9 dB noisier), and at 32 tones, whose tones above 23 take their
    # channel from the probe after the ramp: their level is referenced to it.
    for seed in range(2):
        on, off, tx = _run(4, 24, 20.0, 500 + seed, ramp="tx")
        assert _good(on, tx) == 4 and _good(off, tx) == 0
        on, off, tx = _run(4, 32, 18.0, 600 + seed, ramp="rx")
        assert _good(on, tx) == 4 and _good(off, tx) == 0
        lv = on.detail["level"]
        assert lv.reference == pytest.approx(lv.segments[-1], rel=0.05)


def test_no_ramp_decodes_the_same():
    # Without a level change the tracker holds, or takes out the S2
    # estimate's own common error (a few tenths of a dB at most, which the
    # pilots, the header and the data's power all see): the same blocks
    # decode, and the data symbols sit no further from unit gain than before.
    for mcs, snr in ((4, 16.5), (6, 23.0), (0, 3.5)):
        for seed in range(2):
            on, off, tx = _run(mcs, 24, snr, 700 + 10 * mcs + seed, ramp=None)
            assert _good(on, tx) >= _good(off, tx), (mcs, seed)
            err_on, err_off = abs(_data_gain_db(on, tx)), abs(_data_gain_db(off, tx))
            assert err_on <= max(err_off, 0.1) + 0.05, (mcs, seed, err_on, err_off)


def test_preamble_louder_than_data():
    # The captures without a ramp: the low-crest preamble reads 1-2 dB above
    # the data. A constant step after the preamble is a steady level, no
    # settling term.
    # The data level comes mostly from the data's own power at this SNR, whose
    # 64-QAM points over a 4-block run scatter by about 0.1 dB.
    resid = []
    for seed in range(900, 906):
        on, off, tx = _run(5, 24, 24.0, seed, ramp=None, scale_after=10 ** (-1.5 / 20))
        lv = on.detail["level"]
        assert lv.applied and lv.settle == 0.0
        assert _data_gain_db(off, tx) == pytest.approx(-1.5, abs=0.3)
        assert 20 * np.log10(lv.segments[0]) == pytest.approx(-1.5, abs=0.3)
        resid.append(_data_gain_db(on, tx))
        assert _good(on, tx) == 4
    assert np.max(np.abs(resid)) < 0.25 and np.sqrt(np.mean(np.square(resid))) < 0.15


def test_noisy_frame_falls_back():
    # Below the header's threshold nothing is known but the pilots: a level
    # change too small to tell from their noise is not applied.
    held = 0
    for seed in range(6):
        on, _, _ = _run(0, 24, -2.0, 1000 + seed, ramp=None, blocks=1)
        lv = on.detail["level"]
        held += not lv.applied
        if lv.applied:
            assert np.all(np.abs(20 * np.log10(lv.level)) < 3.0)
    assert held >= 4


def test_level_fit_is_exact_on_a_clean_ramp():
    # Noise-free, the fitted level follows the ramp the S2 estimate missed:
    # the data rows equalise to unit gain (16 blocks: on a short 256-QAM run
    # the data's own power scatters by a tenth of a dB, which the fit counts).
    on, off, tx = _run(6, 24, None, 1100, ramp="rx", blocks=16)
    assert _data_gain_db(off, tx) > 4.0
    assert _data_gain_db(on, tx) == pytest.approx(0.0, abs=0.1)
    assert _good(on, tx) == 16
