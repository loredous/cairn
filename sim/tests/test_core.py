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
    assert abs(10 * np.log10(np.mean(10 ** (r.snr_db / 10))) - 6.0) < 1.5


def test_awgn_sets_in_band_snr():
    rng = np.random.default_rng(10)
    x = np.zeros(480_000)
    noisy = channel.awgn(x, 10.0, 24, rng, signal_power=1.0)
    spec = np.abs(np.fft.rfft(noisy)) ** 2
    f = np.fft.rfftfreq(noisy.size, 1 / params.FS)
    band = (f >= 300) & (f <= 2700)
    in_band = 2 * spec[band].sum() / noisy.size ** 2
    assert abs(10 * np.log10(1.0 / in_band) - 10.0) < 0.2
