"""Generate the published LDPC files (SPEC 20.2 deliverable, v0.2).

``python -m cairn_sim.vectors [--codes DIR] [--vectors DIR]`` writes

``codes/``
    ``<name>.alist``            lifted parity-check matrix, alist format
    ``<name>_lifted.txt``       the same matrix as (block row, block col, shift)
    ``<name>_ratematch.txt``    ordered codeword bit indices sent, per on-air length
``vectors/``
    ``ldpc_<name>.json``        known-answer vectors per code and length

Formats are documented in ``codes/README.md`` and ``vectors/README.md``.
Output is deterministic; the test suite checks that regenerating is
byte-identical to the committed files.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

import numpy as np

from . import crc, ldpc, params

CODES = (params.DATA_CODE, params.CONTROL_CODE, params.HEADER_CODE)
SEEDS = (1, 2, 3)
FORMAT_VERSION = 1


def code_file_stem(spec: params.CodeSpec) -> str:
    return f"{spec.name}_bg{spec.base_graph}_z{spec.lifting}"


# --- helpers ------------------------------------------------------------------


def bits_to_hex(bits: np.ndarray) -> str:
    """Big-endian: first bit is the MSB of the first byte; zero-padded at the end."""
    return np.packbits(np.asarray(bits, dtype=np.uint8)).tobytes().hex()


def hex_to_bits(text: str, n: int) -> np.ndarray:
    return np.unpackbits(np.frombuffer(bytes.fromhex(text), dtype=np.uint8))[:n]


def seeded_bytes(seed: int, n: int) -> bytes:
    """Portable PRNG: concatenated SHA-256("cairn-ldpc-vector:<seed>:<counter>")."""
    out = b""
    counter = 0
    while len(out) < n:
        out += hashlib.sha256(f"cairn-ldpc-vector:{seed}:{counter}".encode()).digest()
        counter += 1
    return out[:n]


def messages(spec: params.CodeSpec) -> list[tuple[str, bytes]]:
    n = (spec.info_bits - 16) // 8
    out = [("zeros", bytes(n)), ("ones", b"\xff" * n),
           ("counting", bytes(i % 256 for i in range(n)))]
    out += [(f"random{s}", seeded_bytes(s, n)) for s in SEEDS]
    return out


def _lines(values: np.ndarray, per_line: int) -> list[str]:
    values = [str(int(v)) for v in values]
    return [" ".join(values[i:i + per_line]) for i in range(0, len(values), per_line)]


# --- codes/ -------------------------------------------------------------------


def alist_text(code: ldpc.LiftedCode) -> str:
    """alist (MacKay) for H, n = codeword columns before puncturing."""
    h = code.parity_check_matrix()
    m, n = h.shape
    rows = [np.nonzero(h[i])[0] + 1 for i in range(m)]
    cols = [np.nonzero(h[:, j])[0] + 1 for j in range(n)]
    out = [f"{n} {m}", f"{max(len(c) for c in cols)} {max(len(r) for r in rows)}",
           " ".join(str(len(c)) for c in cols), " ".join(str(len(r)) for r in rows)]
    out += [" ".join(map(str, c)) for c in cols]
    out += [" ".join(map(str, r)) for r in rows]
    return "\n".join(out) + "\n"


def lifted_text(spec: params.CodeSpec, code: ldpc.LiftedCode) -> str:
    out = [f"# Cairn LDPC lifted base graph: {spec.name} code",
           f"# base_graph={spec.base_graph} Z={spec.lifting} lifting_set_index={spec.set_index}",
           f"# base_rows={code.graph.rows} base_cols={code.graph.cols}"
           f" info_cols={code.graph.info_cols}",
           "# Each line: block_row block_col shift. Block (i, j) is the ZxZ identity cyclically",
           "# shifted so that row z has its one in column j*Z + (z + shift) mod Z.",
           "# shift = V(i, j) mod Z with V from 3GPP TS 38.212 Tables 5.3.2-2/-3."]
    for i, layer in enumerate(code.layers):
        out += [f"{i} {j} {s}" for j, s in layer]
    return "\n".join(out) + "\n"


def ratematch_text(spec: params.CodeSpec, code: ldpc.LiftedCode) -> str:
    out = [f"# Cairn LDPC rate matching: {spec.name} code (38.212 5.4.2, rv0, no limited buffer)",
           f"# codeword length {code.n_codeword}; positions are 0-based codeword bit indices",
           f"# (information bits 0..{code.k - 1}, fillers {code.k}..{code.k_native - 1} never sent,",
           f"# parity {code.k_native}..{code.n_codeword - 1}); first {2 * code.z} bits are punctured.",
           f"# Buffer size {code.buffer.size}. Each 'length E' section lists the E positions sent in",
           "# transmission order, 32 per line. Every length is a prefix of the buffer; a",
           "# retransmission continues at buffer index E (wrapping at the buffer size)."]
    for e in spec.coded_bits:
        out.append(f"length {e}")
        out += _lines(code.tx_positions(e), 32)
    return "\n".join(out) + "\n"


# --- vectors/ -----------------------------------------------------------------


def vector_doc(spec: params.CodeSpec, code: ldpc.LiftedCode) -> dict:
    vecs = []
    for name, msg in messages(spec):
        bits = np.unpackbits(np.frombuffer(msg, dtype=np.uint8))
        info = crc.append_crc(bits)[None, :]
        cw = code.encode(info)
        vecs.append({
            "name": name,
            "message_hex": msg.hex(),
            "info_hex": bits_to_hex(info[0]),
            "codeword_hex": bits_to_hex(cw[0]),
            "rate_matched_hex": {str(e): bits_to_hex(code.rate_match(cw, e)[0])
                                 for e in spec.coded_bits},
        })
    return {
        "format": "cairn-ldpc-vectors", "version": FORMAT_VERSION, "license": "CC BY 4.0",
        "code": spec.name, "base_graph": spec.base_graph, "lifting": spec.lifting,
        "message_bits": spec.info_bits - 16, "info_bits": spec.info_bits,
        "codeword_bits": code.n_codeword, "coded_lengths": list(spec.coded_bits),
        "vectors": vecs,
    }


def vector_text(spec: params.CodeSpec, code: ldpc.LiftedCode) -> str:
    return json.dumps(vector_doc(spec, code), indent=1) + "\n"


# --- driver -------------------------------------------------------------------


def generate(codes_dir: Path, vectors_dir: Path) -> list[Path]:
    codes_dir.mkdir(parents=True, exist_ok=True)
    vectors_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for spec in CODES:
        code = ldpc.code_for(spec)
        stem = code_file_stem(spec)
        files = {
            codes_dir / f"{stem}.alist": alist_text(code),
            codes_dir / f"{stem}_lifted.txt": lifted_text(spec, code),
            codes_dir / f"{stem}_ratematch.txt": ratematch_text(spec, code),
            vectors_dir / f"ldpc_{spec.name}.json": vector_text(spec, code),
        }
        for path, text in files.items():
            path.write_text(text, newline="\n")
            written.append(path)
    return written


def main() -> None:
    root = params.CODES_DIR.parent
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--codes", type=Path, default=root / "codes")
    ap.add_argument("--vectors", type=Path, default=root / "vectors")
    args = ap.parse_args()
    for p in generate(args.codes, args.vectors):
        print(p)


if __name__ == "__main__":
    main()
