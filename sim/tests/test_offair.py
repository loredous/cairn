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

from cairn_sim import frame
from cairn_sim.bench.audio import read_wav

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
