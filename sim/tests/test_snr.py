"""ADR 0006: the receiver's channel SNR (frame.channel_snr), 7 dB-equivalent."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, frame, params
from cairn_sim.link_sim import Budget, default_frame_s


def _frame(mcs, width, rng, mask=None, blocks=None, cp=params.CP_SHORT):
    n = blocks or Budget(mcs, width, default_frame_s(mcs)).blocks
    cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=n, mask=mask, cp=cp)
    return cfg, frame.build(cfg, rng)


def _awgn_snr(mcs, width, true_db, seed, mask=None, blocks=None):
    """Reported SNR for a frame in AWGN at ``true_db`` (7 dB-equivalent)."""
    rng = np.random.default_rng(seed)
    cfg, tx = _frame(mcs, width, rng, mask, blocks)
    y = tx.samples if true_db is None else channel.awgn(tx.samples, true_db - (cfg.clip_db - 7.0), width, rng)
    r = frame.receive(y, cfg, start=0)
    return r, cfg


@pytest.mark.parametrize("mcs,true_db", [(0, 15), (0, 25), (0, 35), (4, 15), (4, 25), (4, 35), (6, 45)])
def test_channel_snr_recovered_in_awgn(mcs, true_db):
    r, cfg = _awgn_snr(mcs, 24, true_db, seed=100 + mcs * 7 + true_db, blocks=6)
    assert r.header_ok
    if r.detail["snr_fallback"]:
        # Below the rung's threshold every data block may fail; the fallback
        # then errs low but never reads more than 1 dB high (ADR 0006).
        assert not r.block_ok.any() and true_db < 17.5
        assert r.detail["snr_db"] <= true_db + 1.0
        return
    if r.block_ok.all():
        assert r.detail["snr_known_symbols"] == r.detail["snr_total_symbols"]
    assert r.detail["snr_db"] == pytest.approx(true_db, abs=1.0)
    assert len(r.detail["snr_per_tone_db"]) == 24
    assert np.all(np.abs(r.detail["snr_per_tone_db"] - true_db) < 6)


def test_clean_frame_reads_at_least_40_db():
    r, _ = _awgn_snr(0, 24, None, seed=1, blocks=3)
    assert r.detail["snr_db"] >= 40.0
    assert r.detail["snr_db"] <= frame.SNR_MAX_DB


def test_failed_blocks_never_over_report():
    # Weak enough that blocks fail: the figure may be low but not more than 1 dB high.
    for mcs, true_db, tries in [(4, 11.0, 4), (6, 18.0, 4), (0, -1.0, 12)]:
        for seed in range(tries):
            r, _ = _awgn_snr(mcs, 24, true_db, seed=200 + seed)
            assert not r.block_ok.all(), (mcs, true_db)
            assert r.detail["snr_db"] <= true_db + 1.0, (mcs, true_db, r.detail["snr_db"], r.detail["snr_fallback"])


def test_too_few_known_symbols_falls_back_and_counts_clipping():
    r, cfg = _awgn_snr(0, 24, -3.0, seed=5)
    assert not r.header_ok and r.detail["snr_fallback"]
    assert r.detail["snr_known_symbols"] < frame.MIN_KNOWN_SYMBOLS
    assert r.detail["snr_db"] <= -3.0 + 1.0
    # A clean frame whose header and blocks are ignored: only the fallback remains, and it reads low.
    rng = np.random.default_rng(6)
    cfg, tx = _frame(0, 24, rng, blocks=3)
    r = frame.receive(tx.samples, cfg, start=0)
    r.header_ok = False
    r.block_ok = np.zeros_like(r.block_ok)
    m = frame.channel_snr(tx.samples, cfg, r)
    assert m.fallback and 15 < m.snr_db < 25     # about 21.7 dB: the designed clipping caps it


def test_saturates_at_field_maximum():
    r, _ = _awgn_snr(0, 24, None, seed=2, blocks=3)
    assert r.detail["snr_db"] <= 63.75 and np.max(r.detail["snr_per_tone_db"]) <= 63.75


def test_repeater_long_prefix_does_not_over_report():
    rng = np.random.default_rng(31)
    cfg, tx = _frame(0, 24, rng, blocks=4, cp=params.CP_LONG)
    x = np.concatenate([np.zeros(500), tx.samples, np.zeros(1000)])
    y = channel.FmChannel("repeater", snr_db=40.0, width=24)(x, rng)
    r = frame.receive(y, cfg, reference=tx.preamble_samples)
    assert r.header_ok and r.block_ok.all() and not r.detail["snr_fallback"]
    assert r.detail["snr_db"] <= 40.0 + 1.0        # dispersion and the radios only lower it


def test_masked_frame():
    mask = frozenset(range(32)) - frozenset({8, 9, 10})
    r, cfg = _awgn_snr(2, 32, 25.0, seed=7, mask=mask, blocks=6)
    assert r.header_ok and r.block_ok.all()
    assert r.detail["snr_db"] == pytest.approx(25.0, abs=1.0)


def test_56_tone_frame():
    r, cfg = _awgn_snr(2, 56, 25.0, seed=8, blocks=6)
    assert r.header_ok and r.block_ok.all() and cfg.has_probe
    assert r.detail["snr_db"] == pytest.approx(25.0, abs=1.0)
    assert len(r.detail["snr_per_tone_db"]) == 56


def test_rebuild_matches_build():
    rng = np.random.default_rng(3)
    cfg, tx = _frame(4, 24, rng, blocks=2)
    again = frame.rebuild(cfg, tx.header_bits, tx.block_bits)
    assert np.array_equal(again.samples, tx.samples)


# --- frames with control blocks and a width probe (SPEC 8.1, 8.2) -------------------

def _vector_frame(name, seed=0):
    """A frame-vector case (``frame_vectors.FRAME_CASES``), clipped as sent on air."""
    from cairn_sim.frame_vectors import FRAME_CASES
    case = next(c for c in FRAME_CASES if c.name == name)
    link = frame.LinkState(case.width, None if case.usable is None else frozenset(case.usable))
    tx = frame.build_frame(case.header, case.controls, link=link, cp=case.cp, rng=np.random.default_rng(seed))
    return link, tx


def test_rebuild_matches_build_with_controls_and_probe():
    link, tx = _vector_frame("data_ack_w30_mcs3_long_masked")
    cfg = tx.config
    assert cfg.n_control == 1 and cfg.has_probe and cfg.mask is not None and cfg.n_blocks == 3
    again = frame.rebuild(cfg, tx.header_bits, tx.block_bits, tx.control_bits)
    assert np.array_equal(again.samples, tx.samples)
    assert np.array_equal(again.grid, tx.grid)
    with pytest.raises(ValueError):
        frame.rebuild(cfg, tx.header_bits, tx.block_bits)          # control bits are needed


@pytest.mark.parametrize("name", ["data_ack_w30_mcs3_long_masked", "ack"])
def test_header_first_channel_snr_with_control_blocks(name):
    link, tx = _vector_frame(name)
    cfg = tx.config
    true_db = 25.0
    # channel.awgn sets the noise by the frame's mean power; reference it to
    # the last segment's (data, else control) so the true per-tone SNR is true_db.
    last = cfg.first_data if cfg.n_blocks else cfg.first_control
    p = float(np.mean(tx.samples[last * cfg.stride:] ** 2))
    rng = np.random.default_rng(41)
    y = channel.awgn(tx.samples, true_db - (cfg.clip_db - 7.0), cfg.width, rng, signal_power=p)
    rx = np.concatenate([np.zeros(700), y, np.zeros(700)])
    r = frame.receive(rx, link=link)
    assert r.header is not None and r.control_ok.all() and r.block_ok.all()
    assert not r.detail["snr_fallback"]
    assert r.detail["snr_known_symbols"] == r.detail["snr_total_symbols"] == cfg.total_symbols
    assert r.detail["snr_db"] == pytest.approx(true_db, abs=1.0)
    assert len(r.detail["snr_per_tone_db"]) == cfg.width


def test_failed_control_block_leaves_its_symbols_unknown():
    cfg = frame.FrameConfig(width=24, mcs=2, n_blocks=1, n_control=3)
    known = frame.known_symbols(cfg, True, np.array([True]), np.array([True, False, True]))
    ct, per_block = len(cfg.control_tones), params.CONTROL_CODE.coded_bits[0] // 2
    rows = np.arange(cfg.control_symbols)
    hit = (rows * ct < 2 * per_block) & ((rows + 1) * ct > per_block)    # symbols holding block 1's points
    assert np.array_equal(known[cfg.first_control:cfg.first_data], ~hit)
    assert known[:cfg.first_control].all() and known[cfg.first_data:].all()
    # No control_ok: no control symbol counts as known.
    assert not frame.known_symbols(cfg, True, np.array([True]))[cfg.first_control:cfg.first_data].any()


def test_control_only_frame_with_failed_controls_falls_back():
    link, tx = _vector_frame("ack")
    r = frame.receive(tx.samples, tx.config, start=0)
    assert r.control_ok.all() and not r.detail["snr_fallback"]
    r.control_ok = np.zeros_like(r.control_ok)
    r.header_ok = False
    m = frame.channel_snr(tx.samples, tx.config, r)
    assert m.fallback and m.known_symbols == params.PREAMBLE_SYMBOLS
