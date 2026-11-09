"""zstd body vectors (SPEC 12.1-12.2, ADR 0014 `zstd-frame-format`):
reproducible byte for byte, written the ADR's way (frame header read here
independently of zstandard), and decodable."""

import hashlib
import json

import zstandard as zstd

from cairn_sim import dict_eval, params, pdu, zstd_vectors

ROOT = params.CODES_DIR.parent / "vectors"
DOC = json.loads((ROOT / "zstd" / "bodies.json").read_text())


def test_regeneration_is_byte_identical(tmp_path):
    for path in zstd_vectors.generate(tmp_path):
        assert path.read_bytes() == (ROOT / path.relative_to(tmp_path)).read_bytes(), path


def test_dictionary_is_the_registered_file():
    d = (params.CODES_DIR.parent / DOC["dictionary"]["file"]).read_bytes()
    assert hashlib.sha256(d).hexdigest() == DOC["dictionary"]["sha256"]
    assert dict_eval.dictionary_id(d).hex() == DOC["dictionary"]["dictionary_id_hex"]


def test_sent_frames_follow_adr_0014():
    # Magic, then a frame header descriptor of 0: no content size, not
    # single-segment, no checksum, no dictionary ID; then a window descriptor.
    for v in DOC["sent"]:
        body = bytes.fromhex(v["body_hex"])
        assert body[:4] == bytes.fromhex("28b52ffd"), v["name"]
        assert body[4] == 0x00, v["name"]
        p = zstd.get_frame_parameters(body)
        assert (p.content_size, p.dict_id, p.has_checksum) == (zstd.CONTENTSIZE_UNKNOWN, 0, False), v["name"]


def test_sent_bodies_decode_and_pdus_carry_them():
    d = (params.CODES_DIR.parent / DOC["dictionary"]["file"]).read_bytes()
    for v in DOC["sent"]:
        body, msg = bytes.fromhex(v["body_hex"]), bytes.fromhex(v["input_hex"])
        dct = zstd.ZstdCompressionDict(d) if v["encoding"] == 2 else None
        assert zstd.ZstdDecompressor(dict_data=dct).decompress(body, max_output_size=len(msg)) == msg
        p = pdu.Pdu.decode(bytes.fromhex(v["pdu_hex"]))
        assert (p.body, p.encoding, p.crc) == (body, v["encoding"], pdu.crc32(msg))


def test_accepted_frames_carry_the_optional_fields_and_decode():
    d = (params.CODES_DIR.parent / DOC["dictionary"]["file"]).read_bytes()
    msg = bytes.fromhex(DOC["accept"][0]["input_hex"])
    for v in DOC["accept"]:
        body = bytes.fromhex(v["body_hex"])
        fhd = body[4]
        assert (fhd >> 6 != 0) or (fhd & 0x04) or (fhd & 3), v["name"]
        dct = zstd.ZstdCompressionDict(d) if v["dictionary"] else None
        assert zstd.ZstdDecompressor(dict_data=dct).decompress(body, max_output_size=len(msg)) == msg


def test_rejected_bodies_are_what_they_say():
    by = {v["name"]: bytes.fromhex(v["body_hex"]) for v in DOC["reject"]}
    assert by["no_magic"][:4] != bytes.fromhex("28b52ffd")
    assert by["magicless_format"][:4] != bytes.fromhex("28b52ffd")
    good = by["trailing_byte"][:-1]
    assert zstd.frame_header_size(good) and len(by["skippable_frame_after"]) == len(good) + 8
    big = zstd.ZstdDecompressor().decompress(by["over_bound"], max_output_size=1 << 17)
    assert len(big) == DOC["decoded_body_max"] + 1
    assert zstd.get_frame_parameters(by["over_bound_content_size"]).content_size == DOC["decoded_body_max"] + 1
