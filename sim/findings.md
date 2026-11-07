## Summary

Stage 1 (SPEC 21.2) is a complete Python/NumPy model of the transmit and
receive chain. It covers framing, all three LDPC codes, the bit mapping,
clip-and-filter, the preamble detector, a pilot-tracking receiver and the
14.2 FM channel model. Every rung decodes end to end at every width from
24 to 56 tones. The numbers below come from whole frames run through the
channel and found by the real detector. A missed preamble or a failed
header counts every block in that frame as an error.

**The v0.2 pass criterion is met for MCS 0–4 and not yet for MCS 5–6.** At
MCS 0–4 the measured AWGN thresholds sit 1.4–3.3 dB *below* the SPEC 6.2
"textbook" column. Through the mic chain they are 0.1–0.6 dB worse than
AWGN. At the SPEC 7.1 clip level of 7 dB, MCS 5 and MCS 6 stop at an error
floor (about 1% and 27% BLER) set by the clipper's own in-band distortion.
With the clip level raised per rung (below), they reach 1% at 21.0 and
25.5 dB peak-referenced, 2.0 and 1.5 dB under the table.
That calls for a spec change, not a modem fix.

### Findings that should change the spec

1. **Clip level should depend on the rung (SPEC 7.1).** A fixed 7 dB clip
   leaves a signal-to-distortion ratio of 21.7 dB. That is below what
   64-QAM 5/6 and 256-QAM 3/4 need. Counting the dB of average power each
   extra dB of clip level costs on a peak-limited radio, the best clip
   level is:

   | Rung | Best clip level | Peak-referenced threshold |
   | --- | --- | --- |
   | MCS 3 | 7 dB | 12.0 dB |
   | MCS 4 | 8 dB | 17.1 dB |
   | MCS 5 | 9 dB | 21.0 dB |
   | MCS 6 | 10 dB | 25.5 dB |

   This changes only the transmitter. The receiver already measures
   distortion from the pilots and needs no signalling. The level-setting
   tone (7.1) is specified by peak amplitude, so it stays valid. The
   proposal: "clipped at 7 dB above RMS at MCS 0–3, 8 dB at MCS 4, 9 dB at
   MCS 5 and 10 dB at MCS 6". PAPR after clip-and-filter is at most the
   clip level.
2. **The header, not the data, sets the MCS 0 threshold (SPEC 6.3
   contingency).** At MCS 0 almost every block error is a lost header. In
   AWGN at 24 tones, data blocks in frames whose header decoded reach 1%
   BLER near 2.6 dB. Frame loss (header plus detector) reaches 1% only
   near 3.7 dB. On the repeater profile, *every* MCS 0 error from 4 to
   6 dB is a lost header. The header code is short (288 bits): it is 0.9 dB
   weaker than the data code in isolation (`results/ldpc.md`). It also
   decodes before the decision-directed noise estimate exists. The rate
   1/3 contingency header is about 2.2 dB stronger in Es/N0 (2.03 dB
   Eb/N0 at rate 1/3 against 2.51 dB at rate 1/2). That would move the
   bottleneck back to the data. Stage 2 still decides (6.3). These results
   say to plan for it.
3. **The SPEC 6.2 SNR column is pessimistic by about 2–3 dB for MCS 0–4.**
   The text calls it "textbook AWGN figures ... a real audio chain adds
   roughly 1–3 dB". The measured AWGN thresholds already include channel
   estimation, pilots, clipping and the real detector, and they still come
   in under the column: MCS 0: 3.6 / 5, MCS 1: 5.5 / 8, MCS 2: 8.0 / 11,
   MCS 3: 12.1 / 15, MCS 4: 17.2 / 19. Strictly, MCS 2–3 miss the "within
   3 dB" criterion by 0.1–0.3 dB, on the good side. Suggested replacement
   column (AWGN, 24 tones, best clip per rung, peak-referenced to the
   power a 7 dB clip delivers, rounded up): 4, 6, 8.5, 12.5, 17.5, 21,
   26. A second column
   should give the mic-chain figures.
4. **Errata found while building the model:**
   - The tones sit on half-bins (350 + 100k Hz with 10 ms symbols is 3.5 +
     k cycles per symbol). A literal cyclic prefix would put a phase jump
     in every tone. The prefix has to be the *negated* tail
     (anti-periodic), and 5.1 should say so. This belongs in the
     bit-mapping ADR.
   - Appendix A.2 lists 84 blocks for MCS 5 at 56 tones. Only 83 fit in
     the 330 block symbols of a 4 s frame (330 × 312 / 1232 = 83.6). Net
     rate 18.7 kbps, not 19.0.
   - The 6.2 "bps" columns are coded-information rates, so they include
     each block's CRC-16. True payload rates are 1.6% lower (1,008 / 1,024).

### Other results

- **Channel profiles.**
  - *Mic chain.* Within 0.6 dB of AWGN at MCS 0–4. ISI from the 300 Hz
    high-pass on tones 0–1 raises the MCS 5 floor to 3% at a 7 dB clip.
  - *Flat ports.* Thresholds depend strongly on the SNR definition. The
    discriminator noise rises 6 dB/octave, so in-band average SNR
    understates most tones. Rate-1/2 rungs gain a lot from that (MCS 0 at 1.6–2.0 dB
    against 3.4–3.6 in AWGN; MCS 2 at 6.3–7.0 against 7.9–8.0); rate-3/4 rungs gain little. Per-tone SNR, which
    the modem actually measures, is the better axis for link adaptation.
  - *Repeater* (long prefix). MCS 0–3 decode 2–5 dB above AWGN. MCS 4 has
    a 1.4% floor and MCS 5–6 fail. The cascade's ~5 ms delay spread
    leaves tones 0–2 near −10 dB SINR. The receiver discounts them, but
    the header has no per-tone noise estimate yet when it decodes. That is
    the first receiver improvement to make (estimate per-tone noise from
    the S1 and S2 repeats).
- **Timing.** Drift-following windows handle ±100 ppm over 4 s frames.
- **Preamble.** All 21.2 targets met: 100% detection at 6 dB, no false
  alarms in 20 minutes of noise or from steady tones, 100% prefix
  identification. S1 PAPR is 2.75 dB. Details are in the preamble section
  below.
- **LDPC.** Codes, rate matching and known-answer vectors are published
  under `codes/` and `vectors/`. Min-sum at 30 iterations is 0.31 dB behind
  sum-product. That is on the 0.3 dB acceptance line, but no citable
  published BG1 curve was found to compare against. Raising the cap to 50
  iterations (allowed by 6.3) buys about 0.1 dB.
- **Time budget.** The model reproduces the SPEC 7.4 worked example (21
  blocks, 4.465 s cycle, 4.74 kbps). Appendix A.2 matches within rounding,
  apart from the erratum above.

### Still provisional (must go in an ADR before v0.2, SPEC 6.4)

The scrambler (38.211 Gold, c_init = 1 + block position), interleaver
(π(i) = a·i mod E, a ≈ 0.618E), Gray labeling (38.211), frequency-first
fill, S1 phases, S2 seed, pilot and probe sequences and the anti-periodic
prefix are all Stage 1 choices. They work, but they are not yet spec. Test
vectors for the full bit mapping should be generated once that ADR fixes
them. The LDPC vectors are final.

### How these numbers were made

`python -m cairn_sim.experiments {awgn,channels,clip}` with fixed seeds, 16
data blocks per frame, at least 2,000 blocks and 100 block errors per point
(capped at 20,000 blocks). Thresholds interpolate log BLER between the
points bracketing 1%. A point with no errors is counted as half an error,
which errs high. The "audio SNR" axis is defined in `sim/README.md`. The
runs took about 17 CPU-hours.
