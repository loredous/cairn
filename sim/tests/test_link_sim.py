"""Link budget (SPEC 7.4, Appendix A) and threshold interpolation."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import link_sim
from cairn_sim.link_sim import Budget, Point, default_frame_s


PRINTED = dict(probe=False, ack_block=False)     # SPEC 7.4 and Appendix A as printed


def test_worked_example_7_4_reproduced_within_5_percent():
    b = Budget(3, **PRINTED)          # 24 tones, MCS 3, 4 s frames, TXDelay 150 ms
    assert b.blocks == 21
    assert b.symbols_per_block == pytest.approx(15.5, abs=0.06)
    assert b.cycle_s == pytest.approx(4.46, rel=0.05)
    assert b.net_bps == pytest.approx(4750, rel=0.05)
    assert b.ack_time_s == pytest.approx(0.259, abs=0.001)


@pytest.mark.parametrize("mcs,blocks24,blocks56", [
    (0, 3, 7), (1, 4, 11), (2, 14, 33), (3, 21, 50), (4, 28, 67), (5, 35, 83), (6, 42, 100)])
def test_appendix_a2_blocks_per_frame(mcs, blocks24, blocks56):
    # Appendix A.2 lists 84 blocks for MCS 5 at 56 tones; only 83 fit in the
    # 330 block symbols of a 4 s frame (330 * 312 / 1232 = 83.6).
    assert Budget(mcs, 24, default_frame_s(mcs), **PRINTED).blocks == blocks24
    assert Budget(mcs, 56, default_frame_s(mcs), **PRINTED).blocks == blocks56


@pytest.mark.parametrize("mcs,blocks24,blocks56", [
    (0, 3, 7), (1, 4, 10), (2, 13, 32), (3, 20, 48), (4, 27, 64), (5, 34, 80), (6, 40, 96)])
def test_blocks_with_ack_block_and_probe(mcs, blocks24, blocks56):
    # SPEC 8.4: every DATA frame carries the ACK control block (12 symbols);
    # ADR 0007: every frame with data above tone 23 carries the probe (2).
    assert Budget(mcs, 24, default_frame_s(mcs)).blocks == blocks24
    assert Budget(mcs, 56, default_frame_s(mcs)).blocks == blocks56


def test_block_symbols_in_a_4s_frame():
    assert Budget(3, **PRINTED).block_symbols_available == 330
    assert Budget(3).block_symbols_available == 318                    # less the ACK block
    assert Budget(3, 56).block_symbols_available == 316                # and the probe
    assert Budget(3, 56, ack_block=False).block_symbols_available == 328
    # A 56-tone link masked down to the base tones needs no probe.
    assert Budget(3, 56, mask=frozenset(range(24))).probe_symbols == 0


def test_threshold_interpolates_log_bler():
    pts = [Point(10.0, 1000, 100, 1, 0), Point(11.0, 1000, 1, 1, 0)]
    assert link_sim.interpolate_threshold(pts, 0.01) == pytest.approx(10.5)
    assert link_sim.interpolate_threshold(pts, 0.5) is None


def test_bler_point_runs_frames():
    pt = link_sim.bler_point(0, 30.0, blocks_per_frame=4, min_blocks=8, min_errors=0,
                             rng=np.random.default_rng(0))
    assert pt.blocks == 8 and pt.block_errors == 0 and pt.frames == 2


def test_threshold_steps_down_until_bracketed(monkeypatch):
    # A fake waterfall: BLER 1 below 3 dB, 0 above. Starting at 8 dB must
    # step down repeatedly rather than give up after one step.
    def fake_point(mcs, snr, **kw):
        errs = 1000 if snr < 3.0 else 0
        return Point(snr, 1000, errs, 1, 0)
    monkeypatch.setattr(link_sim, "bler_point", fake_point)
    thr, pts = link_sim.threshold(0, start_db=8.0)
    assert thr is not None and 2.0 <= thr <= 3.0
    assert min(p.snr_db for p in pts) == 2.0
