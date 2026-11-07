"""One BLER point of Cairn's own LDPC decoders, for the AFF3CT cross-check.

    PYTHONPATH=sim python3 sim/scripts/sim_point.py CODE DECODER EBN0_DB MIN_ERR MAX_BLOCKS [SEED]

CODE is header | data_r12 | data_r23 | data_r34 | data_r56. DECODER is
``min-sum`` (layered normalized min-sum, alpha 0.75, 30 iterations) or ``bp``
(layered sum-product, 50 iterations), both from ``cairn_sim.ldpc_eval``.
Prints ``ebn0 errors blocks bler``; Eb/N0 counts all K' = info_bits (message
plus CRC-16) bits, R = K'/E, as in ``ldpc_eval.measure``.
"""

from __future__ import annotations

import sys

from cairn_sim import ldpc_eval, params

CODES = {
    "header": (params.HEADER_CODE, 288),
    "data_r12": (params.DATA_CODE, 2048),
    "data_r23": (params.DATA_CODE, 1536),
    "data_r34": (params.DATA_CODE, 1368),
    "data_r56": (params.DATA_CODE, 1232),
}


def main() -> None:
    code, decoder, ebn0, min_err, max_blocks = sys.argv[1:6]
    seed = int(sys.argv[6]) if len(sys.argv) > 6 else 0
    spec, e = CODES[code]
    p = ldpc_eval.measure(spec, e, float(ebn0), min_errors=int(min_err),
                          max_blocks=int(max_blocks), decoder=decoder, seed=seed)
    print(f"{p.ebn0_db:.4f} {p.errors} {p.blocks} {p.bler:.6g}")


if __name__ == "__main__":
    main()
