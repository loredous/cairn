"""Interleaver evaluation tools (SPEC 6.4) and the burst-aware receiver."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, interleaver_eval as ie, mapping, ofdm, params


@pytest.mark.parametrize("kind", mapping.INTERLEAVERS)
@pytest.mark.parametrize("e", [288, 1232, 1368, 1536, 2048])
def test_interleavers_are_permutations(kind, e):
    p = mapping.interleaver(e, kind, 3)
    assert np.array_equal(np.sort(p), np.arange(e))
    bits = np.random.default_rng(e).integers(0, 2, e, dtype=np.uint8)
    assert np.array_equal(mapping.deinterleave(mapping.interleave(bits, kind, 3), kind, 3), bits)


def test_linear_interleaver_is_unchanged():
    # pi(i) = a i mod E with a the integer nearest 0.618 E coprime to E.
    assert mapping.interleaver(2048)[1] == 1267 and mapping.interleaver(1536)[1] == 949


@pytest.mark.parametrize("kind", mapping.INTERLEAVERS)
def test_frames_loop_back_with_either_interleaver(kind):
    cfg = frame.FrameConfig(width=30, mcs=3, n_blocks=3, interleaver=kind,
                            mask=frozenset(range(30)) - {7, 8})
    tx = frame.build(cfg, np.random.default_rng(2))
    r = frame.receive(tx.samples, cfg, start=0)
    assert r.header_ok and r.block_ok.all()


@pytest.mark.parametrize("mcs", range(7))
def test_layout_places_every_bit_once(mcs):
    rung = params.RUNGS[mcs]
    sym, slot, pos = ie.layout(rung.coded_bits, rung.bits_per_symbol, 19, offset=3 * rung.coded_bits)
    cells = set(zip(sym.tolist(), slot.tolist(), pos.tolist()))
    assert len(cells) == rung.coded_bits


def test_structure_reports_tone_and_symbol_sets():
    s = ie.structure(2, 22)
    assert s["tone"]["bits"] == pytest.approx(2048 / 22, abs=2)
    assert s["symbol"]["bits"] == 88 and s["two_symbols"]["bits"] == 176
    assert s["systematic_share_by_class"] == [0.5, 0.5]      # E is a multiple of 4: exact


def test_dead_tone_removes_exactly_one_tone():
    cfg = frame.FrameConfig(mcs=2, n_blocks=2)
    tx = frame.build(cfg, np.random.default_rng(4))
    x = np.concatenate([np.zeros(400), tx.samples, np.zeros(400)])
    y = ie.DeadTone((9,), cfg.cp)(x, 200.0, 24, np.random.default_rng(0))
    t0 = ofdm.demodulate(x, 400 + cfg.cp, cfg.total_symbols, 24, cfg.cp, active=24)
    t1 = ofdm.demodulate(y, 400 + cfg.cp, cfg.total_symbols, 24, cfg.cp, active=24)
    assert np.max(np.abs(t1[:, 9])) < 1e-6
    others = [k for k in range(24) if k != 9]
    assert np.allclose(t1[:, others], t0[:, others], atol=1e-6)


def test_burst_aware_receiver_discounts_a_burst():
    """A 10 ms burst 20 dB above the signal: with burst_aware the frame
    decodes; the burst-hit symbols get their noise raised; nothing changes
    without a burst."""
    rng = np.random.default_rng(11)
    cfg = frame.FrameConfig(mcs=2, n_blocks=4)
    ok = {False: 0, True: 0}
    for _ in range(6):
        tx = frame.build(cfg, rng)
        x = np.concatenate([np.zeros(400), tx.samples, np.zeros(400)])
        rx = ie.burst_for(cfg)(x, 12.0, 24, rng)
        for aware in (False, True):
            r = frame.receive(rx, cfg, start=400, burst_aware=aware)
            ok[aware] += int(r.block_ok.sum())
    assert ok[True] >= 22 and ok[True] > ok[False]
    # Without a burst the burst-aware receiver decodes the same.
    tx = frame.build(cfg, rng)
    rx = channel.awgn(np.concatenate([np.zeros(400), tx.samples, np.zeros(400)]), 10.0, 24, rng)
    a = frame.receive(rx, cfg, start=400)
    b = frame.receive(rx, cfg, start=400, burst_aware=True)
    assert a.block_ok.all() and b.block_ok.all()


def test_header_noise_sees_a_distorted_tone_the_s2_pair_misses():
    """estimate_header_noise: the S2 floor where the header agrees with it,
    the header's own measurement where it is clearly worse."""
    rng = np.random.default_rng(5)
    hs, b = 7, params.BASE_TONES
    pil = list(params.pilots_for(b))
    data = [k for k in range(b) if k not in pil]
    known = frame.pilots_on(4 + hs, tuple(pil))[4:]
    s = (rng.choice([-1, 1], (hs, b)) + 1j * rng.choice([-1, 1], (hs, b))) / np.sqrt(2)
    s[:, pil] = known
    v = np.full(b, 0.3)
    v[1] = 30.0                                       # distortion the S2 pair did not see
    eq = s + np.sqrt(v / 2) * (rng.standard_normal((hs, b)) + 1j * rng.standard_normal((hs, b)))
    floor = np.full(b, 0.35)                          # what the S2 pair and pilots predict
    nv = frame.estimate_header_noise(eq, known, floor, data, pil)
    assert np.all(nv >= floor - 1e-12)
    assert nv[1] > 5 and nv[0] > 3 and nv[2] > 3      # pooled over three tones
    ratio = nv[5:20] / floor[5:20]                    # where nothing is wrong: mostly the floor
    assert np.median(ratio) == pytest.approx(1.0) and ratio.max() < 2.0


def test_header_noise_floor_is_flat_in_white_noise_and_ignores_pilot_3_slope():
    rng = np.random.default_rng(8)
    b = params.BASE_TONES
    s1, s2 = np.exp(2j * np.pi * rng.random(b)), rng.choice([-1.0, 1.0], b)
    clean = np.stack([s1, s1, s2, s2])
    floors = []
    for _ in range(200):
        n = np.sqrt(0.25) * (rng.standard_normal((4, b)) + 1j * rng.standard_normal((4, b)))
        pre = clean + n
        thermal = frame.estimate_noise(pre[2], pre[3])
        floors.append(frame.header_noise_floor(pre, np.ones(b), thermal, np.array([0.4, 0.0])))
    f = np.mean(floors, axis=0)
    # Noise 0.5 everywhere, plus the pilots' mean excess (0.2) on every tone,
    # not 0.4 sloping down to 0 from tone 3 to tone 20.
    assert np.allclose(f, 0.7, rtol=0.1)
    assert np.std(floors, axis=0).mean() < np.std([frame.estimate_noise(c[2], c[3]) for c in
                                                   [clean + 0.5 * (rng.standard_normal((4, b)) + 1j * rng.standard_normal((4, b)))
                                                    for _ in range(200)]], axis=0).mean()


def test_burst_noise_flags_only_outlying_symbols():
    rng = np.random.default_rng(1)
    eq = (rng.choice([-1, 1], (40, 22)) + 1j * rng.choice([-1, 1], (40, 22))) / np.sqrt(2)
    eq += 0.3 * (rng.standard_normal(eq.shape) + 1j * rng.standard_normal(eq.shape))
    eq[17] += 5 * (rng.standard_normal(22) + 1j * rng.standard_normal(22))
    extra = frame.burst_noise(eq, np.full(eq.shape, 0.18), np.ones(22))
    assert extra[17].mean() > 20 and np.count_nonzero(extra.sum(axis=1)) == 1
