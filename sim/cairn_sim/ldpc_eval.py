"""LDPC performance measurement (SPEC 21.2 `ldpc` acceptance).

BLER versus Eb/N0 for BPSK on AWGN, the Eb/N0 at a target BLER, and the
Polyanskiy-Poor-Verdu normal approximation for the binary-input AWGN channel
as an absolute reference. A layered sum-product (BP) decoder is included only
to separate "min-sum loss" from "code and finite-length loss".

Eb/N0 counts every information bit the block carries (message plus CRC-16),
so the code rate is R = info_bits / E and Es/N0 = R Eb/N0 per BPSK symbol.
A block counts as an error unless the CRC and syndrome pass and the decoded
information bits equal the transmitted ones.

``python -m cairn_sim.ldpc_eval [--quick]`` prints the full comparison table.

Measured (layered normalized min-sum, alpha 0.75, 30 iterations; BP = layered
sum-product, 50 iterations; Eb/N0 in dB at BLER 1%, 100-300 block errors per
point; PPV = normal approximation):

    code     E    min-sum    BP    PPV   min-sum - PPV
    data   2048    1.565  1.260  0.695     0.87
    data   1536    2.317    -    1.576     0.74
    data   1368    2.818    -    2.141     0.68
    data   1232    3.493    -    2.883     0.61
    control 512    2.118    -    1.139     0.98
    header  288    2.509    -    1.408     1.10
    header  432    2.028    -    0.744     1.28
"""

from __future__ import annotations

import argparse
import math
from dataclasses import dataclass

import numpy as np
from scipy import optimize, special

from . import crc, ldpc, params


@dataclass
class Point:
    ebn0_db: float
    errors: int
    blocks: int

    @property
    def bler(self) -> float:
        return self.errors / self.blocks


# --- reference decoder ----------------------------------------------------------


def decode_spa(code: ldpc.LiftedCode, llr: np.ndarray, *, rows: list[int] | None = None,
               max_iter: int = 50) -> ldpc.DecodeResult:
    """Layered sum-product (belief propagation), float64, early stop on CRC and syndrome."""
    llr = np.atleast_2d(llr)
    b = llr.shape[0]
    rows = list(range(code.graph.rows)) if rows is None else rows
    index = [code.layer_index[i] for i in rows]
    checker = ldpc.crc_checker(code.k)
    k = code.k
    lv = np.ascontiguousarray(llr.T, dtype=np.float64)
    msgs = [np.zeros((idx.shape[0], code.z, b)) for idx in index]
    ok = np.zeros(b, dtype=bool)
    iters = np.full(b, max_iter, dtype=np.int32)
    for it in range(1, max_iter + 1):
        for li, idx in enumerate(index):
            q = lv[idx] - msgs[li]
            t = np.tanh(np.clip(q, -40, 40) / 2)
            t = np.where(np.abs(t) < 1e-12, 1e-12, t)
            prod = np.prod(t, axis=0, keepdims=True)
            r = 2 * np.arctanh(np.clip(prod / t, -1 + 1e-12, 1 - 1e-12))
            msgs[li] = r
            lv[idx] = q + r
        hard = (lv < 0).view(np.uint8).T
        done = checker.check(hard[:, :k])
        done[done] = code.syndrome_ok(hard[done], rows)
        newly = done & ~ok
        iters[newly] = it
        ok |= done
        if ok.all():
            break
    return ldpc.DecodeResult((lv[:k].T < 0).astype(np.uint8), ok, iters)


# --- Monte Carlo ------------------------------------------------------------------


def _decode_min_sum(code, llr, rows):
    return ldpc.decode(code, llr, rows=rows)


def _decode_bp(code, llr, rows):
    return decode_spa(code, llr, rows=rows)


def measure(spec: params.CodeSpec, e: int, ebn0_db: float, *, min_errors: int = 100,
            max_blocks: int = 100_000, batch: int = 500, seed: int = 0,
            decoder: str = "min-sum") -> Point:
    """Block errors at one Eb/N0, stopping at ``min_errors`` or ``max_blocks``."""
    code = ldpc.code_for(spec)
    rows = code.rows_needed(code.tx_positions(e))
    dec = {"min-sum": _decode_min_sum, "bp": _decode_bp}[decoder]
    rate = spec.info_bits / e
    sigma2 = 1.0 / (2.0 * rate * 10 ** (ebn0_db / 10))
    sigma = math.sqrt(sigma2)
    rng = np.random.default_rng([seed, spec.info_bits, e, int(round(ebn0_db * 1000)) + 10**6])
    errors = blocks = 0
    while errors < min_errors and blocks < max_blocks:
        msg = rng.integers(0, 2, (batch, spec.info_bits - 16), dtype=np.uint8)
        info = crc.append_crc(msg)
        tx = code.rate_match(code.encode(info), e)
        y = (1.0 - 2.0 * tx) + sigma * rng.standard_normal(tx.shape)
        llr = code.rate_recover((2.0 * y / sigma2).astype(np.float32), e)
        if decoder == "bp":
            llr = llr.astype(np.float64)
        res = dec(code, llr, rows)
        errors += int((~(res.ok & np.all(res.bits == info, axis=1))).sum())
        blocks += batch
    return Point(ebn0_db, errors, blocks)


def ebn0_at_bler(spec: params.CodeSpec, e: int, target: float = 0.01, *, guess: float | None = None,
                 min_errors: int = 100, max_blocks: int = 100_000, tol_decades: float = 0.08,
                 decoder: str = "min-sum", seed: int = 0, verbose: bool = False
                 ) -> tuple[float, list[Point]]:
    """Eb/N0 (dB) where BLER = target, by iterating on log10 BLER and a local line fit."""
    x = normal_approx_ebn0(spec.info_bits, e, target) + 0.5 if guess is None else guess
    pts: list[Point] = []
    slope = -3.0  # decades per dB, refined from the data
    for _ in range(8):
        p = measure(spec, e, round(x, 3), min_errors=min_errors, max_blocks=max_blocks,
                    decoder=decoder, seed=seed)
        pts.append(p)
        if verbose:
            print(f"    Eb/N0 {p.ebn0_db:6.3f} dB  BLER {p.bler:.4g}  ({p.errors}/{p.blocks})", flush=True)
        usable = [q for q in pts if q.errors >= 5]
        if len(usable) >= 2:
            xs = np.array([q.ebn0_db for q in usable])
            ys = np.log10([q.bler for q in usable])
            if np.ptp(xs) > 0.05:
                slope = float(np.clip(np.polyfit(xs, ys, 1)[0], -8, -1))
        err = (math.log10(max(p.errors, 0.5) / p.blocks) - math.log10(target))
        if abs(err) < tol_decades and p.errors >= min(min_errors, 30):
            break
        x = p.ebn0_db - err / slope
    usable = [q for q in pts if q.errors >= 5 and abs(math.log10(q.bler / target)) < 0.6]
    if len(usable) >= 2 and np.ptp([q.ebn0_db for q in usable]) > 0.02:
        # Weighted by error count: counts set the relative precision of each point.
        xs = np.array([q.ebn0_db for q in usable])
        ys = np.log10([q.bler for q in usable])
        w = np.sqrt([q.errors for q in usable])
        a, c = np.polyfit(xs, ys, 1, w=w)
        return float((math.log10(target) - c) / a), pts
    p = pts[-1]
    return float(p.ebn0_db - (math.log10(max(p.errors, 0.5) / p.blocks) - math.log10(target)) / slope), pts


# --- normal approximation (PPV 2010) -----------------------------------------------

_GH_X, _GH_W = np.polynomial.hermite_e.hermegauss(200)


def biawgn_capacity_dispersion(snr: float) -> tuple[float, float]:
    """Capacity (bits/use) and dispersion (bits^2) of BPSK on AWGN, SNR = Es/sigma^2 (linear)."""
    # Send +1; LLR L = 2/s2 + 2 n/s, n ~ N(0,1), s2 = 1/snr. Information density
    # i = 1 - log2(1 + exp(-L)); C = E[i], V = Var[i].
    sigma = 1.0 / math.sqrt(snr)
    llr = 2 * snr + 2 * math.sqrt(snr) * _GH_X
    dens = 1.0 - np.logaddexp(0.0, -llr) / math.log(2)
    w = _GH_W / _GH_W.sum()
    c = float(np.sum(w * dens))
    v = float(np.sum(w * (dens - c) ** 2))
    del sigma
    return c, v


def normal_approx_bler(k: int, n: int, ebn0_db: float) -> float:
    rate = k / n
    snr = 2 * rate * 10 ** (ebn0_db / 10)
    c, v = biawgn_capacity_dispersion(snr)
    arg = (n * c - k + 0.5 * math.log2(n)) / math.sqrt(n * v)
    return float(special.ndtr(-arg))


def _log10_normal_approx_bler(k: int, n: int, ebn0_db: float) -> float:
    snr = 2 * (k / n) * 10 ** (ebn0_db / 10)
    c, v = biawgn_capacity_dispersion(snr)
    arg = (n * c - k + 0.5 * math.log2(n)) / math.sqrt(n * v)
    return float(special.log_ndtr(-arg)) / math.log(10)


def normal_approx_ebn0(k: int, n: int, target: float = 0.01) -> float:
    f = lambda x: _log10_normal_approx_bler(k, n, x) - math.log10(target)
    return float(optimize.brentq(f, -2.0, 15.0, xtol=1e-4))


# --- report -------------------------------------------------------------------------

CASES = (
    (params.DATA_CODE, 2048, 300), (params.DATA_CODE, 1536, 100),
    (params.DATA_CODE, 1368, 100), (params.DATA_CODE, 1232, 100),
    (params.CONTROL_CODE, 512, 100), (params.HEADER_CODE, 288, 100),
    (params.HEADER_CODE, 432, 100),
)


def report(quick: bool = False) -> None:
    print(f"{'code':8} {'E':>5} {'R':>5}  {'min-sum':>8} {'BP':>8} {'PPV':>8}  "
          f"{'MS-PPV':>7} {'MS-BP':>6}  (Eb/N0 dB at BLER 1%)")
    for spec, e, errs in CASES:
        errs = 30 if quick else errs
        ms, _ = ebn0_at_bler(spec, e, min_errors=errs, max_blocks=200_000)
        bp, _ = ebn0_at_bler(spec, e, guess=ms - 0.3, min_errors=min(errs, 100), max_blocks=200_000,
                             decoder="bp") if spec is params.DATA_CODE and e == 2048 else (float("nan"), None)
        ppv = normal_approx_ebn0(spec.info_bits, e)
        print(f"{spec.name:8} {e:5d} {spec.info_bits / e:5.3f}  {ms:8.3f} {bp:8.3f} {ppv:8.3f}  "
              f"{ms - ppv:7.3f} {ms - bp:6.3f}", flush=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--quick", action="store_true", help="fewer blocks, about 10x faster")
    report(ap.parse_args().quick)


if __name__ == "__main__":
    main()
