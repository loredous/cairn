"""Published header, control block, whole-frame and link vectors (SPEC 8.2,
Appendix B, ADR 0005, ADRs 0008-0012): reproducible, packed as the SPEC tables
say (written out here independently of ``fields``), parsed back by
``fields``, and decodable by the header-first receiver; PDU packing written
out independently of ``pdu``."""

import hashlib
import json

import numpy as np
import pytest

from cairn_sim import crc, fields, frame, frame_vectors, params, pdu, vectors
from cairn_sim.bitmap_vectors import control_json, header_json
from test_bitmap_vectors import check_frame_doc

ROOT = params.CODES_DIR.parent / "vectors"


def load(rel: str) -> dict:
    return json.loads((ROOT / rel).read_text())


def names(kind: str) -> list[str]:
    return sorted(p.stem for p in (ROOT / kind).glob("*.json"))


def test_regeneration_is_byte_identical(tmp_path):
    written = frame_vectors.generate(tmp_path)
    for kind in ("header", "control", "frame", "link"):
        assert sorted(p.stem for p in written if p.parent.name == kind) == names(kind), kind
    for path in written:
        rel = path.relative_to(tmp_path)
        assert path.read_bytes() == (ROOT / rel).read_bytes(), rel


# --- the SPEC tables, written out -------------------------------------------------

ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/-"


def address(a: dict) -> int:
    """B.1: six 6-bit characters (code 1-26 A-Z, 27-36 0-9, 37 '/', 38 '-', 0 padding)
    then the SSID; code 63 and a 30-bit SHA-256 hash for 7-10 characters."""
    if a.get("broadcast"):
        v = (1 << 40) - 1
    elif len(a["callsign"]) > 6:
        h = int.from_bytes(hashlib.sha256(a["callsign"].encode()).digest()[:4], "big") >> 2
        h = h - 1 if h == (1 << 30) - 1 else h
        assert h == a["hash30"]
        v = (63 << 34) | (h << 4) | a["ssid"]
    else:
        codes = [ALPHABET.index(ch) + 1 for ch in a["callsign"]] + [0] * (6 - len(a["callsign"]))
        v = 0
        for c in codes:
            v = (v << 6) | c
        v = (v << 4) | a["ssid"]
    assert f"{v:010x}" == a["value_hex"]
    return v


def pack(layout: list[tuple[int, int]]) -> str:
    """(value, width) fields, big-endian, then CRC-16/CCITT-FALSE -> hex."""
    v, n = 0, 0
    for value, width in layout:
        assert 0 <= value < (1 << width), (value, width)
        v, n = (v << width) | value, n + width
    msg = v.to_bytes(n // 8, "big")
    return (msg + crc.crc16(msg).to_bytes(2, "big")).hex()


def mask(tones) -> int:
    return 0 if tones is None else sum(1 << k for k in tones)


def bitsn(flags: list[bool]) -> int:
    return sum(int(f) << i for i, f in enumerate(flags))


def header_layout(f: dict) -> list[tuple[int, int]]:
    """SPEC 8.2 / ADR 0011 header-field-layout: flag bits 0-7, then Mask ID (2) and one reserved bit."""
    flags = bitsn([f["control_blocks"] > 0, f["data_blocks"] > 0, f["more_fragments"], f["session"],
                   f["probe"], f["repeat"], f["first_fragment"], f["block_map"]])
    assert flags == f["flags"]
    return [(f["version"], 3), (f["frame_type"], 5), (flags, 8), (address(f["source"]), 40),
            (address(f["destination"]), 40), (f["sequence"], 16), (f["mcs"], 4), (f["control_blocks"], 2),
            (max(f["data_blocks"] - 1, 0), 7), (f["mask_id"], 2), (0, 1)]


def ascii_field(text: str | None, n: int) -> int:
    return int.from_bytes(text.ljust(n).encode(), "big") if text is not None else 0


def control_layout(f: dict) -> list[tuple[int, int]]:
    t = f["type"]
    if t == 0:
        return [(0, 4), (bitsn([f["tone_mask"] is not None, False, f["reprobe"], f["long_prefix"]]), 4),
                (f["base_sequence"], 16), (int(f["bitmap_hex"], 16), 128), (f["requested_mcs"], 4),
                (f["snr_q"], 8), (f["block_errors"], 8), (mask(f["tone_mask"]), 56), (f["distortion"], 3),
                (int(f["adc_clipped"]), 1), (f["mask_id"], 2), (0, 6)]
    if t == 1:
        return [(1, 4), (bitsn([f["more_dictionaries"], f["resume"]]), 4), (f["session_id"], 16),
                (f["capabilities"], 16), (f["width_cap"], 6), (f["requested_mcs"], 4), (f["max_frame_time"], 6),
                (mask(f["tone_mask"]), 56)] + [(d, 32) for d in f["dictionary_ids"]]
    if t == 3:
        return [(3, 4), (0, 4), (f["tone_freq"], 8), (f["step_count"], 4), (f["step_length"], 8),
                (f["gap_length"], 8), (f["start_level"], 8), (f["step_size"], 4), (0, 192)]
    if t == 4:
        return ([(4, 4), (int(f["sweep_valid"]), 4)] + [(a, 8) for a in f["amplitudes"]]
                + [(d, 4) for d in f["distortion"]] + [(bitsn(f["clipped"]), 8), (0, 128)])
    if t == 5:
        return [(5, 4), (int(f["grid"] is not None), 4), (ascii_field(f["callsign"], 10), 80), (f["ssid"], 4),
                (ascii_field(f["grid"], 6), 48), (0, 100)]
    if t == 6:
        entries = [address(a) for a in f["entries"]] + [0] * (2 - len(f["entries"]))
        return [(6, 4), (bitsn(f["used"] + [f["via"]]), 4), (f["hops"], 2), (len(f["entries"]), 2),
                (entries[0], 40), (entries[1], 40), (0, 148)]
    if t == 7:
        bitmap = int(f["bitmap_hex"], 16)
        assert f["sequences"] == [(f["base_sequence"] + i) % 65536 for i in range(128) if bitmap >> i & 1]
        return [(7, 4), (0, 4), (f["base_sequence"], 16), (bitmap, 128), (0, 88)]
    raise AssertionError(t)


# --- headers and control blocks ------------------------------------------------------


@pytest.mark.parametrize("name", names("header"))
def test_header_vector(name):
    d = load(f"header/{name}.json")
    b = vectors.hex_to_bits(d["info_hex"], 144)
    assert d["info_hex"].startswith(d["message_hex"])
    if not d["valid"]:
        with pytest.raises(fields.CrcError if d["error"] == "crc" else fields.FieldError):
            fields.Header.unpack(b)
        if d["error"] != "crc":
            assert crc.crc16(bytes.fromhex(d["message_hex"])) == int(d["info_hex"][-4:], 16)
        return
    assert pack(header_layout(d["fields"])) == d["info_hex"]
    h = fields.Header.unpack(b)
    assert np.array_equal(h.bits(), b)
    on_air = dict(d["fields"])
    for side in ("source", "destination"):
        if "hash30" in on_air[side]:
            on_air[side] = {**on_air[side], "callsign": None}
    assert header_json(h) == on_air


def control_names() -> list[str]:
    return [f"control/{n}" for n in names("control")] + [
        f"link/{n}" for n in names("link") if load(f"link/{n}.json")["format"] == "cairn-control-vectors"]


@pytest.mark.parametrize("name", control_names())
def test_control_vector(name):
    d = load(f"{name}.json")
    b = vectors.hex_to_bits(d["info_hex"], 256)
    if not d["valid"]:
        if d["error"] == "unknown":
            out = fields.unpack_control(b)
            t = int(d["message_hex"][0], 16)
            assert t in (2, *range(8, 16)) and isinstance(out, fields.UnknownControl) and out.control_type == t
        else:
            with pytest.raises(fields.CrcError if d["error"] == "crc" else fields.FieldError):
                fields.unpack_control(b)
        return
    assert pack(control_layout(d["fields"])) == d["info_hex"]
    c = fields.unpack_control(b)
    assert control_json(c) == d["fields"]
    assert np.array_equal(fields.control_bits(c), b)


def frame_names() -> list[str]:
    return [f"frame/{n}" for n in names("frame")] + [
        f"link/{n}" for n in names("link") if load(f"link/{n}.json")["format"] == "cairn-bitmap-vectors"]


def test_every_control_type_and_frame_kind_is_covered():
    types = {load(f"{n}.json")["fields"]["type"] for n in control_names() if load(f"{n}.json")["valid"]}
    assert types == {0, 1, 3, 4, 5, 6, 7}
    frames = [load(f"{n}.json") for n in frame_names()]
    kinds = {fr["header"]["fields"]["frame_type"] for fr in frames}
    assert {2, 3, 4, 5, 6, 8} <= kinds
    assert any(fr["header"]["fields"]["block_map"] for fr in frames)
    assert any(fr["header"]["fields"]["mask_id"] for fr in frames)
    assert any(fr["control_blocks"] == 0 and fr["data_blocks"] == 0 for fr in frames)       # header only
    assert any("probe" in {s["kind"] for s in fr["segments"]} and fr["control_blocks"] for fr in frames)
    assert any(fr["masked_tones"] and fr["control_blocks"] for fr in frames)
    assert any(fr["control_blocks"] == 3 and fr["data_blocks"] for fr in frames)


# --- whole frames ----------------------------------------------------------------


@pytest.mark.parametrize("name", frame_names())
def test_frame_vector_follows_the_rules(name):
    d = load(f"{name}.json")
    check_frame_doc(d)
    f = d["header"]["fields"]
    controls = [fields.unpack_control(vectors.hex_to_bits(c["info_hex"], 256)) for c in d.get("controls", [])]
    header = fields.Header.unpack(vectors.hex_to_bits(d["header"]["info_hex"], 144))
    fields.check_controls(header, controls)                  # ADR 0010 control-block-order, ADR 0012
    # ADR 0008: consecutive from the Sequence, or as the block map names them.
    if f["data_blocks"]:
        if f["block_map"]:
            m = d["controls"][1]["fields"]
            assert m["type"] == 7 and m["base_sequence"] == f["sequence"] and m["sequences"][0] == f["sequence"]
            assert d["block_sequences"] == m["sequences"] and len(m["sequences"]) == f["data_blocks"]
        else:
            assert d["block_sequences"] == [(f["sequence"] + k) % 65536 for k in range(f["data_blocks"])]
    # ADR 0011: the probe covers the frame's width (W in a session, the cap + 1 outside one).
    probe = [s for s in d["segments"] if s["kind"] == "probe"]
    if probe:
        assert probe[0]["tones"] == list(range(d["width"]))
    assert pack(header_layout(f)) == d["header"]["info_hex"]
    for c in d.get("controls", []):
        assert pack(control_layout(c["fields"])) == c["info_hex"]
    assert f["control_blocks"] == d["control_blocks"] and f["data_blocks"] == d["data_blocks"]
    assert ("probe" in {s["kind"] for s in d["segments"]}) == f["probe"]
    # SPEC 7.1: the data rung's clip level, 7 dB without data blocks.
    assert d["clip_db"] == (params.clip_db(f["mcs"]) if f["data_blocks"] else 7.0)


@pytest.mark.parametrize("name", [n for n in frame_names() if "samples" in load(f"{n}.json")])
def test_frame_vector_samples_decode_header_first(name):
    """The published samples decode with only the link width and mask known,
    the link state supplied once the header has decoded."""
    d = load(f"{name}.json")
    x = np.array(d["samples"])
    usable = None if d["usable_tones"] is None else frozenset(d["usable_tones"])
    rx = np.concatenate([np.zeros(700), x, np.zeros(700)])
    r = frame.receive(rx, link=lambda h: frame.LinkState(d["width"], usable, mask_id=h.mask_id))
    assert r.header is not None and r.config.cp == d["prefix_samples"]
    assert vectors.bits_to_hex(r.header_bits) == d["header"]["info_hex"]
    assert [vectors.bits_to_hex(b) for b in r.control_bits] == [c["info_hex"] for c in d.get("controls", [])]
    assert r.control_ok.all() and r.block_ok.all()


# --- PDU packing (ADR 0009), written out ------------------------------------------


def zlib_free_crc32(data: bytes) -> int:
    """CRC-32/ISO-HDLC bit by bit: reflected polynomial 0xEDB88320, init and final XOR all ones."""
    c = 0xFFFFFFFF
    for byte in data:
        c ^= byte
        for _ in range(8):
            c = (c >> 1) ^ (0xEDB88320 if c & 1 else 0)
    return c ^ 0xFFFFFFFF


def test_pdu_blocks_vector():
    d = load("link/pdu_blocks.json")
    assert zlib_free_crc32(b"123456789") == 0xCBF43926 == int(d["crc32_check"]["crc32_hex"], 16)
    blocks = [bytes.fromhex(b) for b in d["blocks_hex"]]
    assert all(len(b) == 126 for b in blocks)
    stream = b"".join(blocks)
    for p in d["pdus"]:
        raw = bytes.fromhex(p["pdu_hex"])
        dict_id = bytes.fromhex(p["dictionary_id_hex"]) if p["dictionary_id_hex"] else b""
        body = bytes.fromhex(p["body_hex"])
        assert (p["encoding"] == 2) == bool(dict_id)
        want = (p["length"].to_bytes(2, "big") + bytes([p["type"], p["stream"], p["encoding"]]) + dict_id + body
                + bytes.fromhex(p["crc32_hex"]))
        assert raw == want and p["length"] == len(raw) - 2
        crc_over = bytes.fromhex(d["decoded_hex"]) if p["encoding"] else body
        assert zlib_free_crc32(crc_over) == int(p["crc32_hex"], 16)
        at = p["block"] * 126 + p["offset"]
        assert stream[at:at + len(raw)] == raw
        # A Length field never straddles a block boundary.
        assert p["offset"] <= 124
    # Filler: between PDUs only zeros, a lone byte or a zero Length running to the block's end.
    covered = np.zeros(len(stream), bool)
    for p in d["pdus"]:
        at = p["block"] * 126 + p["offset"]
        covered[at:at + len(bytes.fromhex(p["pdu_hex"]))] = True
    assert not any(stream[i] for i in np.flatnonzero(~covered))
    assert [x.encode().hex() for x in pdu.unpack_blocks(blocks)] == [p["pdu_hex"] for p in d["pdus"]]
    for info, b in zip(d["block_info_hex"], blocks):
        assert info == (b + crc.crc16(b).to_bytes(2, "big")).hex()      # B.4: bytes MSB first, then CRC-16


def test_pdu_fragments_vector():
    d = load("link/pdu_broadcast_fragments.json")
    message = bytes.fromhex(d["message_hex"])
    frames = d["frames"]
    assert b"".join(bytes.fromhex(f["pdu"]["body_hex"]) for f in frames) == message
    for i, f in enumerate(frames):
        h = f["header"]
        assert (h["first_fragment"], h["more_fragments"]) == (i == 0, i < len(frames) - 1)
        assert h["sequence"] == (frames[0]["sequence"] + i) % 65536
        assert pack(header_layout(h)) == f["header_info_hex"]
        assert len(bytes.fromhex(f["pdu"]["body_hex"])) <= d["slice_bytes"]
        blocks = [bytes.fromhex(b) for b in f["blocks_hex"]]
        assert h["data_blocks"] == len(blocks)
        one = pdu.unpack_blocks(blocks)
        assert len(one) == 1 and one[0].encode().hex() == f["pdu"]["pdu_hex"] and one[0].crc_ok
        assert (f["pdu"]["type"], f["pdu"]["stream"]) == (frames[0]["pdu"]["type"], frames[0]["pdu"]["stream"])
    assert bytes.fromhex(d["reassembled"]["body_hex"]) == message
