"""Header and control block fields (SPEC 8.2, Appendix B) and header-first
reception: every field over its range, CRC and malformed-field handling, and
frames decoded without being told their configuration."""

import numpy as np
import pytest

from cairn_sim import channel, fields, frame, params

A = fields.Address
T = fields.FrameType
S1, S2 = A("N0CALL", 1), A("N0CALL", 2)


def header_round_trip(h: fields.Header) -> fields.Header:
    b = h.bits()
    assert b.shape == (144,)
    out = fields.Header.unpack(b)
    assert np.array_equal(out.bits(), b)
    return out


# --- addresses ----------------------------------------------------------------------


@pytest.mark.parametrize("call", ["A", "N0CALL", "N0C/-", "0", "ZZZZZZ", "AB-9"])
@pytest.mark.parametrize("ssid", [0, 7, 15])
def test_short_address_round_trip(call, ssid):
    a = A(call, ssid)
    assert A.decode(a.encode()) == a


def test_long_callsign_is_hashed():
    a = A("N0CALL/7", 4)
    v = a.encode()
    assert v >> 34 == 63 and v & 0xF == 4
    out = A.decode(v)
    assert out.callsign is None and out.hash30 == a.hash30 and out == a.on_air()
    assert A("n0call/7", 4).encode() == v            # upper-cased before hashing
    assert A("N0CALL/7X").hash30 != a.hash30
    assert A("ABCDEFGHIJ").is_hashed
    with pytest.raises(fields.FieldError):
        A("ABCDEFGHIJK")                             # at most 10 characters


def test_hash_never_collides_with_broadcast(monkeypatch):
    import hashlib

    class AllOnes:
        def __init__(self, *_):
            pass

        def digest(self):
            return b"\xff" * 32

    monkeypatch.setattr(hashlib, "sha256", AllOnes)
    assert fields.callsign_hash("ANYCALL1") == (1 << 30) - 2


def test_broadcast_and_malformed_addresses():
    b = A.broadcast_address()
    assert b.encode() == (1 << 40) - 1 and A.decode((1 << 40) - 1) == b
    for bad in (0,                                    # no characters
                (39 << 34),                           # reserved code
                (1 << 34) | (0 << 28) | (1 << 22),    # padding before a character
                (1 << 34) | (63 << 28)):              # code 63 outside position 1
        with pytest.raises(fields.FieldError):
            A.decode(bad)
    for bad in ("", "N0 CALL", "N0CALL!"):
        with pytest.raises(fields.FieldError):
            A(bad)
    with pytest.raises(fields.FieldError):
        A("N0CALL", 16)


# --- header ---------------------------------------------------------------------------


@pytest.mark.parametrize("frame_type", [0, 1, 5, 10, 17, 30, 31])
def test_header_frame_types(frame_type):
    h = fields.Header(frame_type, S1, S2)
    assert header_round_trip(h) == h


def test_header_field_ranges():
    rng = np.random.default_rng(3)
    for seq in (0, 1, 32767, 65535):
        for mcs in (0, 6, 7, 15):
            for n_c in range(4):
                for n_d in (0, 1, 2, 64, 127, 128):
                    flags = rng.integers(0, 2, 4).astype(bool)
                    h = fields.Header(int(rng.integers(0, 32)), S1, A.broadcast_address(), seq, mcs, n_c, n_d,
                                      *flags)
                    assert header_round_trip(h) == h
    with pytest.raises(fields.FieldError):
        fields.Header(T.DATA, S1, S2, n_data=129).bits()
    with pytest.raises(fields.FieldError):
        fields.Header(T.DATA, S1, S2, n_control=4).bits()
    for bad in (dict(sequence=65536), dict(mcs=16), dict(frame_type=32)):
        with pytest.raises(fields.FieldError):
            fields.Header(**{"frame_type": 0, "source": S1, "destination": S2, **bad}).bits()


def test_header_layout_positions():
    h = fields.Header(T.DATA, S1, S2, sequence=0xABCD, mcs=5, n_control=2, n_data=100, session=True, probe=True)
    v = int("".join(map(str, h.message_bits())), 2)
    assert v >> 125 == 1 and (v >> 120) & 0x1F == 4
    assert (v >> 112) & 0xFF == 0b00011011              # control, data, session, probe
    assert (v >> 72) & ((1 << 40) - 1) == S1.encode() and (v >> 32) & ((1 << 40) - 1) == S2.encode()
    assert (v >> 16) & 0xFFFF == 0xABCD and (v >> 12) & 0xF == 5
    assert (v >> 10) & 3 == 2 and (v >> 3) & 0x7F == 99 and v & 7 == 0


def test_header_crc_and_field_errors():
    h = fields.Header(T.CONNECT, S1, S2, n_control=2, probe=True)
    b = h.bits()
    for i in range(144):                                 # CRC-16 catches every single-bit error
        c = b.copy()
        c[i] ^= 1
        with pytest.raises(fields.FieldError):
            fields.Header.unpack(c)
    msg = h.message_bits().copy()
    msg[15] = 0                                          # flag bit 0 cleared, 2 control blocks
    with pytest.raises(fields.FieldError, match="flag bit 0"):
        fields.Header.unpack(fields.with_crc(msg))
    msg = fields.Header(T.UI, S1, S2, n_data=0).message_bits().copy()
    msg[118:125] = 1                                     # count 127 with flag bit 1 clear: ignored
    assert fields.Header.unpack(fields.with_crc(msg)).n_data == 0
    msg[125:128] = 1                                     # reserved bits ignored on receipt
    msg[8:10] = 1                                        # flag bits 6 and 7 too
    assert fields.Header.unpack(fields.with_crc(msg)).n_data == 0


# --- control blocks -------------------------------------------------------------------


def control_round_trip(c):
    b = fields.control_bits(c)
    assert b.shape == (256,)
    out = fields.unpack_control(b)
    assert out == c and np.array_equal(fields.control_bits(out), b)
    return out


def test_ack_block_ranges():
    rng = np.random.default_rng(5)
    for _ in range(40):
        ok = rng.integers(0, 2, 128).astype(bool)
        mask = None if rng.integers(0, 2) else frozenset(int(k) for k in np.flatnonzero(rng.integers(0, 2, 56)))
        c = fields.AckBlock(int(rng.integers(0, 65536)), fields.AckBlock.bitmap_from(ok), int(rng.integers(0, 16)),
                            int(rng.integers(0, 256)), int(rng.integers(0, 256)), mask, bool(rng.integers(0, 2)),
                            bool(rng.integers(0, 2)), int(rng.integers(0, 8)), bool(rng.integers(0, 2)))
        out = control_round_trip(c)
        assert [out.passed(i) for i in range(128)] == list(ok)
    for edge in (fields.AckBlock(), fields.AckBlock(65535, (1 << 128) - 1, 15, 255, 255, fields.ALL_TONES,
                                                    True, True, 7, True)):
        control_round_trip(edge)
    assert fields.snr_code(-3) == 0 and fields.snr_code(17.125) == 69 and fields.snr_code(63.75) == 255
    assert fields.snr_code(99) == 255 and fields.AckBlock(snr_q=69).snr_db == 17.25


def test_ack_block_layout():
    c = fields.AckBlock(base_sequence=1, bitmap=1, tone_mask=frozenset({0}), long_prefix=True)
    b = fields.control_bits(c)
    assert list(b[:8]) == [0, 0, 0, 0, 1, 0, 0, 1]       # type 0; flags bit 3 and bit 0
    assert b[8 + 15] == 1 and b[24 + 127] == 1           # base 1; block base + 0 at bit 0 (sent last)
    assert b[24 + 128 + 4 + 8 + 8 + 55] == 1             # tone 0 at bit 0 of the mask


def test_connect_block_ranges():
    for cap in (23, 39, 55):
        for mft in (5, 20, 40):
            for caps in (0, fields.CAP_WIDE | fields.CAP_DICT_FETCH, 0xFFFF):
                c = fields.ConnectBlock(0xFFFF, caps, cap, 15, mft, frozenset(range(0, 56, 3)),
                                        (0, 1, 0xFFFFFFFF, 0x12345678), True, True)
                control_round_trip(c)
    control_round_trip(fields.ConnectBlock())
    for bad in (dict(width_cap=22), dict(width_cap=56), dict(max_frame_time=4), dict(max_frame_time=41)):
        with pytest.raises(fields.FieldError):
            fields.control_bits(fields.ConnectBlock(**bad))


def test_calibration_id_and_path_blocks():
    control_round_trip(fields.CalRequestBlock())
    control_round_trip(fields.CalRequestBlock(255, 15, 255, 255, 255, 15))
    control_round_trip(fields.CalReportBlock((0, 255, 1, 2, 3, 4, 5, 6), (15, 0, 1, 2, 3, 4, 5, 6),
                                             (True, False, True, False, False, False, False, True), True))
    for call, ssid, grid in (("N0CALL", 0, None), ("N0CALL/7", 15, "FN31"), ("ABCDEFGHIJ", 3, "AA00AA")):
        out = control_round_trip(fields.IdBlock(call, ssid, grid))
        assert out.callsign == call
    with pytest.raises(fields.FieldError):
        fields.control_bits(fields.IdBlock("ABCDEFGHIJK"))
    for entries in ((), (A("WIDE1", 1),), (A("WIDE2", 2), A("N0CALL/7", 1).on_air())):
        control_round_trip(fields.PathBlock(entries, 3, (True, True), True))
    for t in (2, 7, 15):                                 # reserved types are skipped, not errors
        msg = np.zeros(240, np.uint8)
        msg[:4] = [(t >> (3 - i)) & 1 for i in range(4)]
        assert isinstance(fields.unpack_control(fields.with_crc(msg)), fields.UnknownControl)
    b = fields.control_bits(fields.CalRequestBlock())
    b[100] ^= 1
    with pytest.raises(fields.CrcError):
        fields.unpack_control(b)


# --- frames built from headers ----------------------------------------------------------


def _ack(n=4):
    return fields.AckBlock(base_sequence=10, bitmap=fields.AckBlock.bitmap_from([True] * n), requested_mcs=2,
                           snr_q=fields.snr_code(15))


def _pad(x, n=600):
    return np.concatenate([np.zeros(n), x, np.zeros(n)])


def test_header_only_frame_is_clipped_at_7db():
    h = fields.Header(T.PROBE, S1, S2, mcs=6)            # MCS field ignored without data blocks
    tx = frame.build_frame(h)
    cfg = tx.config
    assert cfg.total_symbols == 11 and cfg.clip_db == 7.0
    assert frame.FrameConfig(mcs=6, n_blocks=0).clip_db == 7.0
    assert frame.FrameConfig(mcs=6, n_blocks=1).clip_db == 10.0
    r = frame.receive(_pad(tx.samples))
    assert r.header == h and r.block_ok.size == 0 and r.control_ok.size == 0


def test_control_segment_geometry():
    cfg = frame.FrameConfig(width=30, mcs=3, n_blocks=2, n_control=3, mask=frozenset(range(56)) - {1, 9})
    assert cfg.control_tones == tuple(k for k in range(24) if k not in (1, 3, 9, 20))
    assert cfg.control_symbols == -(-3 * 512 // (2 * 20))
    assert cfg.control_gain == pytest.approx(np.sqrt(24 / 22))
    assert cfg.first_control == 4 + 7 + 2 and cfg.first_data == cfg.first_control + cfg.control_symbols
    assert frame.FrameConfig(n_control=1).control_symbols == 12       # SPEC A.1


def test_header_must_agree_with_config():
    h = fields.Header(T.DATA, S1, S2, mcs=2, n_control=1, n_data=2)
    with pytest.raises(ValueError):
        frame.build(frame.FrameConfig(mcs=2, n_blocks=3, n_control=1), np.random.default_rng(0), header=h,
                    controls=[_ack()])
    with pytest.raises(ValueError):
        frame.build_frame(h, [])                          # one control block promised


@pytest.mark.parametrize("chan,snr", [("awgn", 9.0), ("mic", 14.0)])
def test_header_first_through_channels(chan, snr):
    rng = np.random.default_rng(21)
    h = fields.Header(T.DATA, S1, S2, sequence=500, mcs=2, n_control=1, n_data=3, session=True)
    for trial in range(3):
        tx = frame.build_frame(h, [_ack()], rng=rng)
        x = _pad(tx.samples)
        y = channel.awgn(x, snr, 24, rng) if chan == "awgn" else channel.FmChannel(chan, snr_db=snr)(x, rng)
        r = frame.receive(y)                              # nothing but the samples
        assert r.header == h, (trial, r.header_error)
        assert r.config.n_blocks == 3 and r.config.n_control == 1 and r.config.mcs == 2
        assert r.control_ok.all() and r.controls == (_ack(),)
        assert r.block_ok.all() and np.array_equal(r.block_bits, tx.block_bits)


def test_controls_and_probe_wide_masked_long_prefix():
    rng = np.random.default_rng(8)
    usable = frozenset(range(56)) - {1, 9, 25}
    link = frame.LinkState(width=30, mask=usable, cp=params.CP_LONG)
    h = fields.Header(T.CONNECT_ACK, S2, S1, n_control=2, session=True, probe=True)
    cb = fields.ConnectBlock(session_id=7, width_cap=29, requested_mcs=3, tone_mask=usable)
    tx = frame.build_frame(h, [cb, _ack()], link=link)
    assert tx.config.cp == params.CP_LONG and tx.config.has_probe
    y = channel.FmChannel("mic", snr_db=16.0, width=30)(_pad(tx.samples), rng)
    r = frame.receive(y, link=frame.LinkState(width=30, mask=usable))   # prefix detected
    assert r.header == h and r.config.cp == params.CP_LONG
    assert r.controls == (cb, _ack())
    # Data after controls at width 30 with the probe, masked.
    h2 = fields.Header(T.DATA, S1, S2, sequence=3, mcs=3, n_control=1, n_data=4, session=True, probe=True)
    tx2 = frame.build_frame(h2, [_ack()], link=link, rng=rng)
    y2 = channel.awgn(_pad(tx2.samples), 20.0, 30, rng)
    r2 = frame.receive(y2, link=frame.LinkState(width=30, mask=usable))
    assert r2.header == h2 and r2.control_ok.all() and r2.block_ok.all()
    assert np.array_equal(r2.block_bits, tx2.block_bits)


def test_header_failure_and_control_failure_handling():
    rng = np.random.default_rng(4)
    h = fields.Header(T.DATA, S1, S2, mcs=1, n_control=2, n_data=2, session=True)
    tx = frame.build_frame(h, [_ack(), fields.IdBlock("N0CALL", 1)], rng=rng)
    x = _pad(tx.samples)
    # Wipe out the header's symbols: no header, so nothing else is decoded.
    stride = tx.config.stride
    y = x.copy()
    y[600 + 4 * stride:600 + 11 * stride] = rng.standard_normal(7 * stride) * 3
    r = frame.receive(y, start=600)
    assert r.header is None and r.header_error and r.block_ok.size == 0 and r.config is None
    # A header whose CRC passes but whose fields are malformed is refused the same way.
    msg = h.message_bits().copy()
    msg[0:3] = [0, 1, 0]                                  # version 2
    bad = frame.build(tx.config, rng, header_msg=msg, controls=[_ack(), fields.IdBlock("N0CALL", 1)])
    r = frame.receive(_pad(bad.samples))
    assert r.header_ok is True and r.header is None and "version" in r.header_error and r.block_ok.size == 0
    # Wipe out the second control block only: it fails, the rest decodes.
    cfg = tx.config
    s_second = cfg.first_control + (512 // 2 + 10) // len(cfg.control_tones)   # inside block 2 only
    y = x.copy()
    y[600 + s_second * stride:600 + (s_second + 6) * stride] *= -1
    r = frame.receive(y)
    assert r.header == h and list(r.control_ok) == [True, False] and r.controls[1] is None
    assert r.controls[0] == _ack() and r.block_ok.all()
    # A header naming a reserved rung with data blocks describes no frame.
    msg = fields.Header(T.DATA, S1, S2, mcs=9, n_data=1).message_bits()
    odd = frame.build(frame.FrameConfig(n_blocks=1), rng, header_msg=msg)
    r = frame.receive(_pad(odd.samples))
    assert r.header is None and "reserved rung" in r.header_error
