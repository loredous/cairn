"""On-air fields and PHY rules of the link-layer ADRs 0008-0012: header flag
bits 6 and 7 and the Mask ID, the cumulative ACK, the block map, the
control-block orders, the session width and probe extent, mask selection by
ID on receive, block sequences, and PDU packing (ADR 0009)."""

import numpy as np
import pytest

from cairn_sim import fields, frame, params, pdu

A = fields.Address
T = fields.FrameType
S1, S2 = A("N0CALL", 1), A("N0CALL", 2)


def _int(bits) -> int:
    return int("".join(map(str, bits)), 2)


# --- header and ACK layouts (ADR 0011) ---------------------------------------------


def test_header_new_bits_positions():
    h = fields.Header(T.DATA, S1, S2, sequence=5, n_control=2, n_data=3, session=True, block_map=True, mask_id=3)
    v = _int(h.message_bits())
    assert (v >> 112) & 0xFF == 0b10001011                  # control, data, session, block map
    assert (v >> 1) & 3 == 3 and v & 1 == 0                  # Mask ID, then the reserved bit
    assert fields.Header.unpack(h.bits()) == h
    b = fields.Header(T.UI, S1, A.broadcast_address(), first_fragment=True, more_fragments=True)
    assert (_int(b.message_bits()) >> 112) & 0xFF == 0b01000100
    assert fields.Header.unpack(b.bits()) == b
    with pytest.raises(fields.FieldError):
        fields.Header(T.DATA, S1, S2, mask_id=4).bits()
    # The reserved bit is ignored on receipt.
    msg = h.message_bits().copy()
    msg[-1] = 1
    assert fields.Header.unpack(fields.with_crc(msg)) == h


def test_ack_mask_id_and_cumulative_bitmap():
    base, bm = fields.AckBlock.cumulative(65534, [65535, 0, 3, 65534, 200, 65533])
    assert base == 65534 and bm == (1 << 1) | (1 << 2) | (1 << 5)   # at/before the base or >= 128 beyond: out
    a = fields.AckBlock(base, bm, tone_mask=fields.ALL_TONES, mask_id=2)
    out = fields.unpack_control(fields.control_bits(a))
    assert out == a and out.mask_id == 2 and not out.holds(0)
    v = _int(fields.control_message_bits(a))
    assert (v >> 6) & 3 == 2 and v & 0x3F == 0
    assert a.acknowledged(65533) and not a.acknowledged(65534) and a.acknowledged(0) and not a.acknowledged(1)
    assert a.acknowledged(65535) and not a.acknowledged(65534 + 128)


def test_serial_diff():
    assert fields.serial_diff(65535, 0) == 1 and fields.serial_diff(0, 65535) == -1
    assert fields.serial_diff(10, 10) == 0 and fields.serial_diff(0, 32768) == 32768


# --- block map (ADR 0008, B.8) -------------------------------------------------------


def test_block_map_round_trip_and_layout():
    m = fields.BlockMapBlock.for_sequences([65530, 65531, 2, 121])
    assert m.base_sequence == 65530 and m.sequences() == [65530, 65531, 2, 121]
    assert m.bitmap == 1 | 2 | (1 << 8) | (1 << 127)
    bits = fields.control_bits(m)
    assert _int(bits[:4]) == 7 and _int(bits[8:24]) == 65530 and not bits[152:240].any()
    assert fields.unpack_control(bits) == m


def test_plan_blocks():
    assert fields.plan_blocks([10, 11, 12]) == (10, None)
    assert fields.plan_blocks([65535, 0, 1]) == (65535, None)
    seq, m = fields.plan_blocks([7, 9, 10])
    assert seq == 7 and m.sequences() == [7, 9, 10]
    for bad in ([], [3, 3], [5, 4], [0, 128], list(range(129))):
        with pytest.raises(fields.FieldError):
            fields.plan_blocks(bad)
    assert fields.plan_blocks([0, 127])[1].bitmap == 1 | (1 << 127)


def test_block_sequences_rules():
    h = fields.Header(T.DATA, S1, S2, sequence=100, n_control=2, n_data=3, session=True, block_map=True)
    ack = fields.AckBlock()
    good = fields.BlockMapBlock.for_sequences([100, 104, 110])
    assert fields.block_sequences(h, (ack, good)) == (100, 104, 110)
    assert fields.block_sequences(fields.Header(T.DATA, S1, S2, sequence=65535, n_control=1, n_data=2,
                                                session=True), (ack,)) == (65535, 0)
    for bad in (None,                                                      # failed or missing
                fields.BlockMapBlock(100, 0b111 | 1 << 9),                 # four blocks for three
                fields.BlockMapBlock(101, 0b111),                          # base != Sequence
                fields.BlockMapBlock(100, 0b110 | 1 << 4),                 # bit 0 clear
                fields.IdBlock("N0CALL")):                                 # not a block map
        assert fields.block_sequences(h, (ack, bad)) is None
    assert fields.block_sequences(h, (ack,)) is None


# --- B.3, control-block orders (ADR 0010, ADR 0012) --------------------------------------


def test_dictionary_only_b3():
    d = fields.ConnectBlock.dictionaries((1, 2, 3, 4))
    out = fields.unpack_control(fields.control_bits(d))
    assert out == d and out.dictionary_only and not fields.ConnectBlock().dictionary_only
    for cap, mft in ((0, 40), (23, 0), (10, 40)):
        with pytest.raises(fields.FieldError):
            fields.control_bits(fields.ConnectBlock(width_cap=cap, max_frame_time=mft))


def _h(t, n_control, **kw):
    return fields.Header(t, S1, S2, n_control=n_control, **kw)


B3 = fields.ConnectBlock(session_id=7, capabilities=fields.CAP_WIDE, width_cap=29)
ID = fields.IdBlock("N0CALL", 1)
ACK = fields.AckBlock(tone_mask=fields.ALL_TONES)


@pytest.mark.parametrize("header,controls", [
    (_h(T.CONNECT, 2, probe=True), (B3, ID)),
    (_h(T.CONNECT, 3, probe=True), (fields.ConnectBlock(width_cap=23, more_dictionaries=True), ID,
                                    fields.ConnectBlock.dictionaries((9, 0, 0, 0)))),
    (_h(T.CONNECT_ACK, 2, session=True, probe=True), (B3, ID)),
    (_h(T.CONNECT_ACK, 3, session=True, probe=True), (fields.ConnectBlock(width_cap=23, resume=True), ID, ACK)),
    (_h(T.PROBE, 0), ()),
    (_h(T.PROBE, 1, probe=True), (fields.ConnectBlock.link_test(fields.CAP_WIDE, 39),)),
    (_h(T.SOUND, 1, probe=True), (fields.AckBlock(requested_mcs=3, tone_mask=frozenset(range(24))),)),
    (_h(T.DISCONNECT, 2), (fields.AckBlock(), ID)),
    (_h(T.DISCONNECT, 2, session=True, mask_id=1), (ACK, ID)),
    (_h(T.DATA, 3, session=True, n_data=2, block_map=True),
     (ACK, fields.BlockMapBlock.for_sequences([0, 2]), ID)),
    (_h(T.ACK, 3, session=True), (ACK, ID, fields.CalReportBlock())),
    (fields.Header(T.BEACON, S1, A.broadcast_address(), n_control=1, first_fragment=True), (ID,)),
])
def test_control_orders_accepted(header, controls):
    fields.check_controls(header, controls)


@pytest.mark.parametrize("header,controls", [
    (_h(T.CONNECT, 2, probe=True), (ID, B3)),                                           # order
    (_h(T.CONNECT, 2, probe=True), (fields.ConnectBlock(width_cap=23, requested_mcs=2), ID)),
    (_h(T.CONNECT, 2, probe=True), (fields.ConnectBlock(width_cap=23, more_dictionaries=True), ID)),
    (_h(T.CONNECT_ACK, 1, session=True, probe=True), (B3,)),                               # no ID block
    (_h(T.CONNECT_ACK, 3, session=True, probe=True), (B3, ID, ACK)),                       # ACK without resume
    (_h(T.CONNECT_ACK, 2, session=True, probe=True, mask_id=1), (B3, ID)),                 # masked CONNECT-ACK
    (_h(T.PROBE, 1, probe=True), (fields.ConnectBlock(session_id=3, width_cap=39),)),
    (_h(T.PROBE, 0, probe=True), ()),                                                    # BEACON request with probe
    (_h(T.SOUND, 1, probe=True), (fields.AckBlock(base_sequence=4, tone_mask=frozenset(range(24))),)),
    (_h(T.DISCONNECT, 2), (fields.AckBlock(requested_mcs=1), ID)),
    (_h(T.DISCONNECT, 1, session=True), (ACK,)),                                          # no ID block
    (_h(T.DATA, 2, session=True, n_data=2), (ACK, fields.BlockMapBlock.for_sequences([0, 2]))),   # no flag 7
    (_h(T.DATA, 2, session=True, n_data=2, block_map=True), (fields.BlockMapBlock.for_sequences([0, 2]), ACK)),
    (_h(T.DATA, 3, session=True, n_data=2), (ACK, fields.CalReportBlock(), ID)),           # ID after others
    (_h(T.DATA, 1, session=True, n_data=1, first_fragment=True), (ACK,)),                  # flag 6 off broadcast
    (_h(T.DATA, 1, session=True, n_data=1), ()),                                          # count disagrees
])
def test_control_orders_refused(header, controls):
    with pytest.raises(fields.FieldError):
        fields.check_controls(header, controls)


# --- session width, probe extent, mask by ID (ADR 0011) ---------------------------------


def test_session_width_and_link_state():
    wide = fields.ConnectBlock(capabilities=fields.CAP_WIDE, width_cap=39)
    assert fields.session_width(39, fields.CAP_WIDE) == 40 and fields.session_width(39, 0) == 24
    link = frame.LinkState.session(wide, fields.CAP_WIDE)
    assert (link.width, link.cap) == (40, 40)
    assert frame.LinkState.session(wide, 0).width == 24
    assert frame.LinkState.probe_to(55) == frame.LinkState(width=56, cap=56)
    with pytest.raises(ValueError):
        frame.LinkState(mask_id=1, pending=(1, None))


def test_mask_for_header():
    m0, m1 = frozenset(range(24)) - {5}, frozenset(range(24)) - {6}
    link = frame.LinkState(24, m0, mask_id=2, pending=(3, m1))
    data = lambda i: fields.Header(T.DATA, S1, S2, n_control=1, session=True, mask_id=i)
    assert link.mask_for(data(2)) == m0 and link.mask_for(data(3)) == m1
    with pytest.raises(frame.UnknownMask):
        link.mask_for(data(0))
    assert link.mask_for(fields.Header(T.CONNECT_ACK, S1, S2, session=True)) is None
    assert link.mask_for(fields.Header(T.UI, S1, S2, mask_id=1)) is None


def _payloads(n, seed=1):
    return np.random.default_rng(seed).integers(0, 2, (n, params.DATA_PAYLOAD_BITS), dtype=np.uint8)


def _pad(x, n=600):
    return np.concatenate([np.zeros(n), x, np.zeros(n)])


def test_block_map_frame_round_trip_with_mask_callback():
    sequences = [65534, 1, 2, 9]
    seq, bmap = fields.plan_blocks(sequences)
    m1 = frozenset(range(30)) - {11}
    tx_link = frame.LinkState.session(fields.ConnectBlock(capabilities=fields.CAP_WIDE, width_cap=29),
                                      fields.CAP_WIDE, mask=m1, mask_id=1)
    h = fields.Header(T.DATA, S1, S2, sequence=seq, mcs=2, n_control=2, n_data=4, session=True, block_map=True,
                      mask_id=1)
    ack = fields.AckBlock(*fields.AckBlock.cumulative(3, []), tone_mask=fields.ALL_TONES)
    pl = _payloads(4)
    tx = frame.build_frame(h, [ack, bmap], pl, link=tx_link)
    assert tx.header.probe and tx.config.n_tones == 30 and tx.config.mask == m1
    seen = []

    def link_for(header):
        seen.append(header)
        return frame.LinkState(30, frozenset(range(30)), mask_id=0, pending=(1, m1))

    r = frame.receive(_pad(tx.samples), link=link_for)
    assert seen == [tx.header] and r.block_ok.all() and r.controls[1] == bmap
    assert r.block_sequences == tuple(sequences)
    assert [s for s, _ in r.delivered()] == sequences
    assert all(np.array_equal(b, pl[k]) for k, (_, b) in enumerate(r.delivered()))

    # The header names a mask the receiver does not hold: nothing is decoded.
    r = frame.receive(_pad(tx.samples), link=lambda _: frame.LinkState(30, frozenset(range(30))))
    assert r.header == tx.header and "mask ID 1" in r.link_error
    assert not r.block_ok.any() and not r.control_ok.any() and r.block_sequences is None
    assert len(r.block_ok) == 4 and r.controls == (None, None) and r.delivered() == []

    # Flag bit 7 set but the block map fails its CRC: no block is delivered.
    bad = tx.control_bits.copy()
    bad[1, 30] ^= 1
    broken = frame.rebuild(tx.config, tx.header_bits, tx.block_bits, bad)
    r = frame.receive(_pad(broken.samples), link=link_for)
    assert r.block_ok.all() and r.controls[1] is None
    assert r.block_sequences is None and r.delivered() == []


def test_build_refuses_inconsistent_block_map():
    h = fields.Header(T.DATA, S1, S2, sequence=4, n_control=2, n_data=2, session=True, block_map=True)
    ack = fields.AckBlock(tone_mask=fields.ALL_TONES)
    with pytest.raises(ValueError):
        frame.build_frame(h, [ack, fields.BlockMapBlock.for_sequences([5, 7])], _payloads(2))
    with pytest.raises(ValueError):
        frame.build_frame(h, [ack, fields.IdBlock("N0CALL")], _payloads(2))
    with pytest.raises(ValueError):
        frame.build_frame(fields.Header(T.DATA, S1, S2, sequence=4, n_control=2, n_data=2, session=True),
                          [ack, fields.BlockMapBlock.for_sequences([4, 7])], _payloads(2))


def test_connect_ack_unmasked_and_width_from_probe_without_link():
    """CONNECT-ACK is unmasked whatever the link's mask; with no link state
    (the callback gives None) a session frame takes its width from its probe."""
    b3 = fields.ConnectBlock(session_id=1, capabilities=fields.CAP_WIDE, width_cap=35, requested_mcs=3)
    h = fields.Header(T.CONNECT_ACK, S2, S1, n_control=2, session=True, probe=True)
    tx = frame.build_frame(h, [b3, fields.IdBlock("N0CALL", 2)],
                           link=frame.LinkState.session(b3, fields.CAP_WIDE, mask=frozenset(range(36)) - {4}))
    assert tx.config.mask is None and tx.config.cap == 36
    r = frame.receive(_pad(tx.samples), link=lambda _: None)
    assert r.control_ok.all() and r.config.n_tones == 36 and r.detail["probe_extent"].width == 36


def test_connect_and_probe_extent_to_the_cap():
    """CONNECT and a link-test PROBE probe tones 0 .. width cap (ADR 0011)."""
    b3 = fields.ConnectBlock.link_test(fields.CAP_WIDE, 47)
    h = fields.Header(T.PROBE, S1, S2, sequence=99, n_control=1, probe=True)
    fields.check_controls(h, [b3])
    tx = frame.build_frame(h, [b3], link=frame.LinkState.probe_to(b3.width_cap))
    assert tx.config.n_tones == 48
    r = frame.receive(_pad(tx.samples))
    assert r.controls[0] == b3 and r.detail["probe_extent"].width == 48


# --- PDU packing (ADR 0009) ---------------------------------------------------------------


def test_pdu_fields_and_crc():
    p = pdu.Pdu.raw(pdu.PduType.MESSAGE, b"hello", pdu.stream_byte(1, 3))
    raw = p.encode()
    assert raw[:2] == (3 + 5 + 4).to_bytes(2, "big") and raw[2:5] == bytes([1, 0x43, 0])
    assert raw[-4:] == pdu.crc32(b"hello").to_bytes(4, "big") and pdu.Pdu.decode(raw) == p
    assert pdu.crc32(b"123456789") == 0xCBF43926
    d = pdu.Pdu(0, b"zz", 0x12345678, 0, 2, b"\xde\xad\xbe\xef")
    assert d.encode()[5:9] == b"\xde\xad\xbe\xef" and pdu.Pdu.decode(d.encode()) == d and d.length == 13
    with pytest.raises(pdu.PduError):
        pdu.Pdu(0, b"", 0, 0, 2, None)
    with pytest.raises(pdu.PduError):
        pdu.Pdu.raw(0, bytes(65535))
    with pytest.raises(pdu.PduError):
        pdu.Pdu.decode(b"\x00\x03abc")
    assert not pdu.Pdu.decode(raw[:-1] + bytes([raw[-1] ^ 1])).crc_ok


def test_pdu_one_byte_rule_and_filler():
    first = pdu.Pdu.raw(1, bytes(116))                       # 125 bytes: one left in the block
    second = pdu.Pdu.raw(1, b"x" * 10)
    blocks = pdu.pack_blocks([first, second])
    assert len(blocks) == 2 and blocks[0][125] == 0 and blocks[1][:2] == second.encode()[:2]
    assert not any(blocks[1][len(second.encode()):])         # filler
    assert pdu.unpack_blocks(blocks) == [first, second]
    exact = pdu.Pdu.raw(1, bytes(126 - 9))
    assert pdu.pack_blocks([exact]) == [exact.encode()]
    # A parser at a PDU boundary with one byte left skips it even if it is not zero.
    odd = bytearray(blocks[0])
    odd[125] = 0x55
    assert pdu.unpack_blocks([bytes(odd), blocks[1]]) == [first, second]


def test_pdu_random_streams_round_trip():
    rng = np.random.default_rng(4)
    for _ in range(30):
        pdus = [pdu.Pdu.raw(int(rng.integers(0, 7)), rng.bytes(int(rng.integers(0, 400))),
                            int(rng.integers(0, 256))) for _ in range(int(rng.integers(1, 8)))]
        blocks = pdu.pack_blocks(pdus)
        assert all(len(b) == 126 for b in blocks)
        assert pdu.unpack_blocks(blocks) == pdus
        # Packed in two runs with filler between them, as a session sender would.
        cut = int(rng.integers(0, len(pdus) + 1))
        assert pdu.unpack_blocks(pdu.pack_blocks(pdus[:cut]) + pdu.pack_blocks(pdus[cut:])) == pdus
    assert np.array_equal(pdu.block_bits(bytes([0x80] + [0] * 125))[:9], [1, 0, 0, 0, 0, 0, 0, 0, 0])
    assert pdu.bits_to_payload(pdu.block_bits(bytes(range(126)))) == bytes(range(126))


def test_fragment_and_reassemble():
    msg = bytes(range(256)) * 3
    frags = pdu.fragment(pdu.PduType.MESSAGE, msg, 300)
    assert [(f.first, f.more) for f in frags] == [(True, True), (False, True), (False, False)]
    one = pdu.fragment(pdu.PduType.MESSAGE, b"short", 300)
    assert [(f.first, f.more) for f in one] == [(True, False)]
    r = pdu.Reassembler()
    assert r.feed("A", 9, True, False, one[0].pdu) == one[0].pdu
    out = [r.feed("A", (65535 + i) % 65536, f.first, f.more, f.pdu) for i, f in enumerate(frags)]
    assert out[:2] == [None, None] and out[2].body == msg and out[2].crc_ok
    # A gap discards the partial message; a continuation without a start is dropped.
    assert r.feed("A", 1, True, True, frags[0].pdu) is None
    assert r.feed("A", 3, False, False, frags[2].pdu) is None
    assert r.feed("B", 4, False, False, frags[2].pdu) is None
    # A new first fragment restarts; sources are independent.
    assert r.feed("A", 10, True, True, frags[0].pdu) is None
    assert r.feed("B", 50, True, True, frags[0].pdu) is None
    assert r.feed("A", 11, False, False, frags[2].pdu).body == msg[:300] + msg[600:]
    assert pdu.max_body(1) == 117 and pdu.blocks_needed([pdu.Pdu.raw(1, bytes(pdu.max_body(3)))]) == 3
