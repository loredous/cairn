"""5G NR LDPC codes as used by Cairn (SPEC 6.3, 20.2, A.3).

- Base graphs BG1 and BG2 come from ``codes/bg1.csv`` and ``codes/bg2.csv``,
  transcribed from 3GPP TS 38.212 Tables 5.3.2-2 and 5.3.2-3.
- A lifted code expands every base-graph entry V(i, j) into a Z x Z identity
  matrix cyclically shifted by V mod Z (38.212 5.3.2): row z of block (i, j)
  has its one in column j*Z + (z + shift) mod Z.
- Shortening: information bits beyond ``info_bits`` are filler zeros, known
  to both ends and never sent.
- Rate matching follows 38.212 5.4.2 with rv0 (k0 = 0) and no limited
  buffer: the first 2Z codeword bits are punctured, the remaining bits form a
  circular buffer with filler bits skipped, and E bits are read from it. Every
  on-air length is a prefix of the same buffer, so a retransmission can send
  the next segment (``start``).

LLR convention throughout: L = log P(bit = 0) / P(bit = 1).
"""

from __future__ import annotations

import csv
from dataclasses import dataclass, field
from functools import cache

import numpy as np

from . import params
from .crc import CrcChecker

FILLER_LLR = 1.0e4  # known zeros


@dataclass(frozen=True)
class BaseGraph:
    number: int
    rows: int
    cols: int
    info_cols: int
    entries: tuple[tuple[int, int, tuple[int, ...]], ...]  # (row, col, V per set index)


@cache
def load_base_graph(number: int) -> BaseGraph:
    path = params.CODES_DIR / f"bg{number}.csv"
    entries = []
    with path.open() as f:
        for r in csv.DictReader(f):
            entries.append((int(r["row"]), int(r["col"]),
                            tuple(int(r[f"ils{k}"]) for k in range(8))))
    rows = max(e[0] for e in entries) + 1
    cols = max(e[1] for e in entries) + 1
    info_cols = {1: 22, 2: 10}[number]
    expected = {1: (46, 68, 316), 2: (42, 52, 197)}[number]
    if (rows, cols, len(entries)) != expected:
        raise ValueError(f"{path}: expected rows/cols/entries {expected}, "
                         f"got {(rows, cols, len(entries))}")
    return BaseGraph(number, rows, cols, info_cols, tuple(entries))


def _gf2_solve(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    """Solve a @ x = b over GF(2) for square invertible a; b may have many columns."""
    n = a.shape[0]
    aug = np.concatenate([a, b], axis=1).astype(np.uint8)
    width = aug.shape[1]
    words = (width + 63) // 64
    padded = np.zeros((n, words * 64), dtype=np.uint8)
    padded[:, :width] = aug
    packed = np.packbits(padded, axis=1, bitorder="little").view(np.uint64)
    for col in range(n):
        word, bit = divmod(col, 64)
        mask = np.uint64(1) << np.uint64(bit)
        has = (packed[col:, word] & mask) != 0
        candidates = np.nonzero(has)[0]
        if candidates.size == 0:
            raise ValueError("parity part of H is singular")
        pivot = col + candidates[0]
        if pivot != col:
            packed[[col, pivot]] = packed[[pivot, col]]
        rows = np.nonzero((packed[:, word] & mask) != 0)[0]
        rows = rows[rows != col]
        packed[rows] ^= packed[col]
    unpacked = np.unpackbits(packed.view(np.uint8), axis=1, bitorder="little")[:, :width]
    return unpacked[:, n:]


@dataclass
class LiftedCode:
    spec: params.CodeSpec
    graph: BaseGraph = field(init=False)

    def __post_init__(self) -> None:
        self.graph = load_base_graph(self.spec.base_graph)
        z = self.z = self.spec.lifting
        g = self.graph
        self.k_native = g.info_cols * z                 # K
        self.k = self.spec.info_bits                   # K' (real information bits)
        self.n_fill = self.k_native - self.k
        self.n_codeword = g.cols * z                   # before puncturing
        self.n_rows = g.rows * z
        ils = self.spec.set_index
        # Per base-graph row: the (col, shift) pairs.
        layers: list[list[tuple[int, int]]] = [[] for _ in range(g.rows)]
        for i, j, vs in g.entries:
            layers[i].append((j, vs[ils] % z))
        self.layers = layers
        zz = np.arange(z)
        self.layer_index = [np.array([j * z + (zz + s) % z for j, s in layer]) for layer in layers]
        # Circular buffer: codeword positions after the 2Z punctured bits,
        # filler positions removed.
        pos = np.arange(2 * z, self.n_codeword)
        filler = (pos >= self.k) & (pos < self.k_native)
        self.buffer = pos[~filler]

    # --- matrices -----------------------------------------------------------

    def parity_check_matrix(self) -> np.ndarray:
        h = np.zeros((self.n_rows, self.n_codeword), dtype=np.uint8)
        z = self.z
        for i, idx in enumerate(self.layer_index):
            for e in range(idx.shape[0]):
                h[i * z + np.arange(z), idx[e]] = 1
        return h

    @property
    def generator_parity(self) -> np.ndarray:
        """P with parity = P @ info (mod 2), shape (n_rows, k)."""
        if not hasattr(self, "_p"):
            self._p = _parity_map(self.spec)
        return self._p

    # --- encode / rate matching ---------------------------------------------

    def encode(self, info: np.ndarray) -> np.ndarray:
        """(B, k) information bits -> (B, n_codeword) full codeword (fillers are zero)."""
        info = np.atleast_2d(np.asarray(info, dtype=np.uint8))
        parity = (info.astype(np.float32) @ self.generator_parity.T.astype(np.float32))
        parity = (parity.astype(np.int64) & 1).astype(np.uint8)
        fill = np.zeros((info.shape[0], self.n_fill), dtype=np.uint8)
        return np.concatenate([info, fill, parity], axis=1)

    def tx_positions(self, e: int, start: int = 0) -> np.ndarray:
        """Codeword positions of the E bits sent, starting ``start`` bits into the buffer."""
        return self.buffer[(start + np.arange(e)) % self.buffer.size]

    def rate_match(self, codeword: np.ndarray, e: int, start: int = 0) -> np.ndarray:
        return codeword[..., self.tx_positions(e, start)]

    def rate_recover(self, llr: np.ndarray, e: int, start: int = 0,
                     into: np.ndarray | None = None) -> np.ndarray:
        """Received LLRs (B, E) -> codeword LLRs (B, n_codeword); repeats add up.

        Punctured and unsent bits are erasures (0); fillers are known zeros.
        Pass ``into`` to combine with LLRs from an earlier transmission.
        """
        llr = np.atleast_2d(llr)
        if into is None:
            into = np.zeros((llr.shape[0], self.n_codeword), dtype=np.float32)
            into[:, self.k:self.k_native] = FILLER_LLR
        np.add.at(into, (slice(None), self.tx_positions(e, start)), llr.astype(np.float32))
        return into

    def rows_needed(self, sent_positions: np.ndarray) -> list[int]:
        """Base-graph rows worth decoding when only ``sent_positions`` were sent.

        Rows 0-3 (the core) are always used. Each later row i owns one
        degree-1 parity column (info_cols + i); if none of its bits were sent
        the row only ever passes zero messages, so it is dropped.
        """
        z = self.z
        sent_cols = np.unique(np.asarray(sent_positions) // z)
        rows = list(range(4))
        for i in range(4, self.graph.rows):
            if self.graph.info_cols + i in sent_cols:
                rows.append(i)
        return rows

    def syndrome_ok(self, hard: np.ndarray, rows: list[int] | None = None) -> np.ndarray:
        rows = range(self.graph.rows) if rows is None else rows
        ok = np.ones(hard.shape[0], dtype=bool)
        for i in rows:
            ok &= ~np.any(np.bitwise_xor.reduce(hard[:, self.layer_index[i]], axis=1), axis=1)
        return ok


@cache
def _parity_map(spec: params.CodeSpec) -> np.ndarray:
    code = LiftedCode(spec)
    h = code.parity_check_matrix()
    k_native = code.k_native
    p_full = _gf2_solve(h[:, k_native:], h[:, :k_native])  # parity = P @ [info, fillers]
    return np.ascontiguousarray(p_full[:, : code.k])


@cache
def code_for(spec: params.CodeSpec) -> LiftedCode:
    return LiftedCode(spec)


@cache
def crc_checker(info_bits: int) -> CrcChecker:
    return CrcChecker(info_bits - 16)


@dataclass
class DecodeResult:
    bits: np.ndarray        # (B, k) hard decisions on the information bits
    ok: np.ndarray          # (B,) CRC passed and syndrome satisfied
    iterations: np.ndarray  # (B,) iterations used


def decode(code: LiftedCode, llr: np.ndarray, *, rows: list[int] | None = None,
           max_iter: int = 30, alpha: float = 0.75) -> DecodeResult:
    """Layered normalized min-sum (SPEC 6.3), batched, with early stop on CRC.

    ``llr`` is (B, n_codeword) from ``rate_recover``. A block stops as soon as
    its CRC passes and its syndrome (over ``rows``) is satisfied.
    """
    llr = np.atleast_2d(llr)
    b = llr.shape[0]
    rows = list(range(code.graph.rows)) if rows is None else rows
    index = [code.layer_index[i] for i in rows]
    checker = crc_checker(code.k)
    k = code.k

    out_bits = np.zeros((b, k), dtype=np.uint8)
    out_ok = np.zeros(b, dtype=bool)
    out_iter = np.full(b, max_iter, dtype=np.int32)
    active = np.arange(b)
    # Internal layout is (codeword bit, block): a layer gather then moves
    # contiguous runs of B floats, and the per-check reductions run over the
    # leading axis.
    lv = np.ascontiguousarray(llr.T, dtype=np.float32)
    msgs = [np.zeros((idx.shape[0], code.z, b), dtype=np.float32) for idx in index]
    inf = np.float32(np.inf)

    def finish(done_local: np.ndarray, it: int) -> None:
        nonlocal active, lv, msgs
        ids = active[done_local]
        out_bits[ids] = (lv[:k, done_local].T < 0).astype(np.uint8)
        out_ok[ids] = True
        out_iter[ids] = it
        keep = ~done_local
        active, lv = active[keep], np.ascontiguousarray(lv[:, keep])
        msgs = [np.ascontiguousarray(m[:, :, keep]) for m in msgs]

    for it in range(1, max_iter + 1):
        for li, idx in enumerate(index):
            q = lv[idx]                         # (deg, Z, B)
            q -= msgs[li]
            neg = q < 0
            mag = np.abs(q)
            min1 = mag.min(axis=0, keepdims=True)
            is_min = mag == min1
            # Second minimum; if the minimum occurs twice it equals the first
            # (what argmin plus "the other minimum" gives).
            min2 = np.where(is_min, inf, mag).min(axis=0, keepdims=True)
            min2 = np.minimum(min2, np.where(is_min.sum(axis=0, dtype=np.int8) > 1, min1, inf))
            r = np.where(is_min, min2, min1)
            r *= np.float32(alpha)
            sign = np.bitwise_xor.reduce(neg, axis=0, keepdims=True) ^ neg
            # Flip the sign bit of r where the check's sign for this edge is negative.
            r.view(np.uint32)[...] ^= sign.view(np.uint8).astype(np.uint32) << 31
            msgs[li] = r
            q += r
            lv[idx] = q
        if lv.shape[1] == 0:
            break
        hard = (lv < 0).view(np.uint8).T        # (B, N)
        done = checker.check(hard[:, :k])
        if done.any():
            done[done] = code.syndrome_ok(hard[done], rows)
            if done.any():
                finish(done, it)
        if lv.shape[1] == 0:
            break
    if lv.shape[1]:
        out_bits[active] = (lv[:k].T < 0).astype(np.uint8)
    return DecodeResult(out_bits, out_ok, out_iter)
