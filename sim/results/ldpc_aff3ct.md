## LDPC decoder cross-check against AFF3CT

Independent reference for `sim/results/ldpc.md`: the same parity-check
matrices decoded by AFF3CT **v4.7.3** (git tag `v4.7.3`, commit
`0e8ecda8191cec97a6fe86a56de0d957b63b3b04`, built from source with g++ 16.1 on
ARM64, `-O3 -march=native`, float32 decoders) and by Cairn's own decoders
(`cairn_sim.ldpc.decode`, layered normalized min-sum, alpha 0.75, 30
iterations; and the layered sum-product reference in `cairn_sim.ldpc_eval`,
50 iterations).

**Verdict: confirmed.** At 1% FER Cairn's min-sum and sum-product decoders agree
with AFF3CT's like-for-like decoders to within 0.02 dB for all five codes
(limit asked for: 0.1 dB). Near 0.1% FER the min-sum curves agree to within
about 25% in FER (about 0.03 dB). No H, LLR sign/scale or schedule problem
shows up. The 0.31 dB min-sum-to-sum-product gap in `ldpc.md` is reproduced by
AFF3CT (0.30 dB), so it belongs to the algorithm and is not a bug in our
decoder.

### Eb/N0 (dB) at 1% FER

Eb/N0 counts all K' = 144 (header) or 1024 (data) information bits including
the CRC-16, with R = K'/E on the air and Es/N0 = R Eb/N0 per BPSK symbol, the
same convention as `ldpc_eval.measure`. Punctured and unsent bits carry no
energy. At least 100 frame errors per point, except as noted. Crossings are
log-linear fits through the points within a factor 4 of the target.

| Code (E, R) | Cairn min-sum | AFF3CT layered NMS | diff | Cairn SPA (layered, 50) | AFF3CT layered SPA (50) | diff | AFF3CT flooding SPA (100) | AFF3CT flooding NMS (30) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Header BG2 Z=15 (288, 1/2) | 2.518 | 2.534 | +0.016 | 2.343 | 2.331 | -0.012 | 2.399 | 2.680 |
| Data r1/2 BG1 Z=48 (2048) | 1.571 | 1.569 | -0.002 | 1.261 | 1.266 | +0.005 | 1.287 | 1.672 |
| Data r2/3 (1536) | 2.323 | 2.321 | -0.002 | 2.165 | 2.159 | -0.006 | 2.171 | 2.418 |
| Data r3/4 (1368, R 0.749) | 2.816 | 2.805 | -0.011 | 2.705 | 2.688 | -0.017 | 2.706 | 2.886 |
| Data r5/6 (1232, R 0.831) | 3.492 | 3.490 | -0.002 | 3.396 | 3.395 | -0.001 | 3.415 | 3.551 |

(diff = AFF3CT minus Cairn.) Cairn's min-sum values match `ldpc.md`
(1.57, 2.32, 2.82, 3.49, 2.51).

### Near 0.1% FER

Cairn's min-sum was capped at about +0.3 dB above its 1% point, so its 0.1%
crossing is not bracketed; the table compares FER at the same Eb/N0 instead
(counts are errors/frames, so each FER is good to about 10%).

| Code | Eb/N0 (dB) | Cairn min-sum FER | AFF3CT layered NMS FER |
| --- | --- | --- | --- |
| Data r1/2 | 1.77 | 1.32e-3 (100/76,000) | 1.12e-3 (100/88,909) |
| Data r2/3 | 2.52 | 1.03e-3 (100/97,500) | 1.08e-3 (100/92,955) |
| Data r3/4 | 3.02 | 1.13e-3 (100/88,500) | 1.22e-3 (100/82,052) |
| Data r5/6 | 3.69 | 2.15e-3 (101/47,000) | 1.74e-3 (100/57,609) |
| Header | not reached | not reached (FER 2.7e-3 at 2.81) | not reached (3.1e-3 at 2.81) |

AFF3CT's 0.1% crossings (for reference): layered NMS 1.778, 2.532, 3.037, 3.741
dB for data r1/2, 2/3, 3/4, 5/6; layered SPA 1.506, 2.394, 2.938, 3.656;
flooding SPA 1.522, 2.412, 2.964, 3.684. The header curve is shallow and did not
reach 0.1% inside the trimmed grid. Cairn's SPA grid stopped near its 1% point
(its decoder runs about 110 blocks/s), so no SPA comparison exists at 0.1%.

### What else the numbers show

- The flooding schedule costs 0.06 to 0.15 dB for NMS and 0.02 to 0.07 dB for
  SPA relative to layered. Cairn uses layered, so compare against the layered
  AFF3CT columns. A flooding-only reference decoder would have made our
  min-sum look about 0.1 dB better than it is.
- Min-sum to sum-product at 1%: header 0.20 dB, data 1/2 0.30, 2/3 0.16,
  3/4 0.12, 5/6 0.10 (AFF3CT layered NMS against layered SPA). The rate-1/2
  data code is the one on the 0.3 dB acceptance line, as in `ldpc.md`.
- AFF3CT's sum-product is a second independent implementation (float32,
  `tanh`/`atanh`) agreeing with ours (float64) to 0.02 dB, so the in-house
  sum-product reference used for the SPEC 21.2 comparison is sound.

### How the codes were made identical

AFF3CT's simulator cannot puncture or erase LDPC bits, and it has no way to
read our rate matching, so the decoders were driven from a small C++ harness
(`sim/scripts/aff3ct_ldpc.cpp`) that calls AFF3CT's own decoder classes
(`Decoder_LDPC_BP_flooding` and `Decoder_LDPC_BP_horizontal_layered` with
`Update_rule_SPA` / `Update_rule_NMS`) on an alist H read by AFF3CT's own
`LDPC_matrix_handler`. The harness sends the all-zero codeword (valid for a
linear code with symmetric decoders), BPSK, AWGN, LLR = 2y/sigma^2 on sent
bits and 0 on punctured and unsent bits. A frame is in error if any
information bit is decoded as 1.

The exporter (`sim/scripts/export_alist.py --check`) builds H from the same
`LiftedCode` the sim uses and then:

- removes the filler columns (known zeros: deleting them leaves H c = 0 intact),
  and the base-graph column blocks and checks the sim itself drops
  (`rows_needed`: blocks with no sent bit);
- keeps the 2Z punctured columns and the unsent bits of a partly sent block as
  erasures (per-column sent flags in the `.meta` file);
- asserts the full H equals the published `codes/*.alist`, the sim's
  rate-match positions equal the published `codes/*_ratematch.txt`, 200
  encoder codewords per code satisfy both the full and the reduced H.

Round trip through AFF3CT: 20 codewords per code from the sim's encoder, sent
as noiseless +-8 LLRs with erasures at 0, were recovered exactly by both
AFF3CT layered NMS and flooding SPA for all five codes (20/20 each). That
exercises AFF3CT's reading of H, including recovery of the 96 (30 for the
header) punctured bits. Reduced matrices (N columns x M checks):
header 324 x 180, data 1/2 2176 x 1152, 2/3 1648 x 624, 3/4 1504 x 480,
5/6 1360 x 336.

### Commands

```sh
# AFF3CT (scratchpad), pinned tag
git clone --branch v4.7.3 --depth 1 --recurse-submodules --shallow-submodules \
    https://github.com/aff3ct/aff3ct aff3ct
cd aff3ct && mkdir build && cd build
cmake .. -DCMAKE_BUILD_TYPE=Release -DAFF3CT_COMPILE_EXE=ON \
    -DAFF3CT_COMPILE_STATIC_LIB=ON -DAFF3CT_COMPILE_SHARED_LIB=OFF \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_CXX_FLAGS="-O3 -march=native"
make -j6
# harness: g++ -std=gnu++11 -O3 -march=native -DNDEBUG -DAFF3CT_EXT_STRINGS
#   -DAFF3CT_MULTI_PREC -DAFF3CT_POLAR_BIT_PACKING -DMIPP_ENABLE_BACKTRACE
#   -DSPU_COLORS -DSPU_STACKTRACE -DCPPTRACE_STATIC_DEFINE (same defines as the
#   library; a mismatch corrupts the heap) -I<aff3ct>/include -I<streampu>/include
#   -I<MIPP>/include -I<date>/include -I<cli>/src -I<json>/include ...
#   sim/scripts/aff3ct_ldpc.cpp libaff3ct-4.7.3.a libcpptrace.a -lpthread -ldl

# matrices (+ verification)
PYTHONPATH=sim python3 sim/scripts/export_alist.py WORK --check

# one AFF3CT point: ALIST META TYPE IMPLEM ITER EBN0 MIN_ERR MAX_FRAMES SEED
aff3ct_ldpc WORK/data_r12_E2048.alist WORK/data_r12_E2048.meta LAYERED NMS 30 1.570 100 400000 7
aff3ct_ldpc WORK/data_r12_E2048.alist WORK/data_r12_E2048.meta LAYERED SPA 50 1.270 100 100000 7
aff3ct_ldpc WORK/data_r12_E2048.alist WORK/data_r12_E2048.meta FLOOD  SPA 100 1.270 100 100000 7
aff3ct_ldpc WORK/data_r12_E2048.alist WORK/data_r12_E2048.meta FLOOD  NMS 30 1.670 100 400000 7

# one Cairn point: CODE DECODER EBN0 MIN_ERR MAX_BLOCKS SEED
PYTHONPATH=sim python3 sim/scripts/sim_point.py data_r12 min-sum 1.570 100 300000 11
PYTHONPATH=sim python3 sim/scripts/sim_point.py data_r12 bp      1.270 100 300000 11

# whole grid (6 workers), then the table
python3 sim/scripts/run_ldpc_compare.py WORK /path/to/aff3ct_ldpc --jobs 6
python3 sim/scripts/ldpc_compare_table.py WORK/results.tsv
```

AFF3CT decoders: `--dec-type BP_HORIZONTAL_LAYERED` / `BP_FLOODING`,
`--dec-implem NMS` (factor 0.75) / `SPA`, with the iteration counts above
(the same classes the `aff3ct` CLI builds; the harness only supplies the LLRs).
Seeds: AFF3CT 7, Cairn 11. All raw points are in
`sim/results/ldpc_aff3ct_points.tsv` (code, decoder, Eb/N0, errors, frames,
FER).

### Caveats

- Grid trimmed to about 1% to 0.1% FER (min-sum offsets -0.6 to +0.3 dB about
  Cairn's 1% point; SPA decoders -0.6 to +0.3, Cairn SPA -0.6 to -0.1 plus one
  or two extra points), with frame caps of 100,000 (AFF3CT SPA), 400,000 (AFF3CT
  NMS) and 300,000 (Cairn); Cairn min-sum points at +0.3 dB (below 0.1% FER)
  were stopped by hand and are absent.
- The all-zero-codeword harness does not apply the CRC; Cairn's early stop also
  requires a CRC pass. A CRC-passing wrong word is negligible at these lengths.
- Statistical error is about 10% in FER per point (100 errors), roughly 0.03 dB.
