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

The same document builder serves the whole-frame vectors of
``frame_vectors`` (``vectors/frame/``), whose cases carry real header
fields and control blocks; for the bitmap cases (pseudo-random header bits,
no control blocks) its output is unchanged.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from . import crc, fields, frame, ldpc, mapping, params
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
    header: fields.Header | None = None     # real header fields (frame vectors); None: random bits
    controls: tuple = ()                    # control blocks (``fields``), with ``header`` only
    prefix: str = "cairn-bitmap-vector"     # pseudo-random input domain (``case_bytes``)

    def config(self) -> frame.FrameConfig:
        mask = None if self.usable is None else frozenset(self.usable)
        if self.header is not None:
            return frame.FrameConfig.from_header(self.header, frame.LinkState(self.width, mask),
                                                 cp=self.cp, clip=False)
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


def case_bytes(case: str, item: str, n: int, prefix: str = "cairn-bitmap-vector") -> bytes:
    """Portable PRNG: concatenated SHA-256("<prefix>:<case>:<item>:<counter>")."""
    out = b""
    counter = 0
    while len(out) < n:
        out += hashlib.sha256(f"{prefix}:{case}:{item}:{counter}".encode()).digest()
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
    first_d = cfg.first_data
    segs = [{"kind": "preamble", "first_symbol": 0, "symbols": params.PREAMBLE_SYMBOLS,
             "tones": list(range(params.BASE_TONES))},
            {"kind": "header", "first_symbol": first_h, "symbols": cfg.header_symbols,
             "data_tones": list(cfg.base_data_tones),
             "pilot_tones": list(params.pilots_for(params.BASE_TONES)), "gain": 1.0}]
    if cfg.has_probe:
        segs.append({"kind": "probe", "first_symbol": first_p, "symbols": cfg.probe_symbols,
                     "tones": list(range(cfg.width)),
                     "pilot_tones": list(params.pilots_for(cfg.width))})
    if cfg.n_control:
        segs.append({"kind": "control", "first_symbol": cfg.first_control, "symbols": cfg.control_symbols,
                     "data_tones": list(cfg.control_tones), "pilot_tones": list(cfg.control_pilots),
                     "gain": cfg.control_gain})
    if cfg.n_blocks:
        segs.append({"kind": "data", "first_symbol": first_d, "symbols": cfg.data_symbols,
                     "data_tones": list(cfg.data_tones), "pilot_tones": list(cfg.data_pilots),
                     "gain": cfg.data_gain})
    return segs


def _address_json(a: fields.Address) -> dict:
    if a.broadcast:
        out = {"broadcast": True}
    elif a.is_hashed:
        out = {"callsign": a.callsign, "hash30": a.hash30, "ssid": a.ssid}
    else:
        out = {"callsign": a.callsign, "ssid": a.ssid}
    out["value_hex"] = f"{a.encode():010x}"
    return out


def header_json(h: fields.Header) -> dict:
    """Header fields as published in the vectors."""
    return {"version": h.version, "frame_type": int(h.frame_type), "flags": h.flags(),
            "more_fragments": h.more_fragments, "session": h.session, "probe": h.probe, "repeat": h.repeat,
            "source": _address_json(h.source), "destination": _address_json(h.destination),
            "sequence": h.sequence, "mcs": h.mcs, "control_blocks": h.n_control, "data_blocks": h.n_data}


def _mask_json(mask: frozenset[int] | None) -> list[int] | None:
    return None if mask is None else sorted(mask)


def control_json(c) -> dict:
    """Control block fields as published in the vectors."""
    if isinstance(c, fields.AckBlock):
        return {"type": 0, "base_sequence": c.base_sequence, "bitmap_hex": f"{c.bitmap:032x}",
                "requested_mcs": c.requested_mcs, "snr_q": c.snr_q, "block_errors": c.block_errors,
                "tone_mask": _mask_json(c.tone_mask), "reprobe": c.reprobe, "long_prefix": c.long_prefix,
                "distortion": c.distortion, "adc_clipped": c.adc_clipped}
    if isinstance(c, fields.ConnectBlock):
        return {"type": 1, "session_id": c.session_id, "capabilities": c.capabilities,
                "width_cap": c.width_cap, "requested_mcs": c.requested_mcs,
                "max_frame_time": c.max_frame_time, "tone_mask": _mask_json(c.tone_mask),
                "dictionary_ids": list(c.dictionary_ids), "more_dictionaries": c.more_dictionaries,
                "resume": c.resume}
    if isinstance(c, fields.CalRequestBlock):
        return {"type": 3, "tone_freq": c.tone_freq, "step_count": c.step_count, "step_length": c.step_length,
                "gap_length": c.gap_length, "start_level": c.start_level, "step_size": c.step_size}
    if isinstance(c, fields.CalReportBlock):
        return {"type": 4, "amplitudes": list(c.amplitudes), "distortion": list(c.distortion),
                "clipped": list(c.clipped), "sweep_valid": c.sweep_valid}
    if isinstance(c, fields.IdBlock):
        return {"type": 5, "callsign": c.callsign, "ssid": c.ssid, "grid": c.grid}
    if isinstance(c, fields.PathBlock):
        return {"type": 6, "entries": [_address_json(a) for a in c.entries], "hops": c.hops,
                "used": list(c.used), "via": c.via}
    raise TypeError(c)


def case_doc(case: Case) -> dict:
    cfg = case.config()
    rung = cfg.rung
    bps = rung.bits_per_symbol

    # Header (always MCS 0, E = 288, scrambler position 0).
    hcode = ldpc.code_for(params.HEADER_CODE)
    e_h = params.HEADER_CODE.coded_bits[0]
    if case.header is not None:
        hmsg = np.packbits(case.header.message_bits()).tobytes()
    else:
        hmsg = case_bytes(case.name, "header", (params.HEADER_INFO_BITS - 16) // 8, case.prefix)
    hinfo = crc.append_crc(_bits(hmsg))
    h = block_stages(hcode, hinfo, e_h, frame.HEADER_POSITION)
    h_res = cfg.header_symbols * len(cfg.base_data_tones) * frame.QPSK
    h_pad = mapping.scramble_sequence(frame.PAD_POSITION, h_res - e_h)
    h_stream = np.concatenate([h["interleaved"], h_pad])

    # Control blocks (MCS 0, scrambler position 1 + block index).
    ccode = ldpc.code_for(params.CONTROL_CODE)
    e_c = params.CONTROL_CODE.coded_bits[0]
    controls, c_streams = [], []
    for i, c in enumerate(case.controls):
        cmsg = fields.control_message_bits(c)
        info = crc.append_crc(cmsg)
        st = block_stages(ccode, info, e_c, 1 + i)
        c_streams.append(st["interleaved"])
        start_point = i * e_c // frame.QPSK
        controls.append({
            "index": i, "position": 1 + i, "c_init": 2 + i,
            "first_symbol_offset": start_point // len(cfg.control_tones),
            "first_tone_slot": start_point % len(cfg.control_tones),
            "fields": control_json(c), "message_hex": bits_to_hex(cmsg), "info_hex": bits_to_hex(info),
            "rate_matched_hex": bits_to_hex(st["tx"]), "scrambler_hex": bits_to_hex(st["c"]),
            "scrambled_hex": bits_to_hex(st["scrambled"]),
            "interleaved_hex": bits_to_hex(st["interleaved"]),
        })
    c_res = cfg.control_symbols * len(cfg.control_tones) * frame.QPSK
    c_pad = mapping.scramble_sequence(frame.PAD_POSITION, c_res - len(case.controls) * e_c)
    c_stream = np.concatenate([*c_streams, c_pad])

    # Data blocks (scrambler position 1 + control blocks + block index).
    dcode = ldpc.code_for(params.DATA_CODE)
    e = rung.coded_bits
    n_c = cfg.n_control
    payloads, blocks, streams = [], [], []
    for i in range(cfg.n_blocks):
        msg = case_bytes(case.name, f"block{i}", params.DATA_PAYLOAD_BITS // 8, case.prefix)
        payloads.append(_bits(msg))
        info = crc.append_crc(payloads[-1])
        st = block_stages(dcode, info, e, 1 + n_c + i)
        streams.append(st["interleaved"])
        start_point = i * e / bps
        blocks.append({
            "index": i, "position": 1 + n_c + i, "c_init": 2 + n_c + i,
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
    pl = np.stack(payloads) if payloads else np.zeros((0, params.DATA_PAYLOAD_BITS), np.uint8)
    if case.header is not None:
        tx = frame.build(cfg, np.random.default_rng(0), header=case.header, controls=list(case.controls),
                         payloads=pl)
    else:
        tx = frame.build(cfg, np.random.default_rng(0), header_msg=_bits(hmsg), payloads=pl)
    assert np.array_equal(tx.header_bits, hinfo)
    grid = tx.grid
    first_h, first_d = params.PREAMBLE_SYMBOLS, cfg.first_data
    hp = mapping.modulate(h_stream, frame.QPSK).reshape(cfg.header_symbols, -1)
    assert np.array_equal(grid[first_h:first_h + cfg.header_symbols][:, list(cfg.base_data_tones)], hp)
    cp_ = mapping.modulate(c_stream, frame.QPSK).reshape(cfg.control_symbols, len(cfg.control_tones))
    cp_ = cp_ * cfg.control_gain
    assert np.array_equal(grid[cfg.first_control:first_d][:, list(cfg.control_tones)], cp_)
    dp = mapping.modulate(d_stream, bps).reshape(cfg.data_symbols, len(cfg.data_tones)) * cfg.data_gain
    assert np.array_equal(grid[first_d:][:, list(cfg.data_tones)], dp)

    doc = {
        "format": "cairn-bitmap-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
        "name": case.name, "width": cfg.width, "mcs": cfg.mcs, "modulation": rung.modulation,
        "bits_per_point": bps, "code_rate": rung.code_rate, "coded_bits": e,
        "interleaver_a": int(mapping.interleaver(e)[1]),
        "prefix_samples": cfg.cp, "usable_tones": None if case.usable is None else list(case.usable),
        "masked_tones": sorted(cfg.masked), "control_blocks": cfg.n_control, "data_blocks": cfg.n_blocks,
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
    if case.header is not None:
        doc["header"] = {"fields": header_json(case.header), **doc["header"]}
        doc["clip_db"] = cfg.clip_db       # the level the frame is clipped at on air (7 dB without data)
    if not cfg.n_blocks:
        del doc["data"]
    if case.controls:
        doc["controls"] = controls
        doc["control"] = {"pad_bits": int(c_pad.size), "pad_hex": bits_to_hex(c_pad),
                          "point_scale": mapping._axis_scale(frame.QPSK),
                          "points_iq": iq_levels(c_stream, frame.QPSK).tolist()}
        order = [k for k in doc if k not in ("controls", "control")]
        at = order.index("blocks")
        order[at:at] = ["controls", "control"]
        doc = {k: doc[k] for k in order}
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


def generate(out_dir: Path, cases: tuple[Case, ...] = CASES) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for case in cases:
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
