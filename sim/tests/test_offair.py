"""Real frames received over the air (modem/fixtures/offair, see its README).

Two stations, K0JLB-1 and K0JLB-2, each an AIOC into a handheld, 2026-11-07.
Every frame type decodes; a short frame's clock-offset estimate stays
consistent with the direction's 4 s frame (before the fix a header-only frame
read -115 and -206 ppm, a one-block frame -505 ppm); and a short frame's
channel SNR reads within 1.5 dB of the 4 s frame on the same path, never more
than 1 dB above it (before: up to 7 dB low). The Rust receiver is held to
the same in ``modem/phy/tests/offair.rs``.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
import pytest

from cairn_sim import frame, mapping
from cairn_sim.bench.audio import read_wav
from cairn_sim.crc import append_crc

DIR = Path(__file__).resolve().parents[2] / "modem" / "fixtures" / "offair"
INDEX = json.loads((DIR / "offair.json").read_text())
FRAMES = [(direction, d["long_frame"], f) for direction, d in INDEX["directions"].items() for f in d["frames"]]
SNR_TOLERANCE_DB = 1.5        # short frames against the 4 s frame of the same path
SNR_OVER_DB = 1.0             # ... of which at most this much above it (ADR 0006)
CLOCK_TOLERANCE_PPM = 30.0    # a decodable short frame against the 4 s frame, default prior
PRIOR_SD_PPM = 3.0            # the 4 s frame's estimate as the peer's prior ...
PRIOR_CLOCK_TOLERANCE_PPM = 5.0   # ... puts every frame within this of it


def _hex_bits(h: str, n: int) -> np.ndarray:
    return np.unpackbits(np.frombuffer(bytes.fromhex(h), dtype=np.uint8))[:n]


def _decode(name: str, clock_prior=None) -> frame.RxResult:
    _, x = read_wav(DIR / name)
    return frame.receive(x[:, 0], link=frame.LinkState(width=24, cap=24), clock_prior=clock_prior)


def _clock(r: frame.RxResult) -> tuple[float, float]:
    s = r.config.stride
    return -r.detail["drift"] / s * 1e6, r.detail["drift_sd"] / s * 1e6


def _check_content(f: dict, r: frame.RxResult) -> None:
    assert r.header is not None
    assert np.array_equal(r.header.message_bits(), _hex_bits(f["header_message_hex"], 128))
    assert len(r.control_ok) == len(f["control_hex"])
    for i, c in enumerate(f["control_hex"]):
        assert r.control_ok[i]
        assert np.array_equal(r.control_bits[i, :240], _hex_bits(c, 240))
    assert r.block_ok.size == len(f["payload_hex"])
    if f["data_decodable"]:
        for i, p in enumerate(f["payload_hex"]):
            assert r.block_ok[i]
            assert np.array_equal(r.block_bits[i, :1008], _hex_bits(p, 1008))
    else:
        assert not r.block_ok.any()      # 26.6 ms of the block's audio is missing from the capture


@pytest.mark.parametrize("direction,long,f", FRAMES, ids=[f["wav"] for _, _, f in FRAMES])
def test_offair_frame(direction, long, f):
    r = _decode(f["wav"])
    _check_content(f, r)
    ppm, sd = _clock(r)
    snr = float(r.detail["snr_db"])
    assert abs(ppm) <= frame.CLOCK_BOUND_PPM + 1e-9
    if f["kind"] == "4s":
        assert abs(ppm - long["clock_ppm"]) < 0.1 and sd < 2.0
        assert abs(snr - long["snr_db"]) < 0.15
        return
    if not f["data_decodable"]:
        assert r.detail["snr_fallback"]                 # the fallback errs low
        assert snr <= long["snr_db"] + SNR_OVER_DB
        return
    assert abs(ppm - long["clock_ppm"]) <= CLOCK_TOLERANCE_PPM
    assert abs(ppm - long["clock_ppm"]) <= 3 * sd + 3.0
    assert not r.detail["snr_fallback"]
    assert abs(snr - long["snr_db"]) <= SNR_TOLERANCE_DB
    assert snr - long["snr_db"] <= SNR_OVER_DB


DATA_FRAMES = [(d, lg, f) for d, lg, f in FRAMES if f["data_decodable"] and f["payload_hex"]]
LEVEL_TOLERANCE_DB = 0.5      # the tracked data level against the one every known symbol gives
RAMP_DB = 3.0                 # every captured frame's data sat at least this far above the S2 level


def _data_symbols(r: frame.RxResult, f: dict) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """(equalised data symbols, what was sent, the noise variance the receiver used)."""
    cfg = r.config
    bits = np.array([_hex_bits(p, 1008) for p in f["payload_hex"]], np.uint8)
    sent = frame.rebuild(cfg, r.header_bits, append_crc(bits), r.control_bits if cfg.n_control else None)
    d0, dt = cfg.first_data, list(cfg.data_tones)
    rows = slice(d0 - frame.params.PREAMBLE_SYMBOLS, None)
    return r.detail["eq"][rows, dt], sent.grid[d0:, dt] / cfg.data_gain, r.detail["nv"][rows, dt]


def _gain_db(eq: np.ndarray, x: np.ndarray) -> float:
    """The data symbols' common gain on the sent ones, each tone weighted by its own error."""
    g0 = np.sum(np.real(np.conj(x) * eq)) / np.sum(np.abs(x) ** 2)
    w = 1 / np.maximum(np.mean(np.abs(eq - g0 * x) ** 2, axis=0), 1e-9)
    return float(20 * np.log10(np.sum(w * np.real(np.conj(x) * eq)) / np.sum(w * np.abs(x) ** 2)))


@pytest.mark.parametrize("direction,long,f", DATA_FRAMES, ids=[f["wav"] for _, _, f in DATA_FRAMES])
def test_offair_level_tracking(direction, long, f):
    # The captured frames' data sat 3-6 dB above the level the S2 pair
    # measured (the radios' level still rising under the preamble): without
    # tracking the equalised data symbols are that far off unit power. The
    # level tracker finds the data's level and divides it out.
    assert f["data_gain_db"] > RAMP_DB
    r = _decode(f["wav"])
    lv = r.detail["level"]
    assert lv.applied and lv.settle < 0          # the level was still rising after S2
    assert abs(20 * np.log10(lv.segments[-1]) - f["data_gain_db"]) < LEVEL_TOLERANCE_DB
    eq, x, nv = _data_symbols(r, f)
    assert abs(_gain_db(eq, x)) < LEVEL_TOLERANCE_DB
    # 16-QAM decision regions and LLRs: the hard decisions' error rate, and
    # the error rate the LLRs predict for themselves, agree (untracked, the
    # A to B frames' LLRs claimed twice the errors the symbols had, and the
    # B to A frame's symbols had 19% raw bit errors).
    bps = r.config.rung.bits_per_symbol
    if bps >= 4:
        llr = mapping.demodulate_llr(eq, nv, bps)
        sent = mapping.demodulate_llr(x, np.ones(x.shape), bps) < 0
        raw = float(np.mean((llr < 0) != sent))
        predicted = float(np.mean(1 / (1 + np.exp(np.abs(llr)))))
        assert raw < 0.06
        assert 0.75 < predicted / raw < 1.33, (raw, predicted)
    # Untracked, the data symbols sit at the ramp's offset.
    off = frame.receive(read_wav(DIR / f["wav"])[1][:, 0], link=frame.LinkState(width=24, cap=24),
                        track_levels=False)
    assert _gain_db(*_data_symbols(off, f)[:2]) > RAMP_DB
    assert int(off.block_ok.sum()) == f["blocks_ok_untracked"]


def test_offair_level_tracking_decodes_the_long_frame():
    # B to A: the 4 s frame's data sat 6.2 dB above the S2 level; untracked,
    # two of its 13 MCS 2 (16-QAM) blocks failed.
    f = next(f for d, _, f in FRAMES if d == "BtoA" and f["kind"] == "4s")
    assert f["blocks_ok_untracked"] == 11
    r = _decode(f["wav"])
    assert r.block_ok.all()
    _check_content(f, r)


@pytest.mark.parametrize("direction,long,f", FRAMES, ids=[f["wav"] for _, _, f in FRAMES])
def test_offair_level_fit_recorded(direction, long, f):
    # What modem/phy/tests/offair.rs holds the Rust receiver to.
    r = _decode(f["wav"])
    lv, want = r.detail["level"], f["python_level"]
    assert lv.applied == want["applied"]
    np.testing.assert_allclose(lv.segments, want["segments"], rtol=1e-9)
    np.testing.assert_allclose(lv.level, want["level"], rtol=1e-9)
    assert int(r.block_ok.sum()) == f["blocks_ok"]


@pytest.mark.parametrize("direction,long,f", FRAMES, ids=[f["wav"] for _, _, f in FRAMES])
def test_offair_clock_prior_from_the_link(direction, long, f):
    prior = frame.ClockPrior(long["clock_ppm"], PRIOR_SD_PPM)
    r = _decode(f["wav"], clock_prior=prior)
    _check_content(f, r)
    ppm, sd = _clock(r)
    assert abs(ppm - long["clock_ppm"]) <= PRIOR_CLOCK_TOLERANCE_PPM
    assert sd <= PRIOR_SD_PPM + 1e-9
    # The same prior given per header, as the link layer knows the sender only then.
    r2 = _decode(f["wav"], clock_prior=lambda header: prior)
    assert r2.detail["drift"] == pytest.approx(r.detail["drift"])


WIDE = INDEX["wide"]


@pytest.mark.parametrize("f", WIDE, ids=[f["wav"] for f in WIDE])
def test_offair_wide_broadcast_takes_its_width_from_the_data(f):
    # A 48-tone UI broadcast through radios that pass audio to about 4.25 kHz:
    # tones 42-47 arrive 30 dB below the noise and 38-41 fade into it, so the
    # probe's extent reads 38. Before the fix the receiver kept 38 (and the
    # neighbours 35-41 it retried), failed the block and reported "x38 from
    # probe". The block decodes only at the width it was sent at.
    _, x = read_wav(DIR / f["wav"])
    r = frame.receive(x[:, 0])
    ext = r.detail["probe_extent"]
    assert ext.width == f["probe_extent_width"] < f["width"]
    assert r.config.width == r.config.cap == ext.data_width == f["width"]
    _check_content({**f, "data_decodable": True}, r)
