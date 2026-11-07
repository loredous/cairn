"""LDPC performance measurement: fast sanity checks and slow acceptance."""

import numpy as np
import pytest

from cairn_sim import crc, ldpc, ldpc_eval, params


def test_biawgn_capacity_matches_known_limit():
    # BPSK rate-1/2 Shannon limit is Eb/N0 = 0.187 dB.
    snr = 2 * 0.5 * 10 ** (0.187 / 10)
    c, v = ldpc_eval.biawgn_capacity_dispersion(snr)
    assert c == pytest.approx(0.5, abs=1e-3) and v > 0


def test_normal_approximation_is_above_shannon_and_ordered():
    ppv = [ldpc_eval.normal_approx_ebn0(1024, e) for e in (2048, 1536, 1368, 1232)]
    assert ppv == sorted(ppv)
    assert 0.19 < ppv[0] < 1.2
    # Longer blocks at the same rate need less Eb/N0.
    assert ldpc_eval.normal_approx_ebn0(2048, 4096) < ppv[0]


def test_sum_product_decodes_noiseless_and_zero_errors_at_high_snr():
    spec = params.CONTROL_CODE
    code = ldpc.code_for(spec)
    rng = np.random.default_rng(3)
    info = crc.append_crc(rng.integers(0, 2, (4, 240), dtype=np.uint8))
    tx = code.rate_match(code.encode(info), 512)
    llr = code.rate_recover(8.0 * (1 - 2.0 * tx), 512).astype(np.float64)
    res = ldpc_eval.decode_spa(code, llr)
    assert res.ok.all() and np.array_equal(res.bits, info)
    for decoder in ("min-sum", "bp"):
        p = ldpc_eval.measure(spec, 512, 6.0, min_errors=1, max_blocks=200, batch=100,
                              decoder=decoder)
        assert p.errors == 0 and p.blocks == 200


def test_measure_is_reproducible():
    a = ldpc_eval.measure(params.HEADER_CODE, 288, 1.0, min_errors=5, max_blocks=400, batch=200)
    b = ldpc_eval.measure(params.HEADER_CODE, 288, 1.0, min_errors=5, max_blocks=400, batch=200)
    assert a == b and a.errors > 0


@pytest.mark.slow
def test_data_rate_half_bler_point_is_near_expected():
    """1% BLER at n = 2048, k = 1024. The gap to the normal approximation is bounded
    so a decoder regression shows up; see ldpc_eval.py for the full comparison."""
    spec, e = params.DATA_CODE, 2048
    x, pts = ldpc_eval.ebn0_at_bler(spec, e, min_errors=100, max_blocks=60_000)
    ppv = ldpc_eval.normal_approx_ebn0(spec.info_bits, e)
    assert 0.3 < x - ppv < 1.2, (x, ppv, pts)
    assert 1.4 < x < 1.9, x
