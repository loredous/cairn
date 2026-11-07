"""Export Cairn's effective parity-check matrices as alist files for AFF3CT.

For a code and an on-air length E the exported H is the decoder's view of the
rate-matched block, derived from the same ``cairn_sim.ldpc.LiftedCode`` the
Stage 1 simulator uses:

* filler columns (shortened zeros, known to both ends) are deleted: a column
  of a known-zero bit does not change H @ c = 0;
* base-graph column blocks with no sent bit (the unsent tail of the circular
  buffer) are deleted together with the checks that only they made useful
  (``rows_needed``): those checks carry zero messages in sum-product and
  min-sum alike;
* the 2Z punctured columns, and the unsent bits of a partly sent block, stay
  as erasures (LLR 0 at the receiver); the .meta file's second line flags, per
  column, whether a channel LLR exists (1) or it is an erasure (0).

Column order is the natural codeword order of the surviving columns, so
columns 0..K'-1 are the K' information bits (the first 2Z of them punctured)
followed by the sent parity bits. ``--check`` verifies that every codeword
from the sim's own encoder satisfies the exported H, that the full H equals
the published ``codes/*.alist`` and that the sim's rate-match positions equal
the published ``codes/*_ratematch.txt``. It also writes 20 sample codewords
(OUTDIR/<name>_E<E>.cw, one 0/1 row per word over the exported columns).

Usage (from the repository root):
    PYTHONPATH=sim python3 sim/scripts/export_alist.py OUTDIR [--check]
Writes OUTDIR/<name>_E<E>.alist and OUTDIR/<name>_E<E>.meta
(line 1: N M K punctured E; line 2: per-column sent flags).
"""

from __future__ import annotations

import argparse
from pathlib import Path

import numpy as np

from cairn_sim import crc, ldpc, params

CASES = (
    ("header", params.HEADER_CODE, 288, "header_bg2_z15"),
    ("data_r12", params.DATA_CODE, 2048, "data_bg1_z48"),
    ("data_r23", params.DATA_CODE, 1536, "data_bg1_z48"),
    ("data_r34", params.DATA_CODE, 1368, "data_bg1_z48"),
    ("data_r56", params.DATA_CODE, 1232, "data_bg1_z48"),
)


def reduced_h(code: ldpc.LiftedCode, e: int):
    """Return (H_reduced, kept codeword positions, kept row indices)."""
    tx = code.tx_positions(e)
    z = code.z
    # Whole base-graph columns that are punctured or at least partly sent; the
    # unsent bits inside them stay as erasures (LLR 0), as in the sim.
    blocks = np.union1d(tx // z, np.arange(2))
    kept = np.array([c for c in np.arange(code.n_codeword)
                     if c // z in blocks and not (code.k <= c < code.k_native)])
    base_rows = code.rows_needed(tx)
    rows = np.concatenate([np.arange(i * z, (i + 1) * z) for i in base_rows])
    h = code.parity_check_matrix()[np.ix_(rows, kept)]
    return h, kept, rows


def write_alist(h: np.ndarray, path: Path) -> None:
    m, n = h.shape
    col_adj = [np.nonzero(h[:, j])[0] + 1 for j in range(n)]
    row_adj = [np.nonzero(h[i])[0] + 1 for i in range(m)]
    lines = [f"{n} {m}", f"{max(map(len, col_adj))} {max(map(len, row_adj))}",
             " ".join(str(len(c)) for c in col_adj), " ".join(str(len(r)) for r in row_adj)]
    lines += [" ".join(map(str, c)) for c in col_adj]
    lines += [" ".join(map(str, r)) for r in row_adj]
    path.write_text("\n".join(lines) + "\n")


def read_alist(path: Path) -> np.ndarray:
    rows = [list(map(int, ln.split())) for ln in path.read_text().splitlines()]
    n, m = rows[0]
    h = np.zeros((m, n), dtype=np.uint8)
    for j in range(n):
        for i in rows[4 + j]:
            if i:
                h[i - 1, j] = 1
    return h


def check(name: str, spec: params.CodeSpec, e: int, stem: str, h, kept, rows, outdir: Path) -> None:
    code = ldpc.code_for(spec)
    full = read_alist(params.CODES_DIR / f"{stem}.alist")
    assert np.array_equal(full, code.parity_check_matrix()), "published alist != sim H"
    assert np.array_equal(full[np.ix_(rows, kept)], h)
    rng = np.random.default_rng(1)
    cw = code.encode(crc.append_crc(rng.integers(0, 2, (200, spec.info_bits - 16), dtype=np.uint8)))
    assert not ((full.astype(int) @ cw.T.astype(int)) % 2).any(), "encoder output violates full H"
    assert not ((h.astype(int) @ cw[:, kept].T.astype(int)) % 2).any(), "reduced H violated"
    text = (params.CODES_DIR / f"{stem}_ratematch.txt").read_text().splitlines()
    i = text.index(f"length {e}") + 1
    pos: list[int] = []
    while i < len(text) and not text[i].startswith("length"):
        pos += [int(x) for x in text[i].split()]
        i += 1
    assert np.array_equal(np.array(pos), code.tx_positions(e)), "ratematch table mismatch"
    assert kept[: spec.info_bits].tolist() == list(range(spec.info_bits))
    # Sample codewords (kept columns) for the AFF3CT-side round trip check.
    (outdir / f"{name}_E{e}.cw").write_text(
        "\n".join("".join(map(str, row)) for row in cw[:20][:, kept]) + "\n")
    print(f"  check ok: {name} E={e}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("outdir", type=Path)
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    args.outdir.mkdir(parents=True, exist_ok=True)
    for name, spec, e, stem in CASES:
        code = ldpc.code_for(spec)
        h, kept, rows = reduced_h(code, e)
        write_alist(h, args.outdir / f"{name}_E{e}.alist")
        m, n = h.shape
        sent = np.isin(kept, code.tx_positions(e)).astype(int)
        assert sent.sum() == e and np.array_equal(kept[sent == 1], code.tx_positions(e))
        (args.outdir / f"{name}_E{e}.meta").write_text(
            f"{n} {m} {spec.info_bits} {2 * code.z} {e}\n" + " ".join(map(str, sent)) + "\n")
        print(f"{name}: N={n} M={m} K={spec.info_bits} punctured={2 * code.z} E={e} "
              f"rate={spec.info_bits / e:.4f}")
        if args.check:
            check(name, spec, e, stem, h, kept, rows, args.outdir)


if __name__ == "__main__":
    main()
