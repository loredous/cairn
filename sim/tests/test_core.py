"""Fast checks of the thin slice: CRC, codes, mapping, OFDM and a full frame."""

from __future__ import annotations

import numpy as np
import pytest

from cairn_sim import channel, crc, frame, ldpc, mapping, ofdm, params


def test_crc16_ccitt_false_check_value():
    assert crc.crc16(b"123456789") == 0x29B1
    bits = np.unpackbits(np.frombuffer(b"123456789", dtype=np.uint8))
    assert crc.crc16_bits(bits) == 0x29B1


def test_batched_crc_matches_bit_serial():
    rng = np.random.default_rng(0)
    msgs = rng.integers(0, 2, (5, 128), dtype=np.uint8)
    checker = crc.CrcChecker(128)
    for m, c in zip(msgs, checker.crc(msgs)):
        assert crc.bits_to_int(c) == crc.crc16_bits(m)
    blocks = crc.append_crc(msgs)
    assert checker.check(blocks).all()
    blocks[2, 7] ^= 1
    assert list(checker.check(blocks)) == [True, True, False, True, True]


@pytest.mark.parametrize("spec", [params.HEADER_CODE, params.CONTROL_CODE, params.DATA_CODE],
                         ids=lambda s: s.name)
def test_codewords_satisfy_parity_checks(spec):
    code = ldpc.code_for(spec)
    rng = np.random.default_rng(1)
    info = crc.append_crc(rng.integers(0, 2, (4, spec.info_bits - 16), dtype=np.uint8))
    cw = code.encode(info)
    h = code.parity_check_matrix().astype(np.int32)
    assert not ((h @ cw.T.astype(np.int32)) % 2).any()
    assert np.array_equal(cw[:, : spec.info_bits], info)            # systematic
    assert not cw[:, spec.info_bits:code.k_native].any()            # fillers are zero


def test_code_geometry_matches_spec():
    data = ldpc.code_for(params.DATA_CODE)
    assert (data.z, data.k_native, data.k, data.n_codeword) == (48, 1056, 1024, 3264)
    assert data.buffer.size == 3168 - 32   # rate-matching buffer without fillers
    assert ldpc.code_for(params.CONTROL_CODE).k_native == 260
    assert ldpc.code_for(params.HEADER_CODE).k_native == 150


@pytest.mark.parametrize("e", params.DATA_CODE.coded_bits)
def test_noiseless_decode_every_data_length(e):
    code = ldpc.code_for(params.DATA_CODE)
    rng = np.random.default_rng(2)
    info = crc.append_crc(rng.integers(0, 2, (3, 1008), dtype=np.uint8))
    tx = code.rate_match(code.encode(info), e)
    llr = code.rate_recover(8.0 * (1 - 2.0 * tx), e)
    res = ldpc.decode(code, llr, rows=code.rows_needed(code.tx_positions(e)))
    assert res.ok.all() and np.array_equal(res.bits, info)


def test_shorter_lengths_are_prefixes_of_the_buffer():
    code = ldpc.code_for(params.DATA_CODE)
    longest = code.tx_positions(2048)
    for e in params.DATA_CODE.coded_bits:
        assert np.array_equal(code.tx_positions(e), longest[:e])
    # A retransmission continues where the first transmission stopped.
    assert np.array_equal(code.tx_positions(512, start=2048), code.buffer[2048:2560])


@pytest.mark.parametrize("bps", [2, 4, 6, 8])
def test_constellations_are_gray_and_unit_power(bps):
    levels, labels = mapping.pam_levels(bps // 2)
    order = np.argsort(levels)
    assert all(np.sum(labels[order[i]] != labels[order[i + 1]]) == 1 for i in range(len(order) - 1))
    rng = np.random.default_rng(3)
    bits = rng.integers(0, 2, bps * 4096, dtype=np.uint8)
    x = mapping.modulate(bits, bps)
    assert abs(np.mean(np.abs(x) ** 2) - 1) < 0.03
    assert np.array_equal((mapping.demodulate_llr(x, 1e-3, bps) < 0).astype(np.uint8), bits)


def test_interleaver_and_scrambler_invert():
    rng = np.random.default_rng(4)
    bits = rng.integers(0, 2, 1368, dtype=np.uint8)
    assert np.array_equal(mapping.deinterleave(mapping.interleave(bits)), bits)
    llr = 1 - 2.0 * mapping.scramble(bits, 5)
    assert np.array_equal((mapping.descramble_llr(llr, 5) < 0).astype(np.uint8), bits)


def test_ofdm_tones_round_trip_with_negated_tail_prefix():
    rng = np.random.default_rng(5)
    grid = mapping.modulate(rng.integers(0, 2, 2 * 24 * 3, dtype=np.uint8), 2).reshape(3, 24)
    x = ofdm.modulate(grid)
    assert abs(np.mean(x ** 2) - 1) < 0.1
    sym = x[:540]
    assert np.allclose(sym[:60], -sym[480:540])            # half-bin tones: negated tail
    assert np.allclose(ofdm.demodulate(x, 60, 3, 24), grid)


def test_ofdm_delay_within_prefix_is_a_per_tone_phase():
    rng = np.random.default_rng(6)
    grid = mapping.modulate(rng.integers(0, 2, 2 * 24 * 2, dtype=np.uint8), 2).reshape(2, 24)
    x = np.concatenate([np.zeros(20), ofdm.modulate(grid)])
    y = ofdm.demodulate(x, 60, 2, 24)                      # window 20 samples early
    ratio = y / grid
    assert np.allclose(ratio[0], ratio[1])
    assert np.allclose(np.abs(ratio), 1)


@pytest.mark.parametrize("mcs", range(6))
def test_frame_loopback_ideal_channel(mcs):
    rng = np.random.default_rng(7)
    cfg = frame.FrameConfig(mcs=mcs, n_blocks=3)
    tx = frame.build(cfg, rng)
    rx = np.concatenate([np.zeros(777), tx.samples, np.zeros(500)])
    r = frame.receive(rx, cfg, reference=tx.preamble_samples)
    assert r.start == 777 and r.header_ok
    assert np.array_equal(r.header_bits, tx.header_bits)
    assert r.block_ok.all() and np.array_equal(r.block_bits, tx.block_bits)


def test_frame_unclipped_256qam_loopback():
    rng = np.random.default_rng(8)
    cfg = frame.FrameConfig(mcs=6, n_blocks=3, clip=False)
    tx = frame.build(cfg, rng)
    r = frame.receive(tx.samples, cfg, start=0)
    assert r.block_ok.all()


def test_frame_decodes_qpsk_at_6db():
    rng = np.random.default_rng(9)
    cfg = frame.FrameConfig(mcs=0, n_blocks=4)
    tx = frame.build(cfg, rng)
    rx = channel.awgn(np.concatenate([np.zeros(300), tx.samples, np.zeros(300)]), 6.0, 24, rng)
    r = frame.receive(rx, cfg, reference=tx.preamble_samples)
    assert r.header_ok and r.block_ok.all()
    # Effective SINR: includes channel-estimation error, so a little below the true SNR.
    est = 10 * np.log10(np.mean(10 ** (r.snr_db / 10)))
    assert 6.0 - 3.0 < est < 6.0 + 1.0


def test_awgn_sets_in_band_snr():
    rng = np.random.default_rng(10)
    x = np.zeros(480_000)
    noisy = channel.awgn(x, 10.0, 24, rng, signal_power=1.0)
    spec = np.abs(np.fft.rfft(noisy)) ** 2
    f = np.fft.rfftfreq(noisy.size, 1 / params.FS)
    band = (f >= 300) & (f <= 2700)
    in_band = 2 * spec[band].sum() / noisy.size ** 2
    assert abs(10 * np.log10(1.0 / in_band) - 10.0) < 0.2


@pytest.mark.parametrize("width", [24, 25, 30, 37, 38, 41, 52, 53, 56])
def test_every_width_loops_back(width):
    rng = np.random.default_rng(width)
    for mcs in (0, 2, 4, 5):
        cfg = frame.FrameConfig(width=width, mcs=mcs, n_blocks=2)
        tx = frame.build(cfg, rng)
        r = frame.receive(tx.samples, cfg, start=0)
        assert r.header_ok and r.block_ok.all(), (width, mcs)
    cfg = frame.FrameConfig(width=width, mcs=6, n_blocks=2, clip=False)
    tx = frame.build(cfg, rng)
    assert frame.receive(tx.samples, cfg, start=0).block_ok.all()


def test_width_probe_follows_the_header():
    cfg = frame.FrameConfig(width=30, mcs=3, n_blocks=2)
    assert cfg.has_probe and cfg.probe_symbols == 2
    assert not frame.FrameConfig().has_probe
    with pytest.raises(ValueError):
        frame.FrameConfig(width=30, probe=False)


def test_pilot_value_depends_only_on_symbol_and_slot():
    assert np.array_equal(frame.pilot_values(10, 2), frame.pilot_values(10, 4)[:, :2])


def test_probe_value_does_not_depend_on_probe_width():
    wide = frame.probe_grid(56, 11)
    for width in (24, 30, 38, 53):
        assert np.array_equal(frame.probe_grid(width, 11), wide[:, :width]), width


def test_clip_and_filter_bounds_papr_on_10000_symbols():
    """SPEC 21.2 ofdm acceptance: PAPR <= 7 dB on 10,000 random symbols."""
    rng = np.random.default_rng(11)
    for width in (24, 56):
        n = 5000
        tones = np.array(params.data_tones_for(width))
        grid = np.zeros((n, width), dtype=complex)
        grid[:, tones] = mapping.modulate(rng.integers(0, 2, n * len(tones) * 4, dtype=np.uint8),
                                          4).reshape(n, len(tones))
        x = ofdm.clip_and_filter(ofdm.modulate(grid, active=width), width, tones, scale_tones=width)
        assert ofdm.papr_db(x).max() <= 7.0 + 1e-3


def test_clip_level_by_rung():
    assert [params.clip_db(m) for m in range(7)] == [7, 7, 7, 7, 8, 9, 10]
    assert [params.clip_offset_db(m) for m in range(7)] == [0, 0, 0, 0, 1, 2, 3]
    assert frame.FrameConfig(mcs=5).clip_db == 9.0
    assert frame.FrameConfig(mcs=5, clip_db=7.0).clip_db == 7.0


def test_whole_frame_clipped_at_the_rung_level():
    """Header and data share the data rung's clip level: at MCS 6 (10 dB)
    neither is held to 7 dB, and nothing exceeds 10 dB."""
    cfg = frame.FrameConfig(mcs=6, n_blocks=6)
    tx = frame.build(cfg, np.random.default_rng(12))
    papr = ofdm.papr_db(tx.samples, cfg.cp)
    first_data = params.PREAMBLE_SYMBOLS + cfg.header_symbols
    assert papr.max() <= 10.0 + 1e-3
    assert papr[params.PREAMBLE_SYMBOLS:first_data].max() > 7.5
    assert papr[first_data:].max() > 7.5
    low = ofdm.papr_db(frame.build(frame.FrameConfig(mcs=3, n_blocks=6),
                                   np.random.default_rng(12)).samples)
    assert low.max() <= 7.0 + 1e-3


def test_symbol_distortion_raises_only_a_symbol_far_above_the_assumed_noise():
    pilots = (3, 20, 37, 52)
    known = np.ones((3, 4))
    nv = np.full((3, 56), 1e-4)
    eq = np.ones((3, 56), dtype=complex)
    eq[1, list(pilots)] += 0.1                     # symbol 1: pilot error 1e-2, 100 times the assumed noise
    eq[2, list(pilots)] += 1.5e-2                  # symbol 2: 2.25e-4, scatter within a factor of 6
    out = frame.symbol_distortion(eq, nv, pilots, known, list(range(56)))
    assert np.allclose(out[0], 1e-4) and np.allclose(out[2], 1e-4)
    assert np.allclose(out[1], 1e-2)
    # two pilots (24 tones) scatter too much: left alone
    assert np.allclose(frame.symbol_distortion(eq, nv, (3, 20), known[:, :2], list(range(56)))[1], 1e-4)
