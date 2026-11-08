"""The base tones' channel re-estimated from the decoded header
(frame.header_channel_estimate): nine known symbols per tone instead of the
S2 pair's two, each row at its tracked level."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, params

GUARD = 400


def _ramp(n: int, g0_db: float = -30.0, tau_s: float = 0.040) -> np.ndarray:
    """sim/scripts/level_eval.py's capture-like ramp (S2 4.9 dB low)."""
    t = np.maximum(np.arange(n) - GUARD, 0) / params.FS
    return 1 - (1 - 10 ** (g0_db / 20)) * np.exp(-t / tau_s)


def _frames(mcs: int, width: int, snr: float | None, seed: int, n: int, ramp: bool = False):
    """(sent, clean samples, noisy samples, config) for ``n`` frames through white noise,
    with the ramp on signal and noise."""
    rng = np.random.default_rng(seed)
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=4, clip_db=params.clip_db(mcs))
    for _ in range(n):
        tx = frame.build(cfg, rng)
        x = np.concatenate([np.zeros(GUARD), tx.samples, np.zeros(GUARD)])
        y = channel.awgn(x, snr, width, rng, signal_power=float(np.mean(x ** 2)))
        if ramp:
            x, y = x * _ramp(x.size), y * _ramp(y.size)
        yield tx, x, y, cfg


def _error(h: np.ndarray, ref: np.ndarray) -> float:
    b = params.BASE_TONES
    return float(np.mean(np.abs(h[:b] - ref[:b]) ** 2 / np.abs(ref[:b]) ** 2))


@pytest.mark.parametrize("ramp,width", [(False, 24), (True, 24), (False, 32)])
def test_header_lowers_the_channel_estimate_error(ramp, width):
    # Against the estimate of the noise-free frame (same ramp): the header's
    # seven symbols (and the probe's two at 32 tones) with the S2 pair leave
    # well under half the S2 pair's error.
    s2, hdr = [], []
    for tx, x, y, cfg in _frames(2, width, 9.0, 11 + width + ramp, 6, ramp):
        ref = frame.receive(x, cfg, start=GUARD, measure_snr=False).detail["h_s2"]
        r = frame.receive(y, cfg, start=GUARD, measure_snr=False)
        assert r.header_ok and r.detail["header_channel"] is not None
        assert r.detail["header_channel"].rows == (7 if width == 24 else 9)
        s2.append(_error(r.detail["h_s2"], ref))
        hdr.append(_error(r.detail["h"], ref))
    assert np.mean(hdr) < 0.5 * np.mean(s2), (np.mean(hdr), np.mean(s2))


def test_off_and_without_a_header_the_s2_estimate_stands():
    for tx, x, y, cfg in _frames(3, 24, 14.0, 5, 2):
        off = frame.receive(y, cfg, start=GUARD, measure_snr=False, header_channel=False)
        assert off.detail["header_channel"] is None and off.detail["h"] is off.detail["h_s2"]
        on = frame.receive(y, cfg, start=GUARD, measure_snr=False)
        assert np.array_equal(on.detail["h_s2"], off.detail["h_s2"])
        assert np.array_equal(on.detail["h"][24:], off.detail["h"][24:])
        assert on.block_ok.all() and off.block_ok.all()
    # A header that fails leaves nothing known to re-estimate from.
    for tx, x, y, cfg in _frames(0, 24, -6.0, 6, 3):
        r = frame.receive(y, cfg, start=GUARD, measure_snr=False)
        if not r.header_ok:
            assert r.detail["header_channel"] is None
            break
    else:
        pytest.fail("no header failed at -6 dB")


@pytest.mark.parametrize("ramp", [False, True])
def test_data_noise_stays_calibrated(ramp):
    # The header's error is measured on the estimate without each row
    # (h_loo), so the noise the data are demapped with still matches their
    # actual error, though the estimate now contains the header.
    ratios = []
    for tx, x, y, cfg in _frames(3, 24, 12.0, 21 + ramp, 6, ramp):
        r = frame.receive(y, cfg, start=GUARD, measure_snr=False)
        d0, dt = cfg.first_data, [k for k in cfg.data_tones if k < 24]
        rows = slice(d0 - params.PREAMBLE_SYMBOLS, None)
        sent = tx.grid[d0:, dt] / cfg.data_gain
        err = np.mean(np.abs(r.detail["eq"][rows, dt] - sent) ** 2)
        ratios.append(err / np.mean(r.detail["nv"][rows, dt]))
    assert 0.8 < np.mean(ratios) < 1.25, ratios
