"""ADR 0007: every wide frame carries the probe; the probe reaches the
sender's cap; a broadcast's width comes from its probe; header-first decode
of frames built with real headers (SPEC 5.4, 8.2, 10.4)."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import extent_eval, fields, frame, link_sim, params
from cairn_sim.fields import Address, FrameType as T, Header

SRC = Address("N0CALL", 7)
DST = Address("N1CALL", 2)
BCAST = Address.broadcast_address()
ID = fields.IdBlock(callsign="N0CALL", ssid=7, grid="FN31")


def _air(tx: frame.TxFrame, rng: np.random.Generator, snr: float | None = None, lead: int = 1500) -> np.ndarray:
    x = np.concatenate([np.zeros(lead), tx.samples, np.zeros(1500)])
    return x if snr is None else link_sim.awgn_channel(x, snr, tx.config.width, rng)


def test_wide_frame_without_probe_is_refused():
    with pytest.raises(ValueError, match="probe"):
        frame.FrameConfig(width=30, mcs=2, n_blocks=2, probe=False)
    hdr = Header(T.DATA, SRC, DST, mcs=2, n_data=2, session=True)          # probe flag clear
    cfg = frame.FrameConfig(width=30, mcs=2, n_blocks=2)                   # probe on by the rule
    assert cfg.has_probe and cfg.needs_probe
    with pytest.raises(ValueError):
        frame.build(cfg, np.random.default_rng(0), header=hdr)


def test_probe_follows_the_data_not_the_width():
    # Data on the base tones only (a 56-tone link masked to 24): no probe needed.
    cfg = frame.FrameConfig(width=56, mcs=2, n_blocks=2, mask=frozenset(range(24)))
    assert not cfg.needs_probe and not cfg.has_probe and cfg.n_tones == params.BASE_TONES
    # Header and control blocks only, at width 30: no probe needed either.
    assert not frame.FrameConfig(width=30, n_blocks=0, n_control=1).has_probe
    # 24 tones: never needed, still allowed (CONNECT always probes).
    assert frame.FrameConfig(width=24, n_blocks=0, probe=True).has_probe


def test_build_frame_sets_the_probe_flag():
    hdr = Header(T.UI, SRC, BCAST, sequence=3, mcs=1, n_data=2)
    tx = frame.build_frame(hdr, link=frame.LinkState(width=40))
    assert tx.header.probe and tx.config.has_probe
    assert not frame.build_frame(hdr).header.probe                          # 24 tones: no probe


@pytest.mark.parametrize("width,cap", [(24, 30), (30, 56), (40, 48)])
def test_probe_reaches_the_cap(width, cap):
    rng = np.random.default_rng(width + cap)
    usable = frozenset(range(cap)) - {9}
    link = frame.LinkState(width=width, mask=usable, cap=cap)
    hdr = Header(T.DATA, SRC, DST, sequence=11, mcs=3, n_control=1, n_data=3, session=True, probe=True)
    ack = fields.AckBlock(base_sequence=5, bitmap=0b111, requested_mcs=3, snr_q=fields.snr_code(20.0))
    tx = frame.build_frame(hdr, [ack], link=link, rng=rng)
    cfg = tx.config
    assert cfg.cap == cap and cfg.n_tones == cap and tx.grid.shape[1] == cap
    probe = tx.grid[cfg.first_probe:cfg.first_control]
    assert np.all(probe[:, :cap] != 0)                                       # every tone up to the cap
    assert np.all(tx.grid[cfg.first_data:, width:] == 0)                     # data stays within the width
    y = _air(tx, rng, 25.0)
    r = frame.receive(y, link=link, start=1500)
    assert r.header == tx.header and r.config == cfg
    assert r.block_ok.all() and r.control_ok.all() and r.controls == (ack,)
    assert r.snr_db.shape == (cap,)                                          # tones above the width measured
    assert np.all(r.snr_db[width:cap] > 10)
    # A receiver assuming the probe stops at the width gets the frame's geometry wrong.
    wrong = frame.receive(y, link=frame.LinkState(width=width, mask=usable), start=1500)
    assert wrong.config.cap == width


@pytest.mark.parametrize("width,mcs", [(25, 0), (30, 1), (40, 2), (56, 3)])
def test_broadcast_width_from_probe_without_link_state(width, mcs):
    rng = np.random.default_rng(width)
    hdr = Header(T.UI, SRC, BCAST, sequence=width, mcs=mcs, n_control=1, n_data=3)
    tx = frame.build_frame(hdr, [ID], link=frame.LinkState(width=width), rng=rng)
    assert tx.header.probe and not tx.header.session
    y = _air(tx, rng, 20.0)
    r = frame.receive(y)                       # no link state: 24-tone default, detector start
    ext = r.detail["probe_extent"]
    assert ext.width == width and r.config.width == width and r.config.cap == width
    assert r.header == tx.header and r.block_ok.all() and r.controls == (ID,)
    inside, outside = ext.margin_db()
    assert inside > 3 and outside > 3


def test_broadcast_24_tones_has_no_probe():
    rng = np.random.default_rng(3)
    hdr = Header(T.BEACON, SRC, BCAST, n_control=1)
    tx = frame.build_frame(hdr, [ID], rng=rng)
    assert not tx.header.probe
    r = frame.receive(_air(tx, rng, 15.0))
    assert r.detail["probe_extent"] is None and r.config.width == 24 and r.controls == (ID,)


@pytest.mark.parametrize("profile,width,snr", [("awgn", 25, 6.0), ("awgn", 33, 6.0), ("awgn", 48, 3.5),
                                               ("awgn", 56, 3.5), ("mic", 26, 10.0), ("mic", 28, 10.0),
                                               ("awgn", 40, None)])
def test_probe_extent_detection(profile, width, snr):
    rng = np.random.default_rng(width * 7)
    for _ in range(3):
        e, _, _ = extent_eval.trial(profile, width, snr, 3, rng)
        assert e.width == width, (e.width, e.margin_db())


def test_extent_ignores_clip_regrowth_above_the_cap():
    # Noise-free, clip-and-filter regrowth sits just above the highest tone;
    # it must not extend the width.
    rng = np.random.default_rng(1)
    for w in (25, 37, 52, 55):
        e, _, _ = extent_eval.trial("awgn", w, None, 2, rng, mcs=6)
        assert e.width == w
        assert e.margin_db()[1] > 3


def test_retry_finds_the_width_when_the_edge_misleads(monkeypatch):
    # Force the extent one tone short: header-first decode still lands on
    # the sent width because only it decodes.
    rng = np.random.default_rng(4)
    hdr = Header(T.UI, SRC, BCAST, mcs=0, n_data=2)
    tx = frame.build_frame(hdr, link=frame.LinkState(width=30), rng=rng)
    y = _air(tx, rng, 15.0)
    real = frame.probe_extent

    def short(*a, **k):
        e = real(*a, **k)
        e.width -= 1
        return e
    monkeypatch.setattr(frame, "probe_extent", short)
    r = frame.receive(y, start=1500)
    assert r.config.width == 30 and r.block_ok.all()


@pytest.mark.parametrize("width,mask", [(24, None), (30, frozenset(range(30)) - {7, 26}),
                                        (44, None), (56, frozenset(range(56)) - {40, 41})])
def test_header_first_with_real_headers_in_session(width, mask):
    rng = np.random.default_rng(width)
    link = frame.LinkState(width=width, mask=mask)
    hdr = Header(T.DATA, SRC, DST, sequence=0x1234, mcs=2, n_control=1, n_data=4, session=True)
    ack = fields.AckBlock(base_sequence=9, bitmap=0xF, requested_mcs=2, snr_q=fields.snr_code(12.25))
    tx = frame.build_frame(hdr, [ack], link=link, rng=rng)
    r = frame.receive(_air(tx, rng, 18.0), link=link)
    assert r.header == tx.header and r.header.probe == (width > 24)
    assert r.block_ok.all() and np.array_equal(r.block_bits, tx.block_bits) and r.controls == (ack,)
    assert r.detail["probe_extent"] is None


def test_link_state_from_cap():
    link = frame.LinkState.from_cap(40, mask=frozenset(range(40)) - {30})
    assert link.width == 40 and link.cap == 40


def test_masked_snr_counts_the_boost():
    # ADR 0006 interpretation: a masked frame reads higher than an unmasked
    # one on the same channel, by about the boost on the data symbols.
    rng = np.random.default_rng(9)
    out = []
    for mask in (None, frozenset(range(30)) - {1, 9, 25, 27}):
        cfg = frame.FrameConfig(width=30, mcs=3, n_blocks=12, mask=mask)
        tx = frame.build(cfg, rng)
        x = np.concatenate([np.zeros(1000), tx.samples, np.zeros(1000)])
        noise = rng.standard_normal(x.size) * np.sqrt(np.mean(tx.samples ** 2) / 10 ** 2.0)
        r = frame.receive(x + noise, cfg, start=1000)
        out.append(r.detail["snr_db"])
    assert 0.2 < out[1] - out[0] < 0.9
