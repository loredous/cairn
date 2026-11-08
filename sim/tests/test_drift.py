"""Sample-clock drift from the pilots (frame.track_drift, ClockPrior) and the
channel SNR of short frames whose level is still settling (ADR 0006
interpretation level-settling-at-frame-start)."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, params

STRIDE = params.N_USEFUL + params.CP_SHORT


def _ppm(drift: float, stride: int = STRIDE) -> float:
    return -drift / stride * 1e6


def _sd(sd: float, stride: int = STRIDE) -> float:
    return sd / stride * 1e6


def _pilot_obs(n_rows: int, ppm: float, phase_sd: float, rng, pilots=(3, 20)) -> np.ndarray:
    """Pilot values over ``n_rows`` symbols for a clock offset, with phase noise."""
    drift = -ppm * 1e-6 * STRIDE
    beta = -drift * 2 * np.pi / params.N_USEFUL
    f = params.BIN_OFFSET + np.array(pilots, dtype=float)
    s = np.arange(n_rows)[:, None]
    return np.exp(1j * (beta * f[None, :] * s + 0.3 + phase_sd * rng.standard_normal((n_rows, len(pilots)))))


def test_long_run_measures_the_drift():
    rng = np.random.default_rng(1)
    obs = _pilot_obs(320, 80.0, 0.05, rng)
    _, drift, sd = frame.track_drift(obs, (3, 20), 24, s_ref=-1.5)
    assert _ppm(drift) == pytest.approx(80.0, abs=1.5)
    assert _sd(sd) < 1.5


def test_short_run_keeps_the_prior():
    # Seven symbols (a header-only frame) measure the drift to hundreds of ppm:
    # the estimate stays near the prior and says so.
    rng = np.random.default_rng(2)
    worst = 0.0
    for _ in range(50):
        obs = _pilot_obs(7, 0.0, 0.15, rng)
        _, drift, sd = frame.track_drift(obs, (3, 20), 24, s_ref=-1.5)
        worst = max(worst, abs(_ppm(drift)))
        assert _sd(sd) > 40.0
    assert worst < 60.0
    # A link-supplied prior is kept as given.
    prior = frame.ClockPrior(ppm=-20.0, sd_ppm=2.0).drift_units(STRIDE)
    _, drift, sd = frame.track_drift(_pilot_obs(7, -20.0, 0.15, rng), (3, 20), 24, s_ref=-1.5, prior=prior)
    assert _ppm(drift) == pytest.approx(-20.0, abs=2.0) and _sd(sd) <= 2.0


def test_drift_is_bounded_and_one_pilot_measures_none():
    rng = np.random.default_rng(3)
    _, drift, _ = frame.track_drift(_pilot_obs(320, 400.0, 0.01, rng), (3, 20), 24, s_ref=-1.5)
    assert _ppm(drift) == pytest.approx(frame.CLOCK_BOUND_PPM)
    obs = _pilot_obs(320, 80.0, 0.01, rng)
    obs[:, 1] = np.nan
    obs[0, 1] = 1.0                   # one row only: no slope
    prior = frame.ClockPrior(ppm=7.0).drift_units(STRIDE)
    _, drift, sd = frame.track_drift(obs, (3, 20), 24, s_ref=-1.5, prior=prior)
    assert _ppm(drift) == pytest.approx(7.0) and _sd(sd) == pytest.approx(frame.CLOCK_PRIOR_SD_PPM)


def test_upper_tones_follow_their_probe_with_a_clock_offset():
    # Tones above 23 take their channel from the width probe, nine symbols
    # after the S2 pair: the drift correction is zero there, not at S2 (at
    # 100 ppm the difference was 18 degrees at tone 50).
    for ppm in (100.0, -100.0):
        rng = np.random.default_rng(5)
        cfg = frame.FrameConfig(width=56, mcs=4, n_blocks=6)
        tx = frame.build(cfg, rng)
        x = np.concatenate([np.zeros(600), tx.samples, np.zeros(600)])
        y = channel.FmChannel("ideal", snr_db=40.0, sco_ppm=ppm, width=56)(x, rng)
        r = frame.receive(y, cfg, start=600)
        assert r.block_ok.all()
        assert _ppm(r.detail["drift"]) == pytest.approx(ppm, abs=3.0)
        eq, d0 = r.detail["eq"], r.detail["d0"]
        sent = tx.grid[params.PREAMBLE_SYMBOLS:][d0:]
        upper = [k for k in cfg.data_tones if k >= 40]
        err = np.angle(eq[d0:, upper] / sent[:, upper])
        assert abs(np.degrees(np.mean(err))) < 2.0


def _onset(n: int, start: int, g0_db: float = -14.0, tau_s: float = 0.019) -> np.ndarray:
    """The level settling the 2026-11-07 captures show: 1 - (1 - g0) exp(-t / tau)."""
    t = np.maximum(np.arange(n) - start, 0) / params.FS
    return 1 - (1 - 10 ** (g0_db / 20)) * np.exp(-t / tau_s)


@pytest.mark.parametrize("kind", ["header", "control", "one_block"])
def test_short_frame_snr_with_the_level_settling(kind):
    # Signal and noise scaled together by the settling level (as over the
    # air): the short frame reads the channel SNR, not 2-3 dB below it.
    cfg = {"header": frame.FrameConfig(width=24, mcs=0, n_blocks=0),
           "control": frame.FrameConfig(width=24, mcs=0, n_blocks=0, n_control=1),
           "one_block": frame.FrameConfig(width=24, mcs=2, n_blocks=1, n_control=1)}[kind]
    true_db = 20.0
    got = []
    for seed in range(6):
        rng = np.random.default_rng(100 + seed)
        tx = frame.build(cfg, rng)
        last = cfg.first_data if cfg.n_blocks else (cfg.first_control if cfg.n_control else params.PREAMBLE_SYMBOLS)
        p = float(np.mean(tx.samples[last * cfg.stride:] ** 2))
        x = np.concatenate([np.zeros(600), tx.samples, np.zeros(600)])
        y = channel.awgn(x, true_db - (cfg.clip_db - 7.0), 24, rng, signal_power=p) * _onset(x.size, 600)
        r = frame.receive(y, cfg, start=600)
        assert r.header_ok and r.block_ok.all() and np.all(r.control_ok)
        assert not r.detail["snr_fallback"]
        assert abs(_ppm(r.detail["drift"])) < 60.0
        got.append(r.detail["snr_db"])
    assert np.mean(got) == pytest.approx(true_db, abs=1.0)
    assert max(got) <= true_db + 1.5
