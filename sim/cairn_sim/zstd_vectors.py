"""zstd body vectors (SPEC 12.1-12.2, ADR 0014 `zstd-frame-format`).

Compressed bodies for fixed inputs, with and without a dictionary, written
the ADR's way by ``dict_eval.Codec`` (python-zstandard, libzstd 1.5.7, level
19: magic number present, no content checksum, no in-frame dictionary ID, no
content size), each as a whole PDU too. Further frames carry what a
receiver must also accept (content size, in-frame dictionary ID, checksum)
or must refuse (no magic number, trailing bytes, a body that decodes past a
bound).

    python -m cairn_sim.zstd_vectors            # writes vectors/zstd/*.json
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

import zstandard as zstd

from . import params
from . import pdu as pdu_mod
from .bitmap_vectors import _dump
from .dict_eval import EVAL_DICT_DIR, EVAL_LEVEL, Codec, dictionary_id

DICT_FILE = "eval-forms-64k.zdict"

# Fixed inputs: an ICS-213-style message in the style of the corpus (written
# for this file), a short status line, and incompressible bytes.
ICS213 = (b"ICS 213 GENERAL MESSAGE. TO: Planning Section Chief. FROM: Communications Unit Leader. "
          b"SUBJECT: Repeater status. MESSAGE: The county repeater on 146.940 is back on emergency power; "
          b"net control will resume at 1900 local. Please relay to all field teams. "
          b"APPROVED: J. Smith, COML.")
STATUS = b"N0CALL-1 status: shelter 3 open, 42 occupants, water and power OK, next report 1800."
RANDOM = hashlib.sha256(b"cairn-zstd-vector:random").digest() * 4   # 128 bytes, repeating every 32


def _pdu(body: bytes, decoded: bytes, encoding: int, did: bytes | None) -> dict:
    p = pdu_mod.Pdu(pdu_mod.PduType.MESSAGE, body, pdu_mod.crc32(decoded), 0, encoding, did)
    return {"encoding": encoding, "dictionary_id_hex": did.hex() if did else None,
            "crc32_hex": f"{p.crc:08x}", "pdu_hex": p.encode().hex()}


def _header(frame: bytes) -> dict:
    fhd = frame[4]
    return {"descriptor_hex": f"{fhd:02x}", "content_size_flag": fhd >> 6, "single_segment": bool(fhd & 0x20),
            "checksum": bool(fhd & 0x04), "dictionary_id_flag": fhd & 3}


def documents(dict_dir: Path = EVAL_DICT_DIR) -> dict[str, str]:
    d = (dict_dir / DICT_FILE).read_bytes()
    did = dictionary_id(d)
    dict_info = {"file": f"dictionaries/eval/{DICT_FILE}", "sha256": hashlib.sha256(d).hexdigest(),
                 "dictionary_id_hex": did.hex()}
    plain, with_dict = Codec(EVAL_LEVEL), Codec(EVAL_LEVEL, d)

    sent = []
    for name, msg in (("ics213", ICS213), ("status", STATUS), ("random", RANDOM)):
        for codec, enc in ((plain, 1), (with_dict, 2)):
            body = codec.compress(msg)
            assert codec.decompress(body, len(msg)) == msg
            sent.append({"name": f"{name}_{'dict' if enc == 2 else 'zstd'}", "input_hex": msg.hex(),
                         "input_bytes": len(msg), "body_hex": body.hex(), "body_bytes": len(body),
                         "header": _header(body), **_pdu(body, msg, enc, did if enc == 2 else None)})

    # Frames a receiver accepts: library defaults (content size, in-frame
    # dictionary ID), and the checksum on as well.
    accept = []
    for name, cs, did_in, ck, dct in (("content_size", True, False, False, None),
                                      ("checksum", False, False, True, None),
                                      ("dict_library_defaults", True, True, False, d),
                                      ("dict_all_fields", True, True, True, d)):
        prm = zstd.ZstdCompressionParameters.from_level(
            EVAL_LEVEL, write_content_size=int(cs), write_checksum=int(ck), write_dict_id=int(did_in))
        c = zstd.ZstdCompressor(dict_data=zstd.ZstdCompressionDict(dct) if dct else None, compression_params=prm)
        body = c.compress(ICS213)
        accept.append({"name": name, "input_hex": ICS213.hex(), "body_hex": body.hex(), "header": _header(body),
                       "dictionary": dct is not None})

    # Bodies a receiver refuses. The bound case decodes to 65,529 bytes, one
    # more than the largest raw body (65,535 - 7).
    good = plain.compress(ICS213)
    big = bytes(65_529)
    big_frame = zstd.ZstdCompressor(compression_params=zstd.ZstdCompressionParameters.from_level(
        EVAL_LEVEL, write_content_size=0, write_checksum=0, write_dict_id=0)).compress(big)
    big_frame_sized = zstd.ZstdCompressor(level=EVAL_LEVEL, write_checksum=False).compress(big)
    reject = [
        {"name": "no_magic", "body_hex": good[4:].hex(), "error": "missing_magic"},
        {"name": "magicless_format", "body_hex": zstd.ZstdCompressor(compression_params=zstd.ZstdCompressionParameters
                                                                     .from_level(EVAL_LEVEL, format=zstd.FORMAT_ZSTD1_MAGICLESS,
                                                                                 write_content_size=0, write_checksum=0,
                                                                                 write_dict_id=0)).compress(ICS213).hex(),
         "error": "missing_magic"},
        {"name": "trailing_byte", "body_hex": (good + b"\x00").hex(), "error": "trailing_bytes"},
        {"name": "skippable_frame_after", "body_hex": (good + bytes.fromhex("502a4d1800000000")).hex(),
         "error": "trailing_bytes"},
        {"name": "over_bound", "body_hex": big_frame.hex(), "decoded_bytes": len(big), "error": "too_large"},
        {"name": "over_bound_content_size", "body_hex": big_frame_sized.hex(), "decoded_bytes": len(big),
         "error": "too_large"},
    ]

    doc = {
        "format": "cairn-zstd-vectors",
        "version": 1,
        "license": "CC BY 4.0",
        "library": f"python-zstandard {zstd.__version__} (libzstd {zstd.ZSTD_VERSION[0]}.{zstd.ZSTD_VERSION[1]}."
                   f"{zstd.ZSTD_VERSION[2]})",
        "level": EVAL_LEVEL,
        "decoded_body_max": pdu_mod.MAX_LENGTH - 7,
        "dictionary": dict_info,
        "sent": sent,
        "accept": accept,
        "reject": reject,
    }
    return {"zstd/bodies.json": _dump(doc) + "\n"}


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
