"""Generate the bit-mapping test vectors (ADR 0005, SPEC 5.2, 6.1, 6.4).

``python -m cairn_sim.bitmap_vectors [--out DIR]`` writes one JSON file per
case to ``vectors/bitmap/``. Each file follows one frame through every stage
of the transmitter: payload, CRC, LDPC encoding and rate matching,
scrambling, interleaving, constellation mapping, the frequency-domain value
of every tone in every symbol and, for some cases, the time-domain samples
of the whole frame before clip-and-filter. The format is documented in
``vectors/README.md``.

The stages are computed here from the building blocks in ``crc``, ``ldpc``
and ``mapping``, then checked against ``frame.build`` (the tone grid and
the samples it produces), so the vectors describe exactly what the
simulation transmits. Output is deterministic; the test suite checks that
regenerating is byte-identical to the committed files.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from . import crc, frame, ldpc, mapping, params
from .vectors import bits_to_hex

FORMAT_VERSION = 1
GRID_DECIMALS = 9            # tone values (exact products; rounded for print)
SAMPLE_DECIMALS = 6          # time-domain samples (via an FFT; rounded for print)


@dataclass(frozen=True)
class Case:
    name: str
    width: int
    mcs: int
    n_blocks: int
    cp: int = params.CP_SHORT
    usable: tuple[int, ...] | None = None   # SPEC 5.4 usable tones; None: no mask
    samples: bool = False                   # include the time-domain frame

    def config(self) -> frame.FrameConfig:
        mask = None if self.usable is None else frozenset(self.usable)
        return frame.FrameConfig(width=self.width, mcs=self.mcs, n_blocks=self.n_blocks,
                                 cp=self.cp, clip=False, mask=mask)


def _all_but(*tones: int) -> tuple[int, ...]:
    return tuple(k for k in range(params.MAX_TONES) if k not in tones)


CASES: tuple[Case, ...] = (
    *(Case(f"w24_mcs{m}", 24, m, 2, samples=(m == 6)) for m in range(len(params.RUNGS))),
    Case("w24_mcs0_long", 24, 0, 1, cp=params.CP_LONG, samples=True),
    Case("w24_mcs3_long_masked", 24, 3, 2, cp=params.CP_LONG, usable=_all_but(0, 1, 2)),
    Case("w30_mcs1_masked", 30, 1, 2, usable=_all_but(9, 25, 40)),
    Case("w56_mcs2", 56, 2, 2),
    Case("w56_mcs6", 56, 6, 2),
    Case("w56_mcs4_long_masked", 56, 4, 2, cp=params.CP_LONG,
         usable=_all_but(0, 1, 2, 37, 45, 46), samples=True),
)


def case_bytes(case: str, item: str, n: int) -> bytes:
    """Portable PRNG: concatenated SHA-256("cairn-bitmap-vector:<case>:<item>:<counter>")."""
    out = b""
    counter = 0
    while len(out) < n:
        out += hashlib.sha256(f"cairn-bitmap-vector:{case}:{item}:{counter}".encode()).digest()
        counter += 1
    return out[:n]


def _bits(data: bytes) -> np.ndarray:
    return np.unpackbits(np.frombuffer(data, dtype=np.uint8))


# --- stages -------------------------------------------------------------------


def block_stages(code: ldpc.LiftedCode, info: np.ndarray, e: int, position: int) -> dict:
    """Rate-matched, scrambled and interleaved bits of one block."""
    tx = code.rate_match(code.encode(info[None])[0], e)
    c = mapping.scramble_sequence(position, e)
    scrambled = tx ^ c
    interleaved = mapping.interleave(scrambled)
    return {"tx": tx, "c": c, "scrambled": scrambled, "interleaved": interleaved}


def iq_levels(bits: np.ndarray, bps: int) -> np.ndarray:
    """Unnormalized odd-integer I and Q levels of each point, flattened I0 Q0 I1 Q1 ..."""
    pts = mapping.modulate(bits, bps) / mapping._axis_scale(bps)
    iq = np.stack([pts.real, pts.imag], axis=1).reshape(-1)
    out = np.rint(iq).astype(int)
    assert np.allclose(iq, out, atol=1e-9)
    return out


def _round(values: np.ndarray, decimals: int) -> list[float]:
    return [float(v) + 0.0 for v in np.round(np.asarray(values, dtype=float), decimals)]


def _segments(cfg: frame.FrameConfig) -> list[dict]:
    first_h = params.PREAMBLE_SYMBOLS
    first_p = first_h + cfg.header_symbols
    first_d = first_p + cfg.probe_symbols
    segs = [{"kind": "preamble", "first_symbol": 0, "symbols": params.PREAMBLE_SYMBOLS,
             "tones": list(range(params.BASE_TONES))},
            {"kind": "header", "first_symbol": first_h, "symbols": cfg.header_symbols,
             "data_tones": list(cfg.base_data_tones),
             "pilot_tones": list(params.pilots_for(params.BASE_TONES)), "gain": 1.0}]
    if cfg.has_probe:
        segs.append({"kind": "probe", "first_symbol": first_p, "symbols": cfg.probe_symbols,
                     "tones": list(range(cfg.width)),
                     "pilot_tones": list(params.pilots_for(cfg.width))})
    segs.append({"kind": "data", "first_symbol": first_d, "symbols": cfg.data_symbols,
                 "data_tones": list(cfg.data_tones), "pilot_tones": list(cfg.data_pilots),
                 "gain": cfg.data_gain})
    return segs


def case_doc(case: Case) -> dict:
    cfg = case.config()
    rung = cfg.rung
    bps = rung.bits_per_symbol

    # Header (always MCS 0, E = 288, scrambler position 0).
    hcode = ldpc.code_for(params.HEADER_CODE)
    e_h = params.HEADER_CODE.coded_bits[0]
    hmsg = case_bytes(case.name, "header", (params.HEADER_INFO_BITS - 16) // 8)
    hinfo = crc.append_crc(_bits(hmsg))
    h = block_stages(hcode, hinfo, e_h, frame.HEADER_POSITION)
    h_res = cfg.header_symbols * len(cfg.base_data_tones) * frame.QPSK
    h_pad = mapping.scramble_sequence(frame.PAD_POSITION, h_res - e_h)
    h_stream = np.concatenate([h["interleaved"], h_pad])

    # Data blocks (scrambler position 1 + block index).
    dcode = ldpc.code_for(params.DATA_CODE)
    e = rung.coded_bits
    payloads, blocks, streams = [], [], []
    for i in range(cfg.n_blocks):
        msg = case_bytes(case.name, f"block{i}", params.DATA_PAYLOAD_BITS // 8)
        payloads.append(_bits(msg))
        info = crc.append_crc(payloads[-1])
        st = block_stages(dcode, info, e, 1 + i)
        streams.append(st["interleaved"])
        start_point = i * e / bps
        blocks.append({
            "index": i, "position": 1 + i, "c_init": 2 + i,
            "first_symbol_offset": int(start_point // len(cfg.data_tones)),
            "first_tone_slot": int(start_point % len(cfg.data_tones)),
            "payload_hex": msg.hex(), "info_hex": bits_to_hex(info),
            "rate_matched_hex": bits_to_hex(st["tx"]), "scrambler_hex": bits_to_hex(st["c"]),
            "scrambled_hex": bits_to_hex(st["scrambled"]),
            "interleaved_hex": bits_to_hex(st["interleaved"]),
        })
    d_res = cfg.data_symbols * len(cfg.data_tones) * bps
    d_pad = mapping.scramble_sequence(frame.PAD_POSITION, d_res - cfg.n_blocks * e)
    d_stream = np.concatenate([*streams, d_pad])

    # The simulator's own frame, for the grid and samples and as a check.
    tx = frame.build(cfg, np.random.default_rng(0), header_msg=_bits(hmsg),
                     payloads=np.stack(payloads))
    assert np.array_equal(tx.header_bits, hinfo)
    grid = tx.grid
    first_h, first_d = params.PREAMBLE_SYMBOLS, params.PREAMBLE_SYMBOLS + cfg.header_symbols \
        + cfg.probe_symbols
    hp = mapping.modulate(h_stream, frame.QPSK).reshape(cfg.header_symbols, -1)
    assert np.array_equal(grid[first_h:first_h + cfg.header_symbols][:, list(cfg.base_data_tones)], hp)
    dp = mapping.modulate(d_stream, bps).reshape(cfg.data_symbols, -1) * cfg.data_gain
    assert np.array_equal(grid[first_d:][:, list(cfg.data_tones)], dp)

    doc = {
        "format": "cairn-bitmap-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
        "name": case.name, "width": cfg.width, "mcs": cfg.mcs, "modulation": rung.modulation,
        "bits_per_point": bps, "code_rate": rung.code_rate, "coded_bits": e,
        "interleaver_a": int(mapping.interleaver(e)[1]),
        "prefix_samples": cfg.cp, "usable_tones": None if case.usable is None else list(case.usable),
        "masked_tones": sorted(cfg.masked), "control_blocks": 0, "data_blocks": cfg.n_blocks,
        "total_symbols": cfg.total_symbols, "segments": _segments(cfg),
        "header": {
            "position": frame.HEADER_POSITION, "c_init": 1 + frame.HEADER_POSITION,
            "coded_bits": e_h, "interleaver_a": int(mapping.interleaver(e_h)[1]),
            "message_hex": hmsg.hex(), "info_hex": bits_to_hex(hinfo),
            "rate_matched_hex": bits_to_hex(h["tx"]), "scrambler_hex": bits_to_hex(h["c"]),
            "scrambled_hex": bits_to_hex(h["scrambled"]),
            "interleaved_hex": bits_to_hex(h["interleaved"]),
            "pad_bits": int(h_pad.size), "pad_hex": bits_to_hex(h_pad),
            "points_iq": iq_levels(h_stream, frame.QPSK).tolist(),
        },
        "blocks": blocks,
        "data": {
            "pad_bits": int(d_pad.size), "pad_hex": bits_to_hex(d_pad),
            "point_scale": mapping._axis_scale(bps),
            "points_iq": iq_levels(d_stream, bps).tolist(),
        },
        "grid": {"re": [_round(row.real, GRID_DECIMALS) for row in grid],
                 "im": [_round(row.imag, GRID_DECIMALS) for row in grid]},
    }
    if case.samples:
        doc["samples_scale"] = math.sqrt(2.0 / cfg.width)
        doc["samples"] = _round(tx.samples, SAMPLE_DECIMALS)
    return doc


# --- output -------------------------------------------------------------------


def _dump(obj, indent: int = 0) -> str:
    """JSON with one key per line, lists of scalars kept on one line."""
    pad, inner = " " * indent, " " * (indent + 1)
    if isinstance(obj, dict):
        items = [f"{inner}{json.dumps(k)}: {_dump(v, indent + 1)}" for k, v in obj.items()]
        return "{\n" + ",\n".join(items) + "\n" + pad + "}"
    if isinstance(obj, list) and any(isinstance(v, (dict, list)) for v in obj):
        return "[\n" + ",\n".join(inner + _dump(v, indent + 1) for v in obj) + "\n" + pad + "]"
    return json.dumps(obj, separators=(",", ":"))


def case_text(case: Case) -> str:
    return _dump(case_doc(case)) + "\n"


def generate(out_dir: Path) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for case in CASES:
        path = out_dir / f"{case.name}.json"
        path.write_text(case_text(case), newline="\n")
        written.append(path)
    return written


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=Path, default=params.CODES_DIR.parent / "vectors" / "bitmap")
    for p in generate(ap.parse_args().out):
        print(p)


if __name__ == "__main__":
    main()
