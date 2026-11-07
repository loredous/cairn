"""Preamble sequences and detector (SPEC 6.1, 21.2 ``preamble`` row).

Fast tests check the sequences, the front end and the detector's mechanics;
``slow`` tests measure the acceptance targets: >= 99% detection at 6 dB,
<= 1 false alarm per 10 minutes of open-squelch noise, prefix identified
>= 99.9%, and no detections on steady tones.
"""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, mapping, ofdm, params, preamble
from cairn_sim import preamble_eval as ev

CPS = (params.CP_SHORT, params.CP_LONG)


# --- sequences ------------------------------------------------------------------


def test_s1_is_constant_amplitude_and_low_papr():
    s1 = preamble.s1()
    assert s1.shape == (24,)
    assert np.allclose(np.abs(s1), 1)
    for cp in CPS:
        assert preamble.symbol_papr_db(s1, cp) <= 3.0
    assert preamble.symbol_papr_db(np.exp(1j * preamble.newman_phases()), 60) > 4.0


def test_symbol_papr_matches_the_modulator():
    for values in (preamble.s1(), preamble.s2()):
        for cp in CPS:
            x = ofdm.modulate(values[None, :], cp)
            assert np.isclose(ofdm.papr_db(x, cp)[0], preamble.symbol_papr_db(values, cp))


def test_s2_is_the_published_gold_segment():
    s2 = preamble.s2()
    assert np.array_equal(s2, 1 - 2.0 * mapping.gold_sequence(preamble.S2_C_INIT, 24))
    assert preamble.cross_correlation_peak(preamble.s1(), s2) <= preamble.S2_MAX_XCORR
    assert preamble.worst_papr_db(s2) < 5.5


def test_vectorized_gold_matches_reference():
    inits = np.array([0, 1, 5237, 0x5332, 65535])
    g = preamble._gold_all(24, inits)
    for row, c in zip(g, inits):
        assert np.array_equal(row, mapping.gold_sequence(int(c), 24))


def test_grid_layout():
    g = preamble.grid(24)
    assert np.array_equal(g[0], g[1]) and np.array_equal(g[2], g[3])
    assert np.allclose(g[0], preamble.s1()) and np.allclose(g[2], preamble.s2())


def test_preamble_repeats_at_the_symbol_lag():
    for cp in CPS:
        x = ofdm.modulate(preamble.grid(24), cp)
        lag = params.N_USEFUL + cp
        assert np.allclose(x[:lag], x[lag:2 * lag])


# --- front end and detector mechanics -------------------------------------------


def test_front_end_is_aligned_analytic_and_band_limited():
    n = np.arange(48_000)
    z = preamble.analytic_decimate(np.cos(2 * np.pi * 1234.5 * n / params.FS + 0.3))
    m = np.arange(z.size)
    want = np.exp(1j * (2 * np.pi * 1234.5 * 6 * m / params.FS + 0.3))
    assert np.max(np.abs(z[200:-200] - want[200:-200])) < 1e-4
    for f in (0.0, 60.0, 3500.0, 5000.0):
        out = preamble.analytic_decimate(np.cos(2 * np.pi * f * n / params.FS))
        assert np.sqrt(np.mean(np.abs(out[200:-200]) ** 2)) < 1e-3


@pytest.mark.parametrize("cp", CPS)
@pytest.mark.parametrize("lead", [3001, 3004, 7777])
def test_clean_frame_detected_at_exact_start(cp, lead):
    rng = np.random.default_rng(lead)
    cfg = frame.FrameConfig(cp=cp)
    tx = frame.build(cfg, rng)
    rx = np.concatenate([np.zeros(lead), tx.samples, np.zeros(2000)])
    dets = preamble.detect(rx)
    assert len(dets) == 1
    d = dets[0]
    assert d.start == lead and d.cp == cp
    assert d.quality > 0.95 and d.autocorr > 0.95


@pytest.mark.parametrize("cp", CPS)
@pytest.mark.parametrize("ppm", [-100.0, 100.0])
def test_receive_decodes_from_detected_start(cp, ppm):
    r = ev.decode_run(10.0, cp, ppm, 2, np.random.default_rng(int(cp + ppm + 1000)))
    assert r.detected == 2 and r.header_ok == 2 and r.blocks_ok == 2


def test_streaming_matches_one_shot_across_chunk_boundaries():
    rng = np.random.default_rng(3)
    tx = frame.build(frame.FrameConfig(), rng).samples
    rx = np.zeros(200_000)
    starts = [10_000, 70_000, 131_000]
    for s in starts:
        rx[s:s + tx.size] += tx
    rx = channel.awgn(rx, 10.0, 24, rng, signal_power=float(np.mean(tx ** 2)))
    one = preamble.scan(rx)
    det = preamble.Detector()
    streamed = []
    for i in range(0, rx.size, 65_536):             # boundaries fall inside frames
        streamed += det.process(rx[i:i + 65_536])
    streamed += det.flush()
    assert [d.start for d in one] == starts
    assert [(d.start, d.cp) for d in streamed] == [(d.start, d.cp) for d in one]


@pytest.mark.parametrize("cp", CPS)
def test_detection_at_6db_small(cp):
    r = ev.detection_run(6.0, cp, 100.0, 40, np.random.default_rng(cp))
    assert r.detected == r.n_frames and r.prefix_correct == r.detected
    assert r.false_alarms == 0
    assert r.max_abs_timing_err <= 4


@pytest.mark.parametrize("name", ev.INTERFERERS)
def test_steady_tones_do_not_trigger(name):
    r = ev.false_alarm_run(0.25, None, name)
    assert r.false_alarms == 0
    assert r.max_score < 0.35


def test_noise_does_not_trigger_short():
    for noise in ("white", "tilted"):
        r = ev.false_alarm_run(0.5, noise, seed=1)
        assert r.false_alarms == 0


def test_silence_does_not_trigger():
    assert preamble.detect(np.zeros(100_000)) == []


# --- acceptance (slow) ----------------------------------------------------------


@pytest.mark.slow
def test_s2_search_reproduces_choice():
    c_init, papr, xc = preamble.search_s2()
    assert c_init == preamble.S2_C_INIT
    assert np.isclose(papr, preamble.worst_papr_db(preamble.s2()))


@pytest.mark.slow
def test_s1_search_reproduces_papr():
    _, papr = preamble.search_s1()
    assert papr <= preamble.worst_papr_db(preamble.s1()) + 0.05


@pytest.mark.slow
@pytest.mark.parametrize("cp", CPS)
@pytest.mark.parametrize("ppm", [-100.0, 0.0, 100.0])
def test_detection_and_prefix_at_6db(cp, ppm):
    r = ev.detection_run(6.0, cp, ppm, 1500, np.random.default_rng(int(10 * cp + ppm + 1000)))
    assert r.p_detect >= 0.99
    assert r.prefix_rate >= 0.999
    assert r.false_alarms == 0


@pytest.mark.slow
@pytest.mark.parametrize("cp", CPS)
def test_detection_at_6db_through_repeater_chain(cp):
    r = ev.detection_run(6.0, cp, 100.0, 500, np.random.default_rng(cp + 1),
                         audio_filter=ev.repeater_chain)
    assert r.p_detect >= 0.99 and r.prefix_rate >= 0.999


@pytest.mark.slow
@pytest.mark.parametrize("noise", ["white", "tilted"])
def test_false_alarms_10_minutes(noise):
    r = ev.false_alarm_run(10.0, noise, seed=21)
    assert r.false_alarms <= 1


@pytest.mark.slow
@pytest.mark.parametrize("name", ev.INTERFERERS)
@pytest.mark.parametrize("noise,tnr", [(None, None), ("white", 0.0), ("white", 6.0),
                                       ("tilted", 0.0), ("tilted", 6.0), ("white", 20.0)])
def test_steady_tones_with_noise(name, noise, tnr):
    r = ev.false_alarm_run(2.0, noise, name, tnr, seed=7)
    assert r.false_alarms == 0


@pytest.mark.slow
@pytest.mark.parametrize("cp", CPS)
def test_decode_from_detected_start_at_8db(cp):
    r = ev.decode_run(8.0, cp, 100.0, 20, np.random.default_rng(cp + 2))
    assert r.detected == 20
    assert r.header_ok >= 19
