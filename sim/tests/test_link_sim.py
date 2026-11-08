"""Link budget (SPEC 7.4, Appendix A) and threshold interpolation."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import link_sim
from cairn_sim.link_sim import Budget, Point, default_frame_s


PRE_ERRATUM = dict(probe=False, ack_block=False, pad_symbols=False)   # SPEC 7.4 / Appendix A before 2026-11-07

# SPEC Appendix A.2 as printed: data blocks per frame and net kbps, 24 / 56 tones.
A2 = {0: ((3, 7), (1.21, 2.82)), 1: ((4, 10), (1.73, 4.14)), 2: ((13, 32), (3.02, 7.16)),
      3: ((20, 48), (4.55, 10.74)), 4: ((27, 64), (6.09, 14.32)), 5: ((34, 80), (7.61, 17.90)),
      6: ((40, 96), (9.11, 21.48))}


@pytest.mark.parametrize("mcs", sorted(A2))
def test_appendix_a2_pinned(mcs):
    """Appendix A.2 exactly: the ACK control block in every DATA frame, the
    probe at 56 tones, the data run padded to whole symbols (SPEC 6.4), 2 s
    frames at rungs 0-1, and the 0.51 s peer ACK cycle."""
    (b24, b56), (k24, k56) = A2[mcs]
    for width, blocks, kbps in ((24, b24, k24), (56, b56, k56)):
        b = Budget(mcs, width, default_frame_s(mcs))
        assert b.blocks == blocks
        assert round(b.net_bps / 1000, 2) == kbps
        assert b.peer_turnaround_s + b.ack_time_s == pytest.approx(0.509, abs=0.001)
        lo, hi = (2.33, 2.50) if mcs <= 1 else (4.34, 4.51)                    # the cycle range A.2 quotes
        assert lo - 0.005 <= b.cycle_s <= hi + 0.005


def test_worked_example_7_4():
    """SPEC 7.4: 20 blocks in 311 symbols after the 135 ms ACK block, a 4.43 s
    cycle and 4.55 kbps; 2 s frames about 3.6 kbps; 300 ms TXDelay at both
    ends and 2 s frames about 3.1 kbps."""
    b = Budget(3)
    assert b.blocks == 20 and b.data_symbols == 311
    assert b.control_symbols * link_sim.symbol_s() == pytest.approx(0.135)
    assert b.data_symbols * link_sim.symbol_s() == pytest.approx(3.499, abs=0.001)
    assert round(b.cycle_s, 2) == 4.43 and round(b.net_bps / 1000, 2) == 4.55
    assert round(Budget(3, frame_s=2.0).net_bps / 1000, 1) == 3.6
    assert round(Budget(3, frame_s=2.0, txdelay_s=0.3, peer_turnaround_s=0.4).net_bps / 1000, 1) == 3.1
    assert b.ack_time_s == pytest.approx(0.259, abs=0.001)
    # A block map or an ID block costs 12 data-block symbols (A.2).
    assert Budget(3, extra_controls=1).block_symbols_available == b.block_symbols_available - 12


def test_pre_erratum_budget_still_available():
    # The drafts before the erratum: 21 blocks, about 4.75 kbps.
    b = Budget(3, **PRE_ERRATUM)
    assert b.blocks == 21 and b.symbols_per_block == pytest.approx(15.5, abs=0.06)
    assert b.net_bps == pytest.approx(4750, rel=0.05)


def test_block_symbols_in_a_4s_frame():
    assert Budget(3, **PRE_ERRATUM).block_symbols_available == 330
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
