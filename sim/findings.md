## Summary

Stage 1 (SPEC 21.2) is a complete Python/NumPy model of the transmit and
receive chain. It covers framing, all three LDPC codes, the bit mapping,
clip-and-filter, tone masks, the preamble detector, a pilot-tracking
receiver and the 14.2 FM channel model. Every rung decodes end to end at
every width from 24 to 56 tones. The numbers below come from whole frames
run through the channel and found by the real detector. A missed preamble
or a failed header counts every block in that frame as an error.

**Every rung now reaches 1% block error on every channel profile.** With
the clip level set per rung (ADR 0003: 7 dB at MCS 0–3, 8, 9 and 10 dB at
MCS 4–6), the floors at MCS 5 and 6 are gone. On the 7 dB-equivalent scale
(ADR 0003), the AWGN thresholds at 24 tones are 3.4, 5.5, 8.0, 12.1, 17.0,
21.0 and 25.5 dB for MCS 0–6, 1.5–3 dB under the old textbook column. Those
values, rounded up to the next 0.5 dB, are now the SPEC 6.2 Threshold
column. The mic chain costs 0–0.5 dB at MCS 0–4 and 1.2–2.2 dB at MCS 5–6.
The repeater profile needs its three dispersed low tones masked (SPEC
10.3) for MCS 3–6, and then lands within 0.2–1.5 dB of AWGN.

### Findings that changed the spec (ADR 0003)

1. **Clip level by rung.** A fixed 7 dB clip leaves a signal-to-distortion
   ratio of 21.7 dB, too little for 64-QAM 5/6 and 256-QAM 3/4. Counting
   the dB of average power each extra dB of clip level costs on a
   peak-limited radio, the best levels were 7 dB up to MCS 3, then 8, 9
   and 10 dB. The clip sweep table below has the evidence.
2. **SNR referenced to a 7 dB clip.** Average power now differs by up to
   3 dB between rungs, so every reported or compared SNR adds the rung's
   clip level minus 7 dB. All thresholds in this report are on that scale.
3. **The threshold column comes from simulation.** The textbook column was
   1.5–3 dB pessimistic at MCS 0–4 even with every receiver impairment
   modelled, which would have held links about one rung low.

### Findings that still need spec wording

1. **"Zero energy on masked tones" (SPEC 5.4) cannot be met literally.**
   Clip-and-filter's final clip regrows energy on every tone outside the
   kept set, masked tones included, to the same level as the band-edge
   regrowth: about 30 dB below the active tones at a 7 dB clip, 35, 42 and
   47 dB at 8, 9 and 10 dB. Reaching 40 dB at a 7 dB clip would take two
   or three more clip-and-filter passes and 3–4 dB of in-band SDR. The
   wasted power is about 0.1%. Suggested wording: no data on masked tones;
   residual clipping regrowth no higher than outside the band.
2. **What a mask applies to.** The model applies it to data symbols only;
   preamble, header and probe keep every tone. "Total power constant" is
   taken as data-symbol power equal to `width` unit-power tones, every
   active tone (pilots included) scaled alike. A masked pilot 37 or 52 is
   absent from data symbols but stays in the probe. A 56-bit mask applies
   at any width; tones at or above the width are ignored.
3. **Bursts at the top rungs at 56 tones (SPEC 6.4).** At MCS 4 and up a
   block spans only about 5 symbols, so a 10 ms burst takes two of them
   and BLER stays near 35% at any SNR. No within-block interleaver can fix
   that; ARQ has to.
4. **Receiver notes (non-normative).** Two receiver techniques matter a lot
   without affecting interoperability. Dropping burst-hit symbols from
   pilot tracking and raising their noise variance takes MCS 2 at 24 tones
   from 65% to 1% BLER under a 10 ms +20 dB burst
   (`receive(burst_aware=True)`). A dead tone that is not yet masked costs
   about 2 dB at MCS 4, because the 7-tone channel smoother spreads it to
   its neighbours; an outlier-robust smoother would contain it.

### Other results

- **Header (SPEC 6.3).** At MCS 0 almost every block error is a lost
  header. The receiver's header noise estimate now uses both preamble
  pairs and the header symbols' own error, instead of S2 alone with the
  pilot excess interpolated from the distorted low edge. That cut repeater
  header loss from 5.6% to 1.5% at 5 dB, moved the repeater MCS 0
  threshold from 5.9 to 5.2 dB and AWGN from 3.6 to 3.4 dB. The header
  still limits MCS 0 on the repeater, by about 0.9 dB over the data. The
  rate-1/3 contingency header (decision 20.4) is for headers failing above
  the FM threshold, which this is not, so it is not triggered.
- **Interleaver (SPEC 6.4).** The linear interleaver (π(i) = a·i mod E,
  a ≈ 0.618E) matches a uniformly random permutation on structural
  metrics and on BLER with a dead tone or a burst, at MCS 2 and 4 and at
  24 and 56 tones. Keep it.
- **Channel profiles.**
  - *Mic chain.* Within 0.5 dB of AWGN at MCS 0–4; MCS 5 and 6 lose 1.2
    and 2.2 dB to ISI from the 300 Hz high-pass on tones 0–1.
  - *Flat ports.* Thresholds depend strongly on the SNR definition. The
    discriminator noise rises 6 dB/octave, so in-band average SNR
    understates most tones. Rate-1/2 rungs gain a lot from that (MCS 0 at
    1.6–2.0 dB against 3.4 in AWGN); rate-3/4 rungs gain little. Per-tone
    SNR, which the modem actually measures, is the better axis for link
    adaptation.
  - *Repeater* (long prefix). The cascade's ~5 ms delay spread leaves tones
    0–2 near −10 dB SINR. With all tones in use, MCS 0–3 decode 2–5 dB
    above AWGN, MCS 4 at 22.4 dB, MCS 5 never (a rate-5/6 code cannot
    absorb 3 of 22 data tones lost) and MCS 6 stops at a 9% floor. With
    tones 0–2 masked, which SPEC 10.3's drop rule would do, MCS 3–6 reach
    12.3, 17.5, 22.1 and 27.0 dB. The mask costs 3 of 22 data tones of
    throughput.
- **Timing.** Drift-following windows handle ±100 ppm over 4 s frames, and
  pilot tracking with pilot 37 or 52 masked handles 80 ppm over 3.3 s.
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
  blocks, 4.465 s cycle, 4.74 kbps). Appendix A.2 matches within rounding
  once its MCS 5 row is corrected to 83 blocks (errata, PR #4).

### Still provisional (must go in an ADR before v0.2, SPEC 6.4)

The scrambler (38.211 Gold, c_init = 1 + block position), interleaver
(π(i) = a·i mod E, a ≈ 0.618E, now evaluated), Gray labeling (38.211),
frequency-first fill, S1 phases, S2 seed, pilot and probe sequences and the
anti-periodic prefix are all Stage 1 choices. They work, but they are not
yet spec. Test vectors for the full bit mapping should be generated once
that ADR fixes them. The LDPC vectors are final.

### How these numbers were made

`python -m cairn_sim.experiments {awgn,channels,clip,repeater_masked}` with
fixed seeds, 16 data blocks per frame, at least 2,000 blocks and 100 block
errors per point (capped at 20,000 blocks). Thresholds interpolate log BLER
between the points bracketing 1%. A point with no errors is counted as half
an error, which errs high. `header` and `interleaver` run fixed SNR points
with 2 blocks per frame. The "audio SNR" axis is defined in
`sim/README.md`; thresholds are reported on the 7 dB-equivalent scale. The
runs in `results/` took about 30 CPU-hours.
