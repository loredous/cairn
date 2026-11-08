"""ADR 0006: the receiver's channel SNR (frame.channel_snr), 7 dB-equivalent."""

from __future__ import annotations

import numpy as np
import pytest

from scipy import signal as ss

from cairn_sim import channel, frame, ofdm, params
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
    # About 25 dB: the designed clipping caps it (the level tracker takes the
    # clipped symbols' lower in-band level out of the header's error, which
    # without it read 21.7 dB).
    assert m.fallback and 15 < m.snr_db < 28


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
    link = frame.LinkState(case.width, None if case.usable is None else frozenset(case.usable),
                           mask_id=case.header.mask_id)
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


@pytest.mark.parametrize("n_blocks", [4, 0])
def test_per_tone_snr_on_probed_tones_next_to_bad_tones(n_blocks):
    # A 32-tone frame with its width probe; tones 0-2 swamped by interferers
    # (0 dB there). Every probed tone above 23 is measured (finite, near the
    # channel SNR), even in a header-only frame where only its two probe
    # symbols cover it, and tones 3-4 next to the bad ones read their own
    # SNR: mask decisions use these values (the decoder's smoothed estimate
    # spills the bad tones over their neighbours).
    true_db = 20.0
    rng = np.random.default_rng(77)
    cfg = frame.FrameConfig(width=32, mcs=0, n_blocks=n_blocks, probe=True)
    tx = frame.build(cfg, rng)
    last = cfg.first_data if n_blocks else cfg.first_probe
    p = float(np.mean(tx.samples[last * cfg.stride:] ** 2))
    x = np.concatenate([np.zeros(600), tx.samples, np.zeros(600)])
    y = channel.awgn(x, true_db, cfg.width, rng, signal_power=p)
    t = np.arange(y.size) / params.FS
    for k in (0, 1, 2):
        y = y + np.sqrt(2 * p / cfg.width) * np.cos(2 * np.pi * params.tone_freq(k) * t + rng.uniform(0, 2 * np.pi))
    r = frame.receive(y, cfg, start=600)
    assert r.header_ok and r.block_ok.all() and not r.detail["snr_fallback"]
    per = r.detail["snr_per_tone_db"]
    assert np.all(np.isfinite(per)) and per.size == 32
    assert np.all(per[:3] < 6.0), per[:3]
    # A header-only frame measures each base tone on 9 symbols (+-1.7 dB).
    assert np.all(np.abs(per[3:5] - true_db) < (2.5 if n_blocks else 5.0)), per[3:5]
    upper = per[24:]
    assert np.all(np.abs(upper - true_db) < (3.0 if n_blocks else 6.0)), upper


def _kept_db(sig: np.ndarray, noise: np.ndarray, tones, off: float = 0.0) -> float:
    """10 log10(sum signal / sum noise) over the measured tones among ``tones``, plus ``off``."""
    t = [k for k in tones if k < noise.size and noise[k] > 0]
    return float(10 * np.log10(np.sum(sig[t]) / np.sum(noise[t])) + off)


@pytest.mark.parametrize("fallback", [False, True])
def test_tone_powers_sum_to_the_channel_snr(fallback):
    # The per-tone powers behind the channel SNR, which the link layer sums
    # over the tones a mask keeps (ChannelSnr tone_signal / tone_noise, as in
    # the Rust receiver): over the data's active tones they give it back.
    rng = np.random.default_rng(31)
    cfg, tx = _frame(2, 32, rng, mask=frozenset(range(3, 32)), blocks=4)
    y = channel.awgn(tx.samples, 25.0 - (cfg.clip_db - 7.0), 32, rng)
    r = frame.receive(y, cfg, start=0)
    assert r.header_ok and r.block_ok.all()
    if fallback:
        r.block_ok = np.zeros_like(r.block_ok)
    m = frame.channel_snr(y, cfg, r)
    assert m.fallback == fallback
    off = cfg.clip_db - 7.0
    assert _kept_db(m.tone_signal, m.tone_noise, cfg.data_active_tones, off) == pytest.approx(m.snr_db, abs=1e-9)
    assert np.all(m.tone_signal >= 0) and np.all(m.tone_noise >= 0)
    if not fallback:
        assert np.array_equal(r.detail["snr_tone_signal"] > 0, r.detail["snr_tone_noise"] > 0)


# A radio's CTCSS high-pass, steep enough that tones 0-2 (350-550 Hz) arrive
# 1.5-3 ms after mid-band, past the 1.25 ms prefix, as on the 2026-11-08 1.25 m
# radios (their tones 0-2 were only 1-3 dB down but read -17 to 9 dB).
LOW_CUT = dict(N=6, Wn=330.0, btype="highpass", fs=params.FS, output="sos")
DISPERSIVE_FRAMES = {
    "connect": dict(width=24, mcs=0, n_blocks=0, n_control=2, cap=32, probe=True),
    "ui1": dict(width=24, mcs=2, n_blocks=1, n_control=1),
    "long4": dict(width=32, mcs=4, n_blocks=8, n_control=1),
    "long4_masked": dict(width=32, mcs=4, n_blocks=8, n_control=1, mask=frozenset(range(3, 32))),
}


def _dispersive(kind: str, seed: int, snr_db: float = 30.0) -> tuple[float, float]:
    """(estimated, true) channel SNR over tones 3-23 of a frame through LOW_CUT and white noise.

    The truth is taken over what the estimator fits (the known rows after the
    preamble, the cells that carry something): the noise-free per-tone gain's
    power against the noise-free residual (the high-pass's ISI) plus the noise.
    """
    rng = np.random.default_rng(seed)
    cfg = frame.FrameConfig(**DISPERSIVE_FRAMES[kind])
    tx = frame.build(cfg, rng)
    sos = ss.butter(**LOW_CUT)
    d = int(round(ss.group_delay(ss.sos2tf(sos), [1500.0], fs=params.FS)[1][0]))
    guard = 1000
    clean = ss.sosfilt(sos, np.concatenate([np.zeros(guard), tx.samples, np.zeros(guard + d)]))[d:]
    # A unit tone at the 24-tone scale has power 1/24; noise of variance v puts v/240 in a tone's bin.
    noise = rng.standard_normal(clean.size) * np.sqrt(10 * 10 ** (-snr_db / 10))
    r = frame.receive(clean + noise, cfg, start=guard)
    assert r.header_ok and r.block_ok.all() and np.all(r.control_ok) and not r.detail["snr_fallback"]
    off = cfg.clip_db - 7.0
    tones = range(3, 24)
    est = _kept_db(r.detail["snr_tone_signal"], r.detail["snr_tone_noise"], tones, off)
    w, n_sym = cfg.n_tones, cfg.total_symbols
    rows = frame.known_symbols(cfg, r.header_ok, r.block_ok, r.control_ok)
    rows[:params.PREAMBLE_SYMBOLS] = False
    ws = r.detail["window_start"]
    yc = ofdm.demodulate(np.concatenate([clean, np.zeros(2000)]), ws, n_sym, w, cfg.cp, active=w)[rows]
    nz = ofdm.demodulate(np.concatenate([noise, np.zeros(2000)]), ws, n_sym, w, cfg.cp, active=w)[rows]
    x = ofdm.demodulate(np.concatenate([tx.samples, np.zeros(1)]), cfg.cp, n_sym, w, cfg.cp, active=w)
    x = (x * (tx.grid != 0))[rows]
    live = np.abs(x) > 0
    cnt = np.maximum(live.sum(axis=0), 1)
    g = np.sum(yc * np.conj(x) * live, axis=0) / np.maximum(np.sum(np.abs(x) ** 2 * live, axis=0), 1e-30)
    sig = np.abs(g) ** 2 * np.sum(np.abs(x) ** 2 * live, axis=0) / cnt
    isi = np.sum(np.abs(yc - g * x) ** 2 * live, axis=0) / cnt
    noi = np.sum(np.abs(nz) ** 2 * live, axis=0) / cnt
    return est, _kept_db(sig, isi + noi, tones, off)


def test_short_frames_measure_the_isi_of_tones_past_the_prefix():
    # 2026-11-08 on 1.25 m: CONNECT, CONNECT-ACK and a 1-block UI frame read
    # 23-24 dB over tones 3-23 where the masked 4 s frames read 26-31 dB, the
    # gap on tones 3-7. It is not the short frames' estimate: through a
    # high-pass whose low tones arrive past the prefix, every frame reads its
    # own true SNR within a fraction of a dB (short frames slightly low). It
    # is what the frames carry: tones 0-2, late, spill into the following
    # symbol on the tones around them, so a frame that sends them (CONNECT,
    # CONNECT-ACK and SOUND are unmasked) has less on tones 3-7 than one
    # whose mask leaves them silent. Summing the per-tone powers over the
    # tones a mask keeps therefore under-predicts the masked frame.
    est, true = {}, {}
    for kind in DISPERSIVE_FRAMES:
        pairs = [_dispersive(kind, seed) for seed in (1, 2)]
        est[kind] = float(np.mean([e for e, _ in pairs]))
        true[kind] = float(np.mean([t for _, t in pairs]))
        assert -1.0 <= est[kind] - true[kind] <= 0.25, (kind, est[kind], true[kind])
    for kind in ("connect", "ui1", "long4"):
        assert true["long4_masked"] - true[kind] >= 1.5, (kind, true)
