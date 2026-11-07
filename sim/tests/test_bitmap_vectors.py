"""Published bit-mapping vectors (ADR 0005): reproducible, and consistent with
the normative rules written out independently of the simulator."""

import json
import math

import numpy as np
import pytest

from cairn_sim import bitmap_vectors, crc, ldpc, params, vectors

ROOT = params.CODES_DIR.parent
DIR = ROOT / "vectors" / "bitmap"
NAMES = [c.name for c in bitmap_vectors.CASES]


def load(name: str) -> dict:
    return json.loads((DIR / f"{name}.json").read_text())


def bits(text: str, n: int) -> np.ndarray:
    return vectors.hex_to_bits(text, n).astype(int)


# --- the normative rules, written from ADR 0005 -------------------------------


def gold(c_init: int, n: int) -> np.ndarray:
    """TS 38.211 5.2.1: c(n) = (x1(n + Nc) + x2(n + Nc)) mod 2, Nc = 1600."""
    x1 = [1] + [0] * 30
    x2 = [(c_init >> i) & 1 for i in range(31)]
    for k in range(1600 + n):
        x1.append(x1[k + 3] ^ x1[k])
        x2.append(x2[k + 3] ^ x2[k + 2] ^ x2[k + 1] ^ x2[k])
    return np.array([x1[k + 1600] ^ x2[k + 1600] for k in range(n)])


def multiplier(e: int) -> int:
    a = (618 * e + 500) // 1000
    while math.gcd(a, e) != 1:
        a += 1
    return a


def axis_level(b: list[int]) -> int:
    """38.211 5.1 per-axis level, e.g. (1-2b0)(4 - (1-2b2)(2 - (1-2b4))) for 64-QAM."""
    m, v = len(b), 1
    for j in range(m - 1, 0, -1):
        v = 2 ** (m - j) - (1 - 2 * b[j]) * v
    return (1 - 2 * b[0]) * v


def points(stream: np.ndarray, q: int) -> np.ndarray:
    groups = stream.reshape(-1, q)
    return np.array([[axis_level(list(g[0::2])), axis_level(list(g[1::2]))] for g in groups]).reshape(-1)


S1_UNITS = (181, 160, 172, 183, 202, 203, 204, 79, 7, 184, 91, 108,
            195, 191, 29, 125, 88, 203, 26, 202, 223, 107, 168, 82)
PILOTS = (3, 20, 37, 52)


# --- tests --------------------------------------------------------------------


def test_regeneration_is_byte_identical(tmp_path):
    written = bitmap_vectors.generate(tmp_path)
    assert sorted(p.name for p in written) == sorted(p.name for p in DIR.glob("*.json"))
    for path in written:
        assert path.read_bytes() == (DIR / path.name).read_bytes(), path.name


def test_case_set_covers_the_adr():
    docs = [load(n) for n in NAMES]
    assert {d["mcs"] for d in docs if d["width"] == 24} == set(range(len(params.RUNGS)))
    assert len({d["mcs"] for d in docs if d["width"] == 56}) >= 2
    assert any(d["masked_tones"] for d in docs)
    assert {d["prefix_samples"] for d in docs if "samples" in d} == {params.CP_SHORT, params.CP_LONG}


@pytest.mark.parametrize("name", NAMES)
def test_vector_follows_the_rules(name):
    d = load(name)
    w, q, e = d["width"], d["bits_per_point"], d["coded_bits"]
    hcode, dcode = ldpc.code_for(params.HEADER_CODE), ldpc.code_for(params.DATA_CODE)
    blocks = [("header", d["header"], hcode, 144)] + [("data", b, dcode, 1024) for b in d["blocks"]]
    for kind, b, code, k in blocks:
        n = b["coded_bits"] if kind == "header" else e
        info = bits(b["info_hex"], k)
        msg = bytes.fromhex(b.get("message_hex") or b["payload_hex"])
        assert int("".join(map(str, info[-16:])), 2) == crc.crc16(msg)
        tx = bits(b["rate_matched_hex"], n)
        assert np.array_equal(tx, code.rate_match(code.encode(info[None].astype(np.uint8)), n)[0])
        c = bits(b["scrambler_hex"], n)
        assert b["c_init"] == 1 + b["position"] and np.array_equal(c, gold(b["c_init"], n))
        s = bits(b["scrambled_hex"], n)
        assert np.array_equal(s, tx ^ c)
        a = multiplier(n)
        out = np.empty(n, dtype=int)
        out[(a * np.arange(n)) % n] = s
        assert np.array_equal(bits(b["interleaved_hex"], n), out)
    assert [b["position"] for b in d["blocks"]] == list(range(1, len(d["blocks"]) + 1))

    seg = {s["kind"]: s for s in d["segments"]}
    grid = np.array(d["grid"]["re"]) + 1j * np.array(d["grid"]["im"])
    assert grid.shape == (d["total_symbols"], w)

    # Preamble: S1 twice, S2 twice, base tones only.
    s1 = np.exp(2j * np.pi * np.array(S1_UNITS) / 256)
    s2 = 1 - 2 * gold(5237, 24)
    assert np.allclose(grid[:4, :24], [s1, s1, s2, s2], atol=1e-9)
    assert not grid[:4, 24:].any()

    # Pilots: 1 - 2 c(4 s + j), c_init 0x0B1D, s counted from the first preamble symbol.
    pv = 1 - 2 * gold(0x0B1D, 4 * d["total_symbols"]).reshape(-1, 4)

    def check_coded(sg, stream, gain, scale):
        first, n_sym = sg["first_symbol"], sg["symbols"]
        rows = grid[first:first + n_sym]
        dt, pt = sg["data_tones"], sg["pilot_tones"]
        pts = points(stream, sg_q[sg["kind"]])
        assert pts.tolist() == sg_pts[sg["kind"]]
        vals = (pts[0::2] + 1j * pts[1::2]).reshape(n_sym, len(dt)) * scale * gain
        assert np.allclose(rows[:, dt], vals, atol=1e-8)
        assert np.allclose(rows[:, pt], pv[first:first + n_sym, [PILOTS.index(p) for p in pt]] * gain)
        silent = [t for t in range(w) if t not in dt and t not in pt]
        assert not rows[:, silent].any()

    sg_q = {"header": 2, "data": q}
    sg_pts = {"header": d["header"]["points_iq"], "data": d["data"]["points_iq"]}
    h = d["header"]
    h_stream = np.concatenate([bits(h["interleaved_hex"], h["coded_bits"]), bits(h["pad_hex"], h["pad_bits"])])
    assert np.array_equal(bits(h["pad_hex"], h["pad_bits"]), gold(256, h["pad_bits"]))
    assert seg["header"]["data_tones"] == [t for t in range(24) if t not in (3, 20)]
    check_coded(seg["header"], h_stream, 1.0, 1 / math.sqrt(2))

    masked = set(d["masked_tones"])
    data_tones = [t for t in range(w) if t not in PILOTS and t not in masked]
    data_pilots = [p for p in PILOTS if p < w and p not in masked]
    assert seg["data"]["data_tones"] == data_tones and seg["data"]["pilot_tones"] == data_pilots
    gain = math.sqrt(w / (len(data_tones) + len(data_pilots)))
    assert math.isclose(seg["data"]["gain"], gain)
    dd = d["data"]
    stream = np.concatenate([bits(b["interleaved_hex"], e) for b in d["blocks"]]
                            + [bits(dd["pad_hex"], dd["pad_bits"])])
    assert np.array_equal(bits(dd["pad_hex"], dd["pad_bits"]), gold(256, dd["pad_bits"]))
    assert stream.size == seg["data"]["symbols"] * len(data_tones) * q
    m = q // 2
    labels = [[(v >> (m - 1 - i)) & 1 for i in range(m)] for v in range(2 ** m)]
    scale = 1 / math.sqrt(2 * np.mean([axis_level(lab) ** 2 for lab in labels]))
    assert math.isclose(dd["point_scale"], scale)
    check_coded(seg["data"], stream, gain, scale)

    if "probe" in seg:
        p = seg["probe"]
        first = p["first_symbol"]
        c = (1 - 2 * gold(0x0D0E, 2 * 56).reshape(2, 56)[:, :w]).astype(complex)
        pt = [t for t in PILOTS if t < w]
        c[:, pt] = pv[first:first + 2, :len(pt)]
        assert np.allclose(grid[first:first + 2], c)

    if "samples" in d:
        cp, n_useful = d["prefix_samples"], params.N_USEFUL
        n = np.arange(-cp, n_useful)
        f = 3.5 + np.arange(w)
        basis = np.exp(2j * np.pi * np.outer(f, n) / n_useful)          # (w, N + cp)
        x = d["samples_scale"] * (grid @ basis).real
        got = np.array(d["samples"]).reshape(d["total_symbols"], -1)
        assert got.shape == x.shape and np.max(np.abs(got - x)) < 2e-6
        assert np.allclose(got[:, :cp], -got[:, -cp:], atol=2e-6)     # negated-tail prefix
