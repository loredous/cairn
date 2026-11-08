"""Generate the header, control block, whole-frame and link vectors (SPEC 8.2,
Appendix B, ADRs 0007-0012).

``python -m cairn_sim.frame_vectors [--root DIR]`` writes, under
``vectors/`` (format in ``vectors/README.md``):

- ``header/<case>.json``: header fields and their 144 bits (SPEC 8.2,
  ADR 0011 ``header-field-layout``, B.1), including headers a receiver must
  reject.
- ``control/<case>.json``: control block fields and their 256 bits for the
  Appendix B types, including blocks a receiver must reject or skip.
- ``frame/<case>.json``: whole frames built from real headers and control
  blocks, followed through every stage as in ``vectors/bitmap/``
  (``bitmap_vectors.case_doc``): a CONNECT, a resuming CONNECT-ACK, DATA
  frames with an ACK control block (one masked, long prefix, wider than 24
  tones), a frame with three control blocks and an ACK frame.
- ``link/<case>.json``: the link-layer additions of ADRs 0008-0012: block map
  control blocks (B.8), a DATA frame with an ACK block, a block map and
  non-consecutive blocks, a new-session CONNECT-ACK with its ID block, a
  link-test PROBE and its SOUND reply, a BEACON-request PROBE, and PDU
  packing (``pdu``): PDUs into block payloads with filler and CRC-32, and a
  broadcast message fragmented over three frames.

Every frame passes ``fields.check_controls`` (the control-block order of its
type). Output is deterministic; ``sim/tests/test_frame_vectors.py`` checks
that regenerating is byte-identical to the committed files.
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

import numpy as np

from . import bitmap_vectors, fields, params, pdu
from .bitmap_vectors import Case, control_json, header_json
from .crc import crc16_bits, int_to_bits
from .vectors import bits_to_hex

FORMAT_VERSION = 1
A = fields.Address
T = fields.FrameType
STATION_1 = A("N0CALL", 1)
STATION_2 = A("N0CALL", 2)
LONG_CALL = A("N0CALL/7")
BROADCAST = A.broadcast_address()
ALL_BUT = bitmap_vectors._all_but
W24 = frozenset(range(24))


# --- headers ------------------------------------------------------------------

HEADER_CASES: tuple[tuple[str, str, fields.Header], ...] = (
    ("beacon", "BEACON to broadcast with an ID control block and one data block: a complete message "
     "(flag bit 6 set, bit 2 clear); sequence is the frame counter",
     fields.Header(T.BEACON, STATION_1, BROADCAST, sequence=513, n_control=1, n_data=1, first_fragment=True)),
    ("ui_fragment_repeat", "UI first fragment (flag bits 6 and 2) repeated (bit 5), MCS 3, the maximum "
     "128 data blocks and frame counter 65535",
     fields.Header(T.UI, A("N0CALL", 15), BROADCAST, sequence=65535, mcs=3, n_data=128, more_fragments=True,
                   repeat=True, first_fragment=True)),
    ("ui_middle_fragment", "UI middle fragment of a message: flag bit 2 set, bit 6 clear, frame counter 0",
     fields.Header(T.UI, A("N0CALL", 15), BROADCAST, sequence=0, mcs=1, n_data=4, more_fragments=True)),
    ("connect", "CONNECT with a width probe and two control blocks (B.3, ID), no data, sequence 0 "
     "(next unsent block of a new session)",
     fields.Header(T.CONNECT, STATION_1, STATION_2, n_control=2, probe=True)),
    ("connect_ack", "CONNECT-ACK in session with a probe and its B.3 and ID blocks; unmasked, mask ID 0",
     fields.Header(T.CONNECT_ACK, STATION_2, STATION_1, n_control=2, session=True, probe=True)),
    ("data", "DATA in session at MCS 6 with three control blocks and 42 data blocks, mask ID 1",
     fields.Header(T.DATA, STATION_1, STATION_2, sequence=40000, mcs=6, n_control=3, n_data=42, session=True,
                   mask_id=1)),
    ("data_block_map", "DATA in session with a block map (flag bit 7) as its second of two control blocks, "
     "5 data blocks at MCS 3, mask ID 3",
     fields.Header(T.DATA, STATION_1, STATION_2, sequence=65533, mcs=3, n_control=2, n_data=5, session=True,
                   block_map=True, mask_id=3)),
    ("ack", "ACK frame: one control block, no data, the sender's next unsent sequence, mask ID 2",
     fields.Header(T.ACK, STATION_2, STATION_1, sequence=17, n_control=1, session=True, mask_id=2)),
    ("probe_long_callsign", "BEACON-request PROBE from a 7-character callsign (code 63 and its 30-bit "
     "hash); sequence is the frame counter",
     fields.Header(T.PROBE, LONG_CALL, A("N0CALL", 0), sequence=4242)),
    ("extended_mcs_ignored", "EXTENDED (type 31) with no data blocks: the MCS field (here a reserved "
     "rung, 15) is sent as given and ignored", fields.Header(T.EXTENDED, STATION_1, STATION_2, mcs=15,
                                                            n_control=1)),
)


def _corrupt(h: fields.Header, edit) -> np.ndarray:
    """Header bits with ``edit`` applied to the 128 message bits and the CRC recomputed."""
    msg = h.message_bits().copy()
    edit(msg)
    return np.concatenate([msg, int_to_bits(crc16_bits(msg), 16)])


def _set(msg: np.ndarray, at: int, width: int, value: int) -> None:
    msg[at:at + width] = int_to_bits(value, width)


_CONNECT = HEADER_CASES[3][2]
REJECTED_HEADERS: tuple[tuple[str, str, np.ndarray], ...] = (
    ("reject_crc", "connect with its last CRC bit inverted: CRC failure",
     np.concatenate([_CONNECT.bits()[:-1], 1 - _CONNECT.bits()[-1:]])),
    ("reject_flag0_mismatch", "connect with flag bit 0 cleared while the control block count is 2 "
     "(SPEC 8.2: discard)", _corrupt(_CONNECT, lambda m: _set(m, 15, 1, 0))),
    ("reject_version", "connect with version 2", _corrupt(_CONNECT, lambda m: _set(m, 0, 3, 2))),
    ("reject_address_code", "connect with source character 1 set to code 39 (reserved)",
     _corrupt(_CONNECT, lambda m: _set(m, 16, 6, 39))),
)


def header_doc(name: str, description: str, h: fields.Header | None, bits: np.ndarray) -> dict:
    doc = {"format": "cairn-header-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
           "name": name, "description": description, "valid": h is not None}
    if h is not None:
        doc["fields"] = header_json(h)
    else:
        try:
            fields.Header.unpack(bits)
            raise AssertionError(f"{name} unpacks")
        except fields.CrcError:
            doc["error"] = "crc"
        except fields.FieldError:
            doc["error"] = "fields"
    doc["message_hex"] = bits_to_hex(bits[:-16])
    doc["info_hex"] = bits_to_hex(bits)
    return doc


# --- control blocks ----------------------------------------------------------------

CONTROL_CASES: tuple[tuple[str, str, object], ...] = (
    ("ack", "ACK in a session: next in-order block 1200, blocks 1202 and 1205 held beyond it, tone mask "
     "without tone 9 under mask ID 1",
     fields.AckBlock(*fields.AckBlock.cumulative(1200, [1202, 1205]), requested_mcs=2,
                     snr_q=fields.snr_code(14.6), block_errors=1, tone_mask=frozenset(ALL_BUT(9)), mask_id=1)),
    ("ack_mask_flags", "ACK with every field set: tone mask without tones 9 and 25 (mask ID 3), re-probe, "
     "long prefix, saturated SNR, 255 block errors, distortion 5, ADC clipped, blocks base+1 and "
     "base+127 held",
     fields.AckBlock(base_sequence=65535, bitmap=(1 << 127) | 2, requested_mcs=6, snr_q=fields.snr_code(80.0),
                     block_errors=255, tone_mask=frozenset(ALL_BUT(9, 25)), reprobe=True, long_prefix=True,
                     distortion=5, adc_clipped=True, mask_id=3)),
    ("ack_refusal", "ACK with every field zero: the first block of a refusal DISCONNECT (ADR 0010 "
     "connect-refusal)", fields.AckBlock()),
    ("connect", "CONNECT: all-ones tone mask, widths above 24 and calibration, cap 55, 4 s frames, "
     "requested MCS 0, two dictionaries and a second block of them to follow",
     fields.ConnectBlock(session_id=0xBEEF, capabilities=fields.CAP_WIDE | fields.CAP_CALIBRATION,
                         width_cap=55, requested_mcs=0, max_frame_time=40,
                         dictionary_ids=(0x01020304, 0xA0B0C0D0, 0, 0), more_dictionaries=True)),
    ("connect_dictionaries", "The second B.3 block: dictionary IDs only, every other field zero (width "
     "cap and max frame time 0 mark it)", fields.ConnectBlock.dictionaries((0x11223344, 0x55667788,
                                                                             0x99AABBCC, 0xDDEEFF00))),
    ("connect_ack_resume", "CONNECT-ACK resuming a session: usable tones 0-29 less 9, cap 23, 2 s "
     "frames", fields.ConnectBlock(session_id=0x0042, capabilities=fields.CAP_SIGNATURES, width_cap=23,
                                   requested_mcs=3, max_frame_time=20,
                                   tone_mask=frozenset(range(30)) - {9}, resume=True)),
    ("cal_request", "CAL-REQUEST with the B.5 defaults", fields.CalRequestBlock()),
    ("cal_report", "CAL-REPORT: eight steps, steps 7 and 8 clipped, sweep valid",
     fields.CalReportBlock(amplitudes=(120, 108, 96, 84, 72, 61, 55, 54), distortion=(1, 1, 1, 2, 2, 4, 9, 15),
                           clipped=(False,) * 6 + (True, True), sweep_valid=True)),
    ("id_grid", "ID: a 7-character callsign with a grid locator",
     fields.IdBlock("N0CALL/7", 0, "FN31PR")),
    ("id_no_grid", "ID: callsign and SSID, no grid locator", fields.IdBlock("N0CALL", 1)),
    ("path", "Path: hops 2, entry 1 a WIDE1-1 alias already used, entry 2 a station",
     fields.PathBlock(entries=(A("WIDE1", 1), A("N0CALL", 3)), hops=2, used=(True, False))),
)


def _control_with(msg_edit, base) -> np.ndarray:
    msg = fields.control_message_bits(base).copy()
    msg_edit(msg)
    return np.concatenate([msg, int_to_bits(crc16_bits(msg), 16)])


REJECTED_CONTROLS: tuple[tuple[str, str, str, np.ndarray], ...] = (
    ("reject_crc", "ack with its first CRC bit inverted: CRC failure", "crc",
     (lambda b: np.concatenate([b[:240], 1 - b[240:241], b[241:]]))(fields.control_bits(CONTROL_CASES[0][2]))),
    ("reject_width_cap", "connect with width cap 10 (outside 23-55, max frame time not 0): malformed", "fields",
     _control_with(lambda m: _set(m, 40, 6, 10), CONTROL_CASES[3][2])),
    ("skip_reserved_type", "ack bits with the control type set to 8 (reserved since type 7 became the "
     "block map): receivers skip the block and go on", "unknown",
     _control_with(lambda m: _set(m, 0, 4, 8), CONTROL_CASES[0][2])),
)


def control_doc(name: str, description: str, block, bits: np.ndarray, error: str | None = None) -> dict:
    doc = {"format": "cairn-control-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
           "name": name, "description": description, "valid": block is not None}
    if block is not None:
        doc["fields"] = control_json(block)
    else:
        doc["error"] = error
        try:
            out = fields.unpack_control(bits)
            assert error == "unknown" and isinstance(out, fields.UnknownControl), name
        except fields.CrcError:
            assert error == "crc", name
        except fields.FieldError:
            assert error == "fields", name
    doc["message_hex"] = bits_to_hex(bits[:-16])
    doc["info_hex"] = bits_to_hex(bits)
    return doc


# --- whole frames -----------------------------------------------------------------

_P = "cairn-frame-vector"


def _ack(base: int, held=(), mask=W24, mask_id: int = 0, **kw) -> fields.AckBlock:
    """A session ACK block: cumulative, mask always present (ADR 0008, ADR 0011)."""
    b, bitmap = fields.AckBlock.cumulative(base, held)
    return fields.AckBlock(b, bitmap, tone_mask=frozenset(mask), mask_id=mask_id, **kw)


_ACK = _ack(300, [302, 303], requested_mcs=3, snr_q=fields.snr_code(16.25), block_errors=1)
_ACK_MASK = _ack(7, [8], mask=ALL_BUT(1, 9, 25), mask_id=1, requested_mcs=3, snr_q=fields.snr_code(19.0))
_W30_MASK = ALL_BUT(1, 9, 25, 40)

FRAME_CASES: tuple[Case, ...] = (
    Case("connect_w30", 30, 0, 0, prefix=_P,
         description="CONNECT, new session: sequence 0, probe to the width cap + 1 = 30 tones, B.3 "
                     "(requested MCS 0) then ID",
         header=fields.Header(T.CONNECT, STATION_1, STATION_2, n_control=2, probe=True),
         controls=(fields.ConnectBlock(session_id=0x5A5A, capabilities=fields.CAP_WIDE, width_cap=29,
                                       requested_mcs=0, max_frame_time=40),
                   fields.IdBlock("N0CALL", 1, "FN31"))),
    Case("connect_ack_resume_probe", 24, 0, 0, prefix=_P,
         description="CONNECT-ACK resuming a session (ADR 0008 session-resume-rules): B.3 with flag bit 1, "
                     "ID, then the ACK block for the initiator-to-responder direction; probe to W = 24",
         header=fields.Header(T.CONNECT_ACK, STATION_2, STATION_1, sequence=812, n_control=3, session=True,
                              probe=True),
         controls=(fields.ConnectBlock(session_id=0x5A5A, width_cap=23, requested_mcs=3, max_frame_time=40,
                                       tone_mask=W24, resume=True),
                   fields.IdBlock("N0CALL", 2),
                   _ack(1000, [1001, 1003], requested_mcs=3, snr_q=fields.snr_code(18.0)))),
    Case("data_ack_w24_mcs2", 24, 2, 2, prefix=_P,
         description="DATA at 24 tones, MCS 2: the ACK block, then blocks 301 and 302",
         header=fields.Header(T.DATA, STATION_1, STATION_2, sequence=301, mcs=2, n_control=1, n_data=2,
                              session=True),
         controls=(_ACK,)),
    Case("data_ack_w30_mcs3_long_masked", 30, 3, 3, cp=params.CP_LONG, usable=_W30_MASK, prefix=_P,
         description="DATA at W = 30, MCS 3, long prefix, tones 1, 9 and 25 masked under mask ID 1 (40 is "
                     "above the width); probe to 30",
         header=fields.Header(T.DATA, STATION_2, STATION_1, sequence=9, mcs=3, n_control=1, n_data=3,
                              session=True, probe=True, mask_id=1),
         controls=(_ACK_MASK,)),
    Case("data_three_controls_w56_mcs4", 56, 4, 2, prefix=_P,
         description="DATA at W = 56, MCS 4 with ACK, ID and CAL-REPORT blocks (ADR 0010 control-block-order)",
         header=fields.Header(T.DATA, STATION_1, STATION_2, sequence=12, mcs=4, n_control=3, n_data=2,
                              session=True, probe=True),
         controls=(_ACK, fields.IdBlock("N0CALL", 1),
                   fields.CalReportBlock(amplitudes=(90,) * 8, distortion=(2,) * 8, sweep_valid=True))),
    Case("ack", 24, 0, 0, prefix=_P, samples=True,
         description="ACK frame: the ACK block alone, the sender's next unsent sequence",
         header=fields.Header(T.ACK, STATION_2, STATION_1, sequence=14, n_control=1, session=True),
         controls=(_ACK,)),
)


# --- link vectors (ADRs 0008-0012) ------------------------------------------------

_P_LINK = "cairn-link-vector"

BLOCK_MAP_CASES: tuple[tuple[str, str, fields.BlockMapBlock], ...] = (
    ("block_map_retransmit", "Block map of a frame carrying retransmissions 300 and 302 and new blocks "
     "305-307 (base = header Sequence 300)", fields.BlockMapBlock.for_sequences([300, 302, 305, 306, 307])),
    ("block_map_wrap", "Block map across the sequence wrap: blocks 65533, 65535, 0 and 124 (bit 127)",
     fields.BlockMapBlock.for_sequences([65533, 65535, 0, 124])),
)

_MAP_SEQUENCES = (65533, 65535, 0, 2, 3)
_MAP_SEQ, _MAP = fields.plan_blocks(_MAP_SEQUENCES)
_SOUND_MASK = ALL_BUT(*range(30, 56), 27)        # tones 0-29 less 27 measured usable

LINK_FRAME_CASES: tuple[Case, ...] = (
    Case("data_block_map_w24_mcs3", 24, 3, len(_MAP_SEQUENCES), usable=ALL_BUT(5, 17), prefix=_P_LINK,
         description="DATA with the ACK block and a block map (flag bit 7): blocks 65533, 65535, 0, 2, 3 "
                     "across the wrap, the header Sequence the lowest; tones 5 and 17 masked under mask ID 2",
         header=fields.Header(T.DATA, STATION_1, STATION_2, sequence=_MAP_SEQ, mcs=3, n_control=2,
                              n_data=len(_MAP_SEQUENCES), session=True, block_map=True, mask_id=2),
         controls=(_ack(40, [42], mask=ALL_BUT(3), mask_id=0, requested_mcs=3, snr_q=fields.snr_code(17.5)),
                   _MAP)),
    Case("connect_ack_w30_id", 30, 0, 0, prefix=_P_LINK,
         description="CONNECT-ACK of a new session (ADR 0010 session-establishment-steps): probe to the "
                     "responder's W = 30, B.3 (requested MCS, mask for the initiator-to-responder direction), "
                     "its ID block, then a dictionary-only B.3",
         header=fields.Header(T.CONNECT_ACK, STATION_2, STATION_1, n_control=3, session=True, probe=True),
         controls=(fields.ConnectBlock(session_id=0x5A5A, capabilities=fields.CAP_WIDE, width_cap=29,
                                       requested_mcs=4, max_frame_time=30, tone_mask=frozenset(ALL_BUT(9)),
                                       dictionary_ids=(0x01020304, 0, 0, 0), more_dictionaries=True),
                   fields.IdBlock("N0CALL", 2),
                   fields.ConnectBlock.dictionaries((0xA0B0C0D0, 0x0BADCAFE, 0, 0)))),
    Case("probe_link_test_w40", 40, 0, 0, prefix=_P_LINK,
         description="Link-test PROBE (ADR 0012): frame counter in the Sequence, one link-test B.3, probe to "
                     "its width cap + 1 = 40",
         header=fields.Header(T.PROBE, STATION_1, STATION_2, sequence=4660, n_control=1, probe=True),
         controls=(fields.ConnectBlock.link_test(fields.CAP_WIDE | fields.CAP_CALIBRATION, 39, 40),)),
    Case("sound_probe_reply_w32", 32, 0, 0, prefix=_P_LINK,
         description="SOUND answering the link-test PROBE (ADR 0012 probe-link-test-reply): session flag "
                     "clear, Sequence 0, probe to its own width cap + 1 = 32, one ACK block with its "
                     "measurements (base, bitmap, block errors 0; mask of tones 0-29 less 27, mask ID 0)",
         header=fields.Header(T.SOUND, STATION_2, STATION_1, n_control=1, probe=True),
         controls=(fields.AckBlock(requested_mcs=4, snr_q=fields.snr_code(23.25), tone_mask=frozenset(_SOUND_MASK),
                                   distortion=1),)),
    Case("probe_beacon_request", 24, 0, 0, prefix=_P_LINK, samples=True,
         description="BEACON-request PROBE (ADR 0012): no control blocks, no probe, frame counter 4661",
         header=fields.Header(T.PROBE, STATION_1, STATION_2, sequence=4661)),
)


def _seeded(label: str, n: int) -> bytes:
    out, i = b"", 0
    while len(out) < n:
        out += hashlib.sha256(f"{_P_LINK}:{label}:{i}".encode()).digest()
        i += 1
    return out[:n]


def _pdu_json(p: pdu.Pdu) -> dict:
    return {"type": p.type, "stream": p.stream, "encoding": p.encoding,
            "dictionary_id_hex": None if p.dictionary_id is None else p.dictionary_id.hex(),
            "length": p.length, "body_hex": p.body.hex(), "crc32_hex": f"{p.crc:08x}", "pdu_hex": p.encode().hex()}


def _file_chunk_body() -> bytes:
    # FILE-CHUNK body: 32-bit object ID and 32-bit byte offset, big-endian (ADR 0009 pdu-byte-order).
    return (0x0000BEEF).to_bytes(4, "big") + (4096).to_bytes(4, "big") + _seeded("file-chunk", 292)


_DECODED = b"Dictionary-compressed text stands in here; the CRC-32 covers this decoded body."
PDU_GROUPS: tuple[tuple[pdu.Pdu, ...], ...] = (
    (pdu.Pdu.raw(pdu.PduType.MESSAGE, _seeded("message", 116)),          # 125 bytes: one byte left, filler
     pdu.Pdu.raw(pdu.PduType.FILE_CHUNK, _file_chunk_body(), pdu.stream_byte(3, 2)),   # spans three blocks
     pdu.Pdu(pdu.PduType.AX25, _seeded("compressed", 40), pdu.crc32(_DECODED), 0, pdu.ENCODING_DICT,
             hashlib.sha256(b"cairn example dictionary").digest()[:4])),
    (pdu.Pdu.raw(pdu.PduType.CONTROL, bytes([0, 1, 2, 3]), pdu.stream_byte(1, 5)),
     pdu.Pdu.raw(pdu.PduType.MESSAGE, b"")),
)


def pdu_blocks_doc() -> dict:
    blocks, pdus, starts = [], [], []
    for g, group in enumerate(PDU_GROUPS):
        packer = pdu.BlockPacker()
        for p in group:
            at = len(blocks) * pdu.BLOCK_BYTES + packer.queued
            if packer.queued % pdu.BLOCK_BYTES == pdu.BLOCK_BYTES - 1:
                at += 1
            packer.push(p)
            pdus.append({"group": g, **_pdu_json(p)})
            starts.append(at)
        while (b := packer.take()) is not None:
            blocks.append(b)
    for d, at in zip(pdus, starts):
        d["block"], d["offset"] = divmod(at, pdu.BLOCK_BYTES)
    back = pdu.unpack_blocks(blocks)
    assert [x.encode().hex() for x in back] == [d["pdu_hex"] for d in pdus]
    return {"format": "cairn-pdu-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
            "name": "pdu_blocks",
            "description": "PDUs packed into 126-byte block payloads (ADR 0009): each group is packed from a "
                           "block boundary and its last block completed with filler (the sender ran out of "
                           "PDUs); a PDU that would leave one byte is followed by a 0x00 filler byte; one "
                           "PDU spans three blocks; one uses encoding 2 (Dictionary ID; its body stands in "
                           "for compressed bytes and its CRC-32 covers decoded_hex)",
            "crc32_check": {"input_ascii": "123456789", "crc32_hex": f"{pdu.crc32(b'123456789'):08x}"},
            "decoded_hex": _DECODED.hex(),
            "pdus": pdus,
            "blocks_hex": [b.hex() for b in blocks],
            "block_info_hex": [bits_to_hex(np.concatenate([pdu.block_bits(b),
                                                          int_to_bits(crc16_bits(pdu.block_bits(b)), 16)]))
                               for b in blocks]}


_MESSAGE = (b"N0CALL bulletin: " + b"The quick brown fox jumps over the lazy dog. " * 11)[:500]
_SLICE = 200
_FIRST_COUNTER = 65534


def pdu_fragments_doc() -> dict:
    frags = pdu.fragment(pdu.PduType.MESSAGE, _MESSAGE, _SLICE, pdu.stream_byte(2, 1))
    frames, r, done = [], pdu.Reassembler(), None
    for i, f in enumerate(frags):
        counter = (_FIRST_COUNTER + i) % fields.SEQUENCE_MODULUS
        blocks = pdu.pack_blocks([f.pdu])
        h = fields.Header(T.UI, STATION_1, BROADCAST, sequence=counter, mcs=1, n_data=len(blocks),
                          more_fragments=f.more, first_fragment=f.first)
        frames.append({"sequence": counter, "first_fragment": f.first, "more_fragments": f.more,
                       "header": header_json(h), "header_info_hex": bits_to_hex(h.bits()),
                       "pdu": _pdu_json(f.pdu), "blocks_hex": [b.hex() for b in blocks]})
        done = r.feed(STATION_1, counter, f.first, f.more, f.pdu)
    assert done is not None and done.body == _MESSAGE
    return {"format": "cairn-pdu-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
            "name": "pdu_broadcast_fragments",
            "description": "A 500-byte MESSAGE sent as UI frames of one complete PDU each, 200-byte slices "
                           "(ADR 0009 broadcast-message-fragments): consecutive frame counters across the "
                           "wrap, flag bit 6 on the first frame, bit 2 on all but the last",
            "message_hex": _MESSAGE.hex(), "slice_bytes": _SLICE,
            "reassembled": _pdu_json(done), "frames": frames}


# --- output ----------------------------------------------------------------------------


def _text(doc: dict) -> str:
    return bitmap_vectors._dump(doc) + "\n"


def _check(case: Case) -> None:
    fields.check_controls(case.header, case.controls)


def documents() -> dict[str, str]:
    """Relative path -> file text for every vector this module writes."""
    out = {}
    for name, desc, h in HEADER_CASES:
        out[f"header/{name}.json"] = _text(header_doc(name, desc, h, h.bits()))
    for name, desc, bits in REJECTED_HEADERS:
        out[f"header/{name}.json"] = _text(header_doc(name, desc, None, bits))
    for name, desc, block in CONTROL_CASES:
        out[f"control/{name}.json"] = _text(control_doc(name, desc, block, fields.control_bits(block)))
    for name, desc, error, bits in REJECTED_CONTROLS:
        out[f"control/{name}.json"] = _text(control_doc(name, desc, None, bits, error))
    for case in FRAME_CASES:
        _check(case)
        out[f"frame/{case.name}.json"] = bitmap_vectors.case_text(case)
    for name, desc, block in BLOCK_MAP_CASES:
        out[f"link/{name}.json"] = _text(control_doc(name, desc, block, fields.control_bits(block)))
    for case in LINK_FRAME_CASES:
        _check(case)
        out[f"link/{case.name}.json"] = bitmap_vectors.case_text(case)
    out["link/pdu_blocks.json"] = _text(pdu_blocks_doc())
    out["link/pdu_broadcast_fragments.json"] = _text(pdu_fragments_doc())
    return out


def generate(root: Path) -> list[Path]:
    written = []
    for rel, text in documents().items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, newline="\n")
        written.append(path)
    return written


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=params.CODES_DIR.parent / "vectors")
    for p in generate(ap.parse_args().root):
        print(p)


if __name__ == "__main__":
    main()
