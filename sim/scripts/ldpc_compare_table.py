"""Summarize results.tsv from run_ldpc_compare.py: Eb/N0 at FER 1% and 0.1%.

    python3 sim/scripts/ldpc_compare_table.py WORKDIR/results.tsv

The crossing is found by a least-squares line through log10(FER) versus Eb/N0
over points with FER within a factor 4 of the target and at least 20 errors
(weighted by sqrt(errors)); a target is reported only if points bracket it.
"""

from __future__ import annotations

import math
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np

DECODERS = ("sim-ms", "aff-lnms", "aff-fnms", "sim-bp", "aff-lspa", "aff-fspa")


def crossing(pts: list[tuple[float, int, int]], target: float) -> float | None:
    if not (any(e / n > target for _, e, n in pts) and any(e / n < target for _, e, n in pts)):
        return None
    use = [(x, e, n) for x, e, n in pts
           if e >= 20 and abs(math.log10(e / n / target)) < math.log10(4)]
    if len(use) < 2:
        use = sorted(pts, key=lambda p: abs(math.log10(max(p[1], .5) / p[2] / target)))[:3]
    xs = np.array([u[0] for u in use])
    ys = np.log10([max(u[1], 0.5) / u[2] for u in use])
    w = np.sqrt([max(u[1], 1) for u in use])
    a, c = np.polyfit(xs, ys, 1, w=w)
    return float((math.log10(target) - c) / a)


def main() -> None:
    data: dict[tuple[str, str], list] = defaultdict(list)
    for ln in Path(sys.argv[1]).read_text().splitlines():
        code, dec, x, e, n, _ = ln.split("\t")
        data[(code, dec)].append((float(x), int(e), int(n)))
    codes = []
    for c, _ in data:
        if c not in codes:
            codes.append(c)
    for target in (0.01, 0.001):
        print(f"\nEb/N0 (dB) at FER {target:g}")
        print("code      " + " ".join(f"{d:>9}" for d in DECODERS))
        for c in codes:
            row = []
            for d in DECODERS:
                pts = sorted(data.get((c, d), []))
                x = crossing(pts, target) if pts else None
                row.append(f"{x:9.3f}" if x is not None else f"{'-':>9}")
            print(f"{c:9} " + " ".join(row))


if __name__ == "__main__":
    main()
