"""Run the Cairn-vs-AFF3CT LDPC FER comparison grid and append results to a TSV.

    python3 sim/scripts/run_ldpc_compare.py WORKDIR HARNESS [--jobs 6]

WORKDIR holds the files written by ``export_alist.py`` and receives
``results.tsv`` (columns: code, decoder, ebn0_db, errors, frames, fer). HARNESS
is the compiled AFF3CT harness (``sim/scripts/aff3ct_ldpc.cpp``). Finished
points already in results.tsv are skipped, so the run can be resumed.
Decoders: ``sim-ms`` / ``sim-bp`` (Cairn's layered normalized min-sum 0.75 x 30
and layered sum-product x 50), ``aff-lnms`` (AFF3CT layered NMS 0.75 x 30),
``aff-lspa`` (layered SPA x 50), ``aff-fspa`` (flooding SPA x 100) and
``aff-fnms`` (flooding NMS 0.75 x 30).
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# code -> (file stem, Cairn min-sum Eb/N0 at 1% FER from sim/results/ldpc.md)
CODES = {
    "header": ("header_E288", 2.51),
    "data_r12": ("data_r12_E2048", 1.57),
    "data_r23": ("data_r23_E1536", 2.32),
    "data_r34": ("data_r34_E1368", 2.82),
    "data_r56": ("data_r56_E1232", 3.49),
}
MIN_ERR = 100
AFF = {
    "aff-lnms": ("LAYERED", "NMS", 30),
    "aff-lspa": ("LAYERED", "SPA", 50),
    "aff-fspa": ("FLOOD", "SPA", 100),
    "aff-fnms": ("FLOOD", "NMS", 30),
}


def grid(decoder: str) -> list[float]:
    if decoder == "sim-bp":                      # slow: stop near BP's own 1% point
        return [round(-0.6 + 0.1 * i, 1) for i in range(6)]
    if decoder in ("aff-lspa", "aff-fspa"):      # slow: sum-product reaches 0.1% by about +0.3 dB
        return [round(-0.6 + 0.1 * i, 1) for i in range(10)]
    return [round(-0.6 + 0.1 * i, 1) for i in range(10)]   # -0.6 .. +0.3 dB about min-sum 1%


def command(workdir: Path, harness: Path, code: str, dec: str, ebn0: float) -> list[str]:
    stem = CODES[code][0]
    if dec.startswith("sim"):
        name = {"sim-ms": "min-sum", "sim-bp": "bp"}[dec]
        return [sys.executable, str(ROOT / "sim/scripts/sim_point.py"), code, name,
                f"{ebn0:.3f}", str(MIN_ERR), "300000", "11"]
    typ, impl, it = AFF[dec]
    return [str(harness), str(workdir / f"{stem}.alist"), str(workdir / f"{stem}.meta"), typ, impl,
            str(it), f"{ebn0:.3f}", str(MIN_ERR),
            "100000" if impl == "SPA" else "400000", "7"]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("workdir", type=Path)
    ap.add_argument("harness", type=Path)
    ap.add_argument("--jobs", type=int, default=6)
    args = ap.parse_args()
    out = args.workdir / "results.tsv"
    done = set()
    if out.exists():
        for ln in out.read_text().splitlines():
            c, d, e, *_ = ln.split("\t")
            done.add((c, d, e))
    todo = []
    for code, (_, base) in CODES.items():
        for dec in ("sim-ms", "sim-bp", *AFF):
            for off in grid(dec):
                e = round(base + off, 3)
                if (code, dec, f"{e:.3f}") not in done:
                    todo.append((code, dec, e))
    # slowest first keeps the pool busy
    todo.sort(key=lambda t: (t[1] != "sim-bp", t[1] not in ("aff-fspa", "aff-lspa"), t[2]))

    def work(t):
        code, dec, e = t
        env = {"PYTHONPATH": str(ROOT / "sim"), "PATH": "/usr/bin:/bin", "OMP_NUM_THREADS": "1",
               "OPENBLAS_NUM_THREADS": "1", "MKL_NUM_THREADS": "1"}
        r = subprocess.run(command(args.workdir, args.harness, code, dec, e),
                           capture_output=True, text=True, env=env)
        parts = r.stdout.split()
        if r.returncode or len(parts) != 4:
            print("FAILED", t, r.stderr[-300:], flush=True)
            return
        with out.open("a") as f:
            f.write(f"{code}\t{dec}\t{e:.3f}\t{parts[1]}\t{parts[2]}\t{parts[3]}\n")

    with ThreadPoolExecutor(args.jobs) as ex:
        list(ex.map(work, todo))


if __name__ == "__main__":
    main()
