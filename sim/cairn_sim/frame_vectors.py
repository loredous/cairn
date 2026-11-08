"""Generate the header, control block and whole-frame vectors (SPEC 8.2, Appendix B).

``python -m cairn_sim.frame_vectors [--root DIR]`` writes, under
``vectors/`` (format in ``vectors/README.md``):

- ``header/<case>.json``: header fields and their 144 bits (SPEC 8.2,
  B.1), including headers a receiver must reject.
- ``control/<case>.json``: control block fields and their 256 bits for every
  Appendix B type, including blocks a receiver must reject or skip.
- ``frame/<case>.json``: whole frames built from real headers and control
  blocks, followed through every stage as in ``vectors/bitmap/``
  (``bitmap_vectors.case_doc``): a CONNECT, a CONNECT-ACK with a probe, DATA
  frames with an ACK control block (one masked, long prefix, wider than 24
  tones), a frame with three control blocks, an ACK frame and a header-only
  PROBE.

Output is deterministic; ``sim/tests/test_frame_vectors.py`` checks that
regenerating is byte-identical to the committed files.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import numpy as np

from . import bitmap_vectors, fields, params
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


# --- headers ------------------------------------------------------------------

HEADER_CASES: tuple[tuple[str, str, fields.Header], ...] = (
    ("beacon", "BEACON to broadcast with an ID control block and one data block; sequence is the "
     "frame counter", fields.Header(T.BEACON, STATION_1, BROADCAST, sequence=513, n_control=1, n_data=1)),
    ("ui_fragment_repeat", "UI with more-fragments and repeat flags, MCS 3, the maximum 128 data blocks "
     "and sequence 65535", fields.Header(T.UI, A("N0CALL", 15), BROADCAST, sequence=65535, mcs=3,
                                          n_data=128, more_fragments=True, repeat=True)),
    ("connect", "CONNECT with a width probe and two control blocks, no data",
     fields.Header(T.CONNECT, STATION_1, STATION_2, n_control=2, probe=True)),
    ("connect_ack", "CONNECT-ACK in session with a probe and one control block",
     fields.Header(T.CONNECT_ACK, STATION_2, STATION_1, n_control=1, session=True, probe=True)),
    ("data", "DATA in session at MCS 6 with three control blocks and 42 data blocks",
     fields.Header(T.DATA, STATION_1, STATION_2, sequence=40000, mcs=6, n_control=3, n_data=42, session=True)),
    ("ack", "ACK frame: one control block, no data",
     fields.Header(T.ACK, STATION_2, STATION_1, sequence=0, n_control=1, session=True)),
    ("probe_long_callsign", "PROBE from a 7-character callsign (code 63 and its 30-bit hash)",
     fields.Header(T.PROBE, LONG_CALL, A("N0CALL", 0))),
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


_CONNECT = HEADER_CASES[2][2]
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
    ("ack", "ACK: base sequence, bitmap of blocks 0-2 with block 1 failed, no tone mask",
     fields.AckBlock(base_sequence=1200, bitmap=fields.AckBlock.bitmap_from([True, False, True]),
                     requested_mcs=2, snr_q=fields.snr_code(14.6), block_errors=1)),
    ("ack_mask_flags", "ACK with every field set: tone mask without tones 9 and 25, re-probe, long "
     "prefix, saturated SNR, 255 block errors, distortion 5, ADC clipped, block 127 and 0 passed",
     fields.AckBlock(base_sequence=65535, bitmap=(1 << 127) | 1, requested_mcs=6, snr_q=fields.snr_code(80.0),
                     block_errors=255, tone_mask=frozenset(ALL_BUT(9, 25)), reprobe=True, long_prefix=True,
                     distortion=5, adc_clipped=True)),
    ("connect", "CONNECT: all-ones tone mask, widths above 24 and calibration, cap 55, 4 s frames, "
     "two dictionaries and a second block of them to follow",
     fields.ConnectBlock(session_id=0xBEEF, capabilities=fields.CAP_WIDE | fields.CAP_CALIBRATION,
                         width_cap=55, requested_mcs=1, max_frame_time=40,
                         dictionary_ids=(0x01020304, 0xA0B0C0D0, 0, 0), more_dictionaries=True)),
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
    ("reject_width_cap", "connect with width cap 10 (outside 23-55): malformed", "fields",
     _control_with(lambda m: _set(m, 40, 6, 10), CONTROL_CASES[2][2])),
    ("skip_reserved_type", "ack bits with the control type set to 7 (reserved): receivers skip the "
     "block and go on", "unknown", _control_with(lambda m: _set(m, 0, 4, 7), CONTROL_CASES[0][2])),
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

_ACK = fields.AckBlock(base_sequence=300, bitmap=fields.AckBlock.bitmap_from([True] * 5 + [False] + [True] * 2),
                       requested_mcs=3, snr_q=fields.snr_code(16.25), block_errors=1)
_ACK_MASK = fields.AckBlock(base_sequence=7, bitmap=0b11, requested_mcs=3, snr_q=fields.snr_code(19.0),
                            tone_mask=frozenset(ALL_BUT(1, 9, 25)))
_P = "cairn-frame-vector"

FRAME_CASES: tuple[Case, ...] = (
    Case("connect_w30", 30, 0, 0, prefix=_P,
         header=fields.Header(T.CONNECT, STATION_1, STATION_2, n_control=2, probe=True),
         controls=(fields.ConnectBlock(session_id=0x5A5A, capabilities=fields.CAP_WIDE, width_cap=29,
                                       requested_mcs=2, max_frame_time=40),
                   fields.IdBlock("N0CALL", 1, "FN31"))),
    Case("connect_ack_resume_probe", 24, 0, 0, prefix=_P,
         header=fields.Header(T.CONNECT_ACK, STATION_2, STATION_1, n_control=2, session=True, probe=True),
         controls=(fields.ConnectBlock(session_id=0x5A5A, width_cap=23, requested_mcs=3, max_frame_time=40,
                                       tone_mask=frozenset(range(24)), resume=True),
                   fields.AckBlock(base_sequence=1000, bitmap=(1 << 128) - 1, requested_mcs=3,
                                   snr_q=fields.snr_code(18.0)))),
    Case("data_ack_w24_mcs2", 24, 2, 2, prefix=_P,
         header=fields.Header(T.DATA, STATION_1, STATION_2, sequence=301, mcs=2, n_control=1, n_data=2,
                              session=True),
         controls=(_ACK,)),
    Case("data_ack_w30_mcs3_long_masked", 30, 3, 3, cp=params.CP_LONG, usable=ALL_BUT(1, 9, 25, 40), prefix=_P,
         header=fields.Header(T.DATA, STATION_2, STATION_1, sequence=9, mcs=3, n_control=1, n_data=3,
                              session=True, probe=True),
         controls=(_ACK_MASK,)),
    Case("data_three_controls_w56_mcs4", 56, 4, 2, prefix=_P,
         header=fields.Header(T.DATA, STATION_1, STATION_2, sequence=12, mcs=4, n_control=3, n_data=2,
                              session=True, probe=True),
         controls=(_ACK, fields.IdBlock("N0CALL", 1),
                   fields.CalReportBlock(amplitudes=(90,) * 8, distortion=(2,) * 8, sweep_valid=True))),
    Case("ack", 24, 0, 0, prefix=_P, samples=True,
         header=fields.Header(T.ACK, STATION_2, STATION_1, n_control=1, session=True),
         controls=(_ACK,)),
    Case("probe_header_only", 24, 0, 0, prefix=_P, samples=True,
         header=fields.Header(T.PROBE, STATION_1, STATION_2)),
)


# --- output ----------------------------------------------------------------------------


def _text(doc: dict) -> str:
    return bitmap_vectors._dump(doc) + "\n"


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
        out[f"frame/{case.name}.json"] = bitmap_vectors.case_text(case)
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
