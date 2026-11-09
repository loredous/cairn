"""A weak 70 cm link (modem/fixtures/offair/weak70.json, see its README).

2026-11-08, K0JLB-2 receiving K0JLB-1 on 441.000 MHz: the sender's audio
carried lines at 1, 2, 3 and 4 kHz, 13-26 dB below the data and each midway
between two tones, which cost those tones 8-12 dB (sim/findings.md, "A weak
70 cm link"). Every block that passes its CRC carries what was sent; with
the lines subtracted (``frame.remove_lines``, the default) the receiver
decodes at least what it does today, and without, exactly what it did
before. The Rust receiver is held to the same numbers in
``modem/phy/tests/offair.rs``.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
import pytest

from cairn_sim import channel, frame, params
from cairn_sim.bench.audio import read_wav

DIR = Path(__file__).resolve().parents[2] / "modem" / "fixtures" / "offair"
INDEX = json.loads((DIR / "weak70.json").read_text())
SNR_TOLERANCE_DB = 0.5


def _bits(h: str, n: int) -> np.ndarray:
    return np.unpackbits(np.frombuffer(bytes.fromhex(h), dtype=np.uint8))[:n]


def _link(f: dict) -> frame.LinkState:
    mask = frozenset(f["mask"]) if f["mask"] is not None else None
    return frame.LinkState(width=f["width"], cap=f["width"], mask=mask, mask_id=f["mask_id"])


@pytest.mark.parametrize("f", INDEX["frames"], ids=lambda f: f["wav"])
@pytest.mark.parametrize("cancel", [True, False], ids=["lines_removed", "lines_kept"])
def test_weak70_frames(f, cancel):
    _, x = read_wav(DIR / f["wav"])
    r = frame.receive_header_first(x[:, 0], _link(f), start=f["start_sample"], cp=f["cp"], cancel_lines=cancel)
    assert r.header is not None
    assert np.array_equal(r.header.message_bits(), _bits(f["header_message_hex"], 128))
    assert r.control_ok.all() and len(r.control_ok) == len(f["control_hex"])
    for i, c in enumerate(f["control_hex"]):
        assert np.array_equal(r.control_bits[i, :240], _bits(c, 240))
    assert r.block_ok.size == len(f["payload_hex"])
    for i, p in enumerate(f["payload_hex"]):
        if r.block_ok[i]:
            assert np.array_equal(r.block_bits[i, :params.DATA_PAYLOAD_BITS], _bits(p, params.DATA_PAYLOAD_BITS))
    want = f["python"]["lines_removed" if cancel else "lines_kept"]
    ok = int(r.block_ok.sum())
    if cancel:
        assert ok >= want["blocks_ok"]
        assert [k for k, _ in r.detail["lines"]] == [k for k, _ in want["lines"]]
    else:
        assert ok == want["blocks_ok"]
        assert r.detail["lines"] == []
    assert r.detail["snr_db"] == pytest.approx(want["snr_db"], abs=SNR_TOLERANCE_DB)


def test_remove_lines_leaves_a_clean_frame_alone_and_finds_a_comb():
    """No line on a clean frame; on the same frame with the 70 cm comb added,
    the in-band harmonics found and the residue at least 25 dB down."""
    rng = np.random.default_rng(3)
    cfg = frame.FrameConfig(width=32, mcs=4, n_blocks=16)
    tx = frame.build(cfg, rng)
    x = np.concatenate([np.zeros(500), tx.samples, np.zeros(500)])
    clean = channel.FmChannel("mic_wide", snr_db=30.0, width=32)(x, np.random.default_rng(4))
    out, removed = frame.remove_lines(clean, 500, cfg.n_samples, 32)
    assert removed == [] and out is clean
    comb = channel.CAPTURE_COMBS["70cm-4"]
    dirty = channel.FmChannel("mic_wide", snr_db=30.0, width=32, comb=comb)(x, np.random.default_rng(4))
    out, removed = frame.remove_lines(dirty, 500, cfg.n_samples, 32)
    assert [k for k, _ in removed] == [1, 2, 3]
    added = dirty - clean
    left = (out - clean)[500:500 + cfg.n_samples]
    band = lambda s: channel.in_band_power(s, 32)          # noqa: E731
    assert 10 * np.log10(band(added[500:500 + cfg.n_samples]) / band(left)) > 25.0
    r = frame.receive(dirty, cfg, start=500)
    assert r.block_ok.all() and [k for k, _ in r.detail["lines"]] == [1, 2, 3]
