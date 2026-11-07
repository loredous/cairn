"""Published LDPC files: reproducible, self-consistent, and decodable."""

import json

import numpy as np
import pytest

from cairn_sim import crc, ldpc, params, vectors

SPECS = [params.HEADER_CODE, params.CONTROL_CODE, params.DATA_CODE]
ROOT = params.CODES_DIR.parent


def test_regeneration_is_byte_identical(tmp_path):
    written = vectors.generate(tmp_path / "codes", tmp_path / "vectors")
    assert len(written) == 12
    for path in written:
        committed = (ROOT / path.parent.name / path.name).read_bytes()
        assert path.read_bytes() == committed, path.name


@pytest.mark.parametrize("spec", SPECS, ids=lambda s: s.name)
def test_alist_matches_code(spec):
    code = ldpc.code_for(spec)
    lines = (ROOT / "codes" / f"{vectors.code_file_stem(spec)}.alist").read_text().split("\n")
    n, m = map(int, lines[0].split())
    assert (m, n) == (code.n_rows, code.n_codeword)
    h = np.zeros((m, n), dtype=np.uint8)
    for j in range(n):
        for i in map(int, lines[4 + j].split()):
            h[i - 1, j] = 1
    assert np.array_equal(h, code.parity_check_matrix())


@pytest.mark.parametrize("spec", SPECS, ids=lambda s: s.name)
def test_ratematch_file_matches_code(spec):
    code = ldpc.code_for(spec)
    text = (ROOT / "codes" / f"{vectors.code_file_stem(spec)}_ratematch.txt").read_text()
    sections, cur = {}, None
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        if line.startswith("length "):
            cur = sections[int(line.split()[1])] = []
        else:
            cur += [int(x) for x in line.split()]
    assert sorted(sections) == sorted(spec.coded_bits)
    for e, pos in sections.items():
        assert np.array_equal(pos, code.tx_positions(e))


@pytest.mark.parametrize("spec", SPECS, ids=lambda s: s.name)
def test_vectors_are_valid_and_decode_noiselessly(spec):
    code = ldpc.code_for(spec)
    doc = json.loads((ROOT / "vectors" / f"ldpc_{spec.name}.json").read_text())
    assert doc["license"] == "CC BY 4.0" and doc["codeword_bits"] == code.n_codeword
    assert {v["name"] for v in doc["vectors"]} >= {"zeros", "ones", "counting", "random1"}
    h = code.parity_check_matrix().astype(np.int32)
    for v in doc["vectors"]:
        msg = bytes.fromhex(v["message_hex"])
        info = vectors.hex_to_bits(v["info_hex"], spec.info_bits)
        assert np.array_equal(info[:-16], np.unpackbits(np.frombuffer(msg, dtype=np.uint8)))
        assert int("".join(map(str, info[-16:])), 2) == crc.crc16(msg)   # independent CRC
        cw = vectors.hex_to_bits(v["codeword_hex"], code.n_codeword)
        assert not ((h @ cw.astype(np.int32)) % 2).any()
        assert np.array_equal(cw[: spec.info_bits], info)
        for e in spec.coded_bits:
            tx = vectors.hex_to_bits(v["rate_matched_hex"][str(e)], e)
            assert np.array_equal(tx, cw[code.tx_positions(e)])
            llr = code.rate_recover(8.0 * (1 - 2.0 * tx.astype(np.float32)), e)
            res = ldpc.decode(code, llr, rows=code.rows_needed(code.tx_positions(e)))
            assert res.ok.all() and np.array_equal(res.bits[0], info), (v["name"], e)
