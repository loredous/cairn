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
  sum-product. That is on the 0.3 dB acceptance line. An independent check
  against AFF3CT v4.7.3 (its own decoders on the identical parity-check
  matrices, `results/ldpc_aff3ct.md`) agrees within 0.02 dB at 1% FER for
  every code, for both min-sum and sum-product, and shows the same 0.30 dB
  gap, so the gap is the algorithm's, not a decoder bug. A citable published
  BG1 curve is still missing for formal acceptance. Raising the cap to 50
  iterations (allowed by 6.3) buys about 0.1 dB.
- **Time budget.** The model reproduces the SPEC 7.4 worked example (21
  blocks, 4.465 s cycle, 4.74 kbps). Appendix A.2 matches within rounding
  once its MCS 5 row is corrected to 83 blocks (errata, PR #4).

### Bit mapping (ADR 0005)

The scrambler (38.211 Gold, c_init = 1 + block position), interleaver
(π(i) = a·i mod E, a ≈ 0.618E, evaluated above), Gray labeling (38.211),
frequency-first fill, S1 phases, S2 seed, pilot and probe sequences and the
anti-periodic prefix were Stage 1 choices; ADR 0005 makes them normative,
with full bit-mapping vectors in `vectors/bitmap/` generated by
`bitmap_vectors`. SPEC 6.4 now states the burst limit (item 3 above) and
SPEC 6.5 carries the receiver notes (item 4). The LDPC vectors are final.

### How these numbers were made

`python -m cairn_sim.experiments {awgn,channels,clip,repeater_masked}` with
fixed seeds, 16 data blocks per frame, at least 2,000 blocks and 100 block
errors per point (capped at 20,000 blocks). Thresholds interpolate log BLER
between the points bracketing 1%. A point with no errors is counted as half
an error, which errs high. `header` and `interleaver` run fixed SNR points
with 2 blocks per frame. The "audio SNR" axis is defined in
`sim/README.md`; thresholds are reported on the 7 dB-equivalent scale. The
runs in `results/` took about 30 CPU-hours.

## A weak 70 cm link (2026-11-08)

On the evening of 2026-11-08 one direction of a 70 cm link (441.000 MHz,
K0JLB-1 to K0JLB-2, "A to B") decoded far worse than the other and than the
same stations on 2 m and 1.25 m, and its reported SNR swung from 19 to 9 dB
across four setups of the sending station. It looked like a receiver sitting
at its FM threshold. **It was not: the receiver was quieted, and the
impairment was a comb of steady lines at 1, 2, 3 and 4 kHz that came in with
the sender's audio.** Both receivers now measure those lines over the frame
and subtract them (non-normative, `frame.remove_lines`,
`cairn_phy::rx::remove_lines`), which takes the frames of the evening from
0-25 decoded blocks of 33-49 at MCS 4-6 to nearly all of them. The rate
adapter needs no change.

The captures are the session folder `2026-11-08-3band` (not in the
repository): DATA frames at MCS 4, 5 and 6, 32 tones with tones 0-2 masked,
two per run, with seeded payloads, so every transmitted symbol can be
rebuilt. `python -m cairn_sim.weak_fm_eval captures CAPTURE_DIR` decodes
each frame with the Python receiver (it agrees with the live listener's
blocks and SNR), rebuilds what was sent, and measures the error per data
cell, in the time domain (the residual after a least-squares FIR fit of the
sent waveform to the received one), and at the harmonics of 1 kHz
(`results/weak_fm_captures.json`).

### What the captures show

1. **Lines at harmonics of 1 kHz, 12-20 dB below the data in all.** On every
   70 cm A to B frame, against the data's in-band power (7 dB-equivalent
   scale, ADR 0003):

   | Setup | 1 kHz | 2 kHz | 3 kHz | 4 kHz | All | MCS 4 blocks, SNR | MCS 5 | MCS 6 |
   | --- | --- | --- | --- | --- | --- | --- | --- | --- |
   | 1: first session | -23.6 | -19.8 | -19.8 | -39.1 | -16.0 | 25/33, 15.2-15.5 dB | 0/41, 12.4-13.3 | 0/49, 13.3-13.5 |
   | 2: new antenna on A | -28.2 | -24.0 | -23.1 | -43.1 | -19.8 | 33/33, 18.9 | 6/41, 19.4-19.5 | 0/49, 16.2-16.5 |
   | 3: another radio on A | -25.4 | -21.1 | -20.2 | -40.1 | -16.9 | 29/33, 16.0-16.2 | 0/41, 13.2-13.5 | 0/49, 14.0 |
   | 4: antenna moved | -20.7 | -16.4 | -15.4 | -35.2 | -12.2 | 0/33, 8.8-9.1 | 0/41, 9.0-10.0 | 0/49, 10.2-10.8 |

   The reported SNR follows the lines' level, setup by setup. On every other
   path recorded that evening (70 cm B to A, 1.25 m and 2 m both ways) the
   same lines sit 40 dB or more below the data.
2. **They come from the sender, and only while its audio output is open.**
   The lines have the same absolute level (to 1 dB) in the carrier lead-in
   before the preamble, under the data, and in the carrier tail after the
   frame: they do not depend on the modulation. After the second (last)
   frame of each `tx` command they vanish (-75 to -87 dBFS against -40 to
   -48 during the frame) while the sender's carrier is still up, and the
   same happens after every single-frame `t1` transmission; after the first
   frame of a pair, when the sender keeps its output stream open for the
   next frame, they persist through the tail. They held to 0.1-0.2 dB over
   0.25 s windows and sat within 6 ppm of exact multiples of 1 kHz. Two
   radios and two antennas on A changed their level by 8 dB but never
   removed them. That points at the sending station's USB sound card (a 1 ms
   frame period) coupling into its 70 cm transmission, for example RF on the
   USB or audio cable; it is a station problem to fix with ferrites and
   cable dressing, and the receiver can also remove it (below).
3. **The receiver was quieted.** In the carrier tails where the lines had
   gone, the in-band noise under A's unmodulated carrier was -62 to -68 dBFS
   (1.25 m: -66 to -69; 2 m: -73 to -76) against data at -25 dBFS: 37-43 dB of
   quieting, roughly 30 dB CNR on the FM model below, 20 dB above threshold.
   The "carrier floor" of -40 to -42 dBFS noted during the session was the
   lines (-38.7 dBFS together in the first run's tail, against -58 dBFS for
   everything else in the band).
4. **No clicks.** With the lines in, the in-band residual is as Gaussian as
   noise gets (kurtosis 2.9-3.7, Gaussian 3), because the steady lines
   dominate it. With them subtracted, what is left is the radios' own peak
   distortion, the same as on the strong paths: kurtosis 5.8-9.5 (strong
   paths 4.0-12.5), the loudest 1% of 1 ms windows holding 9-13% of its
   energy (strong 6-16%), and 1.7-4.8 dB louder when the sent waveform is in
   its top quartile (strong 2.5-5.6 dB). Nothing here grows with the
   modulation or comes in bursts beyond what every path shows.
5. **Failures come from a few tones, not a few symbols.** Per data cell the
   error is close to a complex Gaussian's tail (at most 0.1% of cells above
   10 times their tone's mean, against 0.1-0.9% on the strong paths, whose
   peak distortion is burstier; a Gaussian gives 0.005%), and 10-symbol
   averages of the error vary by 0.3-0.9 dB through a frame. Across tones it is anything but
   flat: each line sits midway between two tones (k kHz is tone 6.5 + 10 (k
   - 1)) and, being an exact number of cycles in the 10 ms FFT window, leaks
   into tone t with 1/(pi d)^2 of its power at d tone spacings, so tones 6/7,
   16/17 and 26/27 lose 8-12 dB and their neighbours 1-3 dB. Setup 1 at MCS
   4: 10.6/10.9, 7.8/8.0 and 8.0/8.5 dB on those tones, 18-20 dB on the
   rest. The session's mask tracker dropped exactly those tones (mask IDs 1
   and 2: tones 0-2, 6-7, 16-17, 26-27) and held MCS 3; the reverse
   direction ran MCS 6.
6. **Removing the lines recovers the frames.** Fitted with the sent signal
   known and subtracted, the lines take the reported SNR up by 5-12 dB, to
   1.5-5 dB short of the 1.25 m path's (setup 4: 7 dB short at MCS 4, the one
   setup whose floor was also lower). The receivers now do the same without
   knowing what was sent: over a 4 s frame the data's own projection on a
   line's frequency sits about 41 dB below the data, so a line is measured
   to within a fraction of a dB, and it is removed when it stands 10 dB above
   the projections 20-30 Hz either side of it. It never fires on the strong
   paths (30 frames, every one decoded exactly as before):

   | Setup | MCS 4 blocks (SNR) | MCS 5 | MCS 6 |
   | --- | --- | --- | --- |
   | 1 | 33, 33 of 33 (22.3, 23.2 dB) | 41, 39 of 41 (25.0, 25.2) | 43, 42 of 49 (26.5, 26.2) |
   | 2 | 33, 33 (24.3, 24.7) | 41, 41 (26.7, 27.0) | 49, 47 (27.9, 27.9) |
   | 3 | 33, 33 (22.5, 22.9) | 41, 41 (24.9, 25.2) | 40, 42 (26.3, 26.5) |
   | 4 | 28, 32 (19.3, 19.2) | 25, 25 (22.0, 22.2) | 9, 18 (24.5, 25.4) |

   The single-frame MCS 2 UI frames read 13.1 dB instead of 10.5-11.9 (the
   fourth, already 12.8, is unchanged), the same as the strong paths'
   (13.2-13.3). The canceller also takes the first harmonic above the band:
   at 24 tones the 3 kHz line, 350 Hz above the top tone, still cost MCS 6
   6 dB of signal-to-comb ratio in the sweep below (1% at 10.4 dB, against
   4.2 dB with it removed).
   `python -m cairn_sim.weak_fm_eval canceller CAPTURE_DIR`,
   `results/weak_fm_canceller.json`. Three of these frames are now off-air
   fixtures (`modem/fixtures/offair/w70_*`, 0.85 MB), with tests holding both
   receivers to these results.

### The models

**The comb (`channel.Comb`, `channel.CAPTURE_COMBS`).** `FmChannel(comb=...)`
adds steady lines at harmonics of 1 kHz to the audio output, each at a level
against the output's in-band signal power, with an optional slow level
wander (the captures did not need it). A new profile, `mic_wide`, is the
mic chain as these handhelds pass it, to 4.25 kHz instead of SPEC 14.2's 3
kHz, so that 32 tones arrive whole; driven 1.5 dB harder than the SPEC 7.1
target (`MIC_WIDE_DRIVE_DB`) its limiter leaves the strong paths' distortion
floor (MCS 4 at 32 tones: 25 dB reported, 26 on air; 20.8 dB against every
sent symbol, 21.2 on air; residual kurtosis 21, on air 11).

**Calibration** (`weak_fm_eval calibrate`, `results/weak_fm_calibration.json`).
Each setup's frames through `mic_wide` with that setup's measured lines and
a Gaussian floor set so the model's error without the lines matches the
capture's with its lines removed; nothing is fitted to the decode results.
Four model frames per row:

| Setup | MCS | Floor, dB | Capture: blocks (SNR, dB) | Model: blocks (SNR) | Per-tone rms, dB | Canceller: capture | Model |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 4 | 20.9 | 25, 25 (15.2-15.5) | 22, 25, 18, 12 (14.9) | 1.7 | 33, 33 | 33, 33, 33, 33 |
| 1 | 5 | 21.1 | 0, 0 (12.4-13.3) | 0, 0, 0, 0 (13.3-14.1) | 2.3 | 41, 39 | 41, 41, 41, 41 |
| 1 | 6 | 19.6 | 0, 0 (13.3-13.5) | 0, 0, 0, 0 (13.5-14.1) | 2.8 | 43, 42 | 0, 0, 0, 0 |
| 2 | 4 | 24.7 | 33, 33 (18.9) | 33, 33, 33, 33 (18.0-18.4) | 1.3 | 33, 33 | 33, 33, 33, 33 |
| 2 | 5 | 24.7 | 6, 6 (19.4-19.5) | 9, 4, 16, 20 (18.7-19.0) | 2.0 | 41, 41 | 41, 41, 41, 41 |
| 2 | 6 | 23.1 | 0, 0 (16.2-16.5) | 0, 0, 0, 0 (17.2-17.8) | 2.7 | 49, 47 | 47, 46, 46, 45 |
| 3 | 4 | 21.0 | 29, 29 (16.0-16.2) | 31, 33, 26, 25 (15.5-15.6) | 1.5 | 33, 33 | 33, 33, 33, 33 |
| 3 | 5 | 22.0 | 0, 0 (13.2-13.5) | 0, 0, 0, 0 (14.2-15.1) | 2.1 | 41, 41 | 41, 41, 41, 41 |
| 3 | 6 | 20.1 | 0, 0 (14.0) | 0, 0, 0, 0 (14.3-15.0) | 2.7 | 40, 42 | 1, 1, 1, 2 |
| 4 | 4 | 15.4 | 0, 0 (8.8-9.1) | 0, 0, 0, 0 (8.7-9.8) | 2.1 | 28, 32 | 32, 33, 27, 26 |
| 4 | 5 | 16.6 | 0, 0 (9.0-10.0) | 0, 0, 0, 0 (9.4-10.5) | 2.3 | 25, 25 | 1, 1, 4, 7 |
| 4 | 6 | 15.8 | 0, 0 (10.2-10.8) | 0, 0, 0, 0 (9.7-10.5) | 3.1 | 9, 18 | 0, 0, 0, 0 |
| 1.25 m | 4 | 31.4 | 33, 33 (25.9-26.2) | 33, 33, 33, 33 (24.1-26.2) | 1.0 | 33, 33 | 33, 33, 33, 33 |
| 1.25 m | 5 | 35.5 | 41, 41 (29.2) | 41, 41, 41, 41 (28.0-30.3) | 1.8 | 41, 41 | 41, 41, 41, 41 |
| 1.25 m | 6 | 29.5 | 49, 49 (30.6-30.9) | 49, 49, 48, 49 (29.5-30.4) | 2.7 | 49, 49 | 49, 49, 48, 49 |

The model reproduces the reported SNR to about 1 dB and the per-tone SNR
(notches included) to 1.3-3 dB rms, and every all-or-nothing outcome; where
a capture decoded part of a frame, the model scatters around it (setup 1
MCS 4: 12-25 blocks against 25; setup 2 MCS 5: 4-20 against 6). With the
canceller in both, it reproduces the recovery at MCS 4 and 5 (setups 1-3)
and at MCS 6 on setup 2, but is too pessimistic at the margin: MCS 6 on
setups 1 and 3 (0-2 blocks of 49 against 40-43) and MCS 5-6 on setup 4. Its
limiter's distortion at MCS 6's 10 dB clip is far burstier than the radios'
(residual kurtosis about 100 against 8-11), which a Gaussian floor set by
power cannot correct. Within 1-2 dB of a rung's threshold the model is
therefore a lower bound at MCS 5-6.

**The FM threshold (`channel.fm_discriminate`, `FmChannel(cnr_db=...)`).**
Complex-baseband FM with the carrier-to-noise ratio as the parameter: the
transmitter's audio modulates a unit carrier (5 kHz deviation at the
deviation clip), complex Gaussian noise band-limited to a 16 kHz IF is added
at 1/CNR, and a phase-difference discriminator recovers the audio before
the receive filters. Above threshold it gives Gaussian noise rising 6
dB/octave and an audio SNR of CNR + 8.7 dB (mic path, 24 tones); below
about 9 dB CNR the noise drags the phase around the origin and every slip is
a click (in-band kurtosis 40-65 at 6-8 dB CNR). The ladder against CNR on
`mic_wide` (`experiments fm_cnr`, `results/fm_cnr.json`; 1% BLER):

| MCS | 24 tones | 32 tones (0-2 masked) | For comparison: SPEC 6.2 threshold (audio SNR) |
| --- | --- | --- | --- |
| 0 | 5.2 | 5.7 | 3.5 |
| 1 | 5.6 | 5.7 | 6.0 |
| 2 | 6.4 | 6.4 | 8.5 |
| 3 | 8.5 | 8.0 | 12.5 |
| 4 | 9.6 | 10.0 | 17.5 |
| 5 | 12.5 | 13.5 | 21.0 |
| 6 | 17.4 | 18.1 | 25.5 |

Above about 10 dB CNR the rungs sit where their audio-SNR thresholds put
them (MCS 6 at 17.4 dB CNR is 26 dB of audio SNR). Below it the ladder
folds up against the threshold: MCS 0, 1 and 2 reach 1% within 1.3 dB of
CNR of one another (5.2-6.4 dB at 24 tones), and MCS 4 needs only 1-2 dB
more than MCS 3. A rung below MCS 0 would buy almost nothing, as decision
20.4 expected. The curves are steep: MCS 0 goes from 31% to 1.6% BLER
between 4 and 5 dB CNR.

**The comb sweep** (`experiments comb`, `results/comb.json`): the ladder
against the signal-to-comb ratio (all lines together, 7 dB-equivalent) of
the evening's mean comb shape, over a floor 30 dB down on `mic_wide`, with
the comb's tones in use, with the tones beside each line masked (as the
session's mask tracker did), and with the line canceller (1% BLER, dB):

| MCS | 24: tones in use | 24: masked | 24: canceller | 32: tones in use | 32: masked | 32: canceller |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | 0.7 | 0.7 | 0.6 | 1.7 | 1.7 | 1.7 |
| 1 | 0.8 | 0.6 | 0.7 | 5.7 | 1.7 | 1.7 |
| 2 | 0.7 | 0.7 | 0.7 | 4.6 | 2.0 | 2.0 |
| 3 | 6.5 | 1.7 | 0.7 | 11.2 | 4.7 | 1.9 |
| 4 | 11.4 | 7.2 | 2.0 | 15.8 | 10.2 | 3.1 |
| 5 | 18.4 | 12.5 | 3.1 | 22.2 | 15.4 | 4.2 |
| 6 | 24.9 | 19.0 | 4.2 | 27.5 | 20.8 | 5.4 |

The evening's combs sat 12-20 dB below the data. Without the canceller, at
32 tones and even over this clean floor, MCS 4 needs the comb 15.8 dB down,
MCS 5 22.2 dB and MCS 6 27.5 dB, so MCS 5-6 were out of reach on every
setup and MCS 4 marginal. Masking the tones beside the lines, as the
session did, buys 4-7 dB at MCS 4-6 for about a fifth of the data tones.
The canceller buys 6-22 dB at MCS 3-6 and leaves every rung needing only
1-5 dB. There the header fails first: it is MCS 0 on the unmasked base
tones, decoded before the frame's length is known and so never cleaned,
which sets the 0.6-0.7 dB (24 tones) and 1.7 dB (32 tones) floor of the low
rungs. No comb seen on air comes near it.

### The rate adapter

**It did not step into rungs that failed, and it needs no change.** On air
the session held MCS 2 and then MCS 3 for 155 s without a failed block,
with the mask tracker dropping the tones beside the lines. Every capture
agrees with the rules: no frame whose reported SNR would have let the
adapter request its rung (the next rung's threshold plus the 1 dB margin
plus the 2 dB step-up margin, or 3 dB for the reply rule) lost more than a
few blocks. With the lines in, MCS 4 decoded 76% of its blocks at 15.2-15.5
dB, 88% at 16.0-16.2 and all at 18.9, MCS 5 15% at 19.4, against request
points of 20.5 and 24 dB. With the canceller, MCS 5 decoded 61% at 22.0-22.2
dB (setup 4) and 95-100% at 24.9-27.0; MCS 6 18-37% at 24.5-25.4, 82-88% at
26.2-26.5 and 96-100% at 27.9, against a request point of 28.5 dB.

The cost of those margins is about one rung where the SNR sits within 3 dB
above a threshold. Judged on the captures with the canceller, against the
best fixed rung for each setup (blocks decoded times payload bits per
tone-symbol), the rules' choice gets 77% (setup 1: MCS 4 against MCS 6 at 87%), 85% (setup 2: MCS 5
against MCS 6 at 98%), 80% (setup 3) and 82% (setup 4: MCS 3 against MCS 4
at 91%), from two frames per rung.

The link simulator (`modem/link/tests/common`, `Sim`) now carries the comb:
`Link::comb_db` (line powers against one tone, changed on schedule by
`Sim::comb`), each line leaking into tone t as 1/(pi d)^2 (`comb_noise`;
setup 1's notched tones come out at 10.8 and 7.9 dB, on air 10.6-10.9 and
7.8-8.0), `Sim::line_cancel_db` for the receiver's canceller (25 dB; 0 for a
receiver without it), and an exponential effective-SNR mapping (`ESM_BETA`,
fitted to the comb sweep: within 1.5 dB of its 1% points at MCS 3-6, both
widths, comb tones in use or masked) for the frames' block errors. The
reported SNR is the power sum over the tones in use, as the receiver
measures it, so the mask tracker sees the notches. `tests/weak_fm.rs` runs a
32-tone session through the four setups, a minute each with the floor and
comb measured for each, three seeds, against an oracle that knows each
setup's channel and picks the best rung and mask on the same model:

| Setup | Without the canceller: rung, kbps | Oracle | With it: rung, kbps | Oracle |
| --- | --- | --- | --- | --- |
| 1 | MCS 3, 4.25 | MCS 4, 7.24 | MCS 4, 7.32 | MCS 5, 9.24 |
| 2 | MCS 4, 5.04-5.42 | MCS 5, 8.05 | MCS 5, 8.93 | MCS 6, 9.44 |
| 3 | MCS 4, 5.54 | MCS 4, 7.35 | MCS 5, 8.27 | MCS 5, 9.24 |
| 4 | MCS 3, 3.75-3.83 | MCS 3, 5.49 | MCS 3, 4.84-5.32 | MCS 4, 7.42 |
| All | 67% of the oracle, 1 frame of about 125 over 20% failed | | 84%, none | |

Without the canceller the simulated session settles at MCS 3 on setup 1,
where the real one did. About 10 points of each gap are the session's own
overhead (ARQ turns, probes), which the oracle's link-test rate leaves out:
setup 3 with the canceller runs the oracle's rung at 89% of its rate. The
rest is the margin, and the simulator's block error curve (a 0.4 dB
logistic about each threshold, as in AWGN) is steeper than these radios'
(their peak distortion leaves 1-4% of blocks failing 1-3 dB above a
threshold, see above), so it overstates what the next rung would deliver;
on the captures the loss is the 15-23% above. Fast fades were not seen on
this link (the reported SNR held to 0.3 dB between frames of a setup), so
they are not modelled here; the 6 dB steps of `scenarios.rs` cover them.

Two ways to recover that rung would change §10.1 and are left to the
project owner: per-link thresholds (which §10.1 already allows, computed
from the measured per-tone channel), or a step-up that tolerates a small
block error rate where the SPEC's own 5-10% target says throughput peaks.
The evidence here, two frames per rung on one evening, is not enough to
set either.

**On a link that really is at its FM threshold** the rules stay safe too.
`weak_fm_eval fm_reported` (`results/weak_fm_reported.json`) runs 40 frames
of every rung at each CNR from 4 to 20 dB through the FM model and records
the reported SNR and BLER. Applying §10.1 to that table, the adapter climbs
from MCS 0 and settles at each CNR on a rung with no more than 2% BLER
(apart from 4 dB CNR, where even MCS 0 fails), never on one it must step
down from. Below the threshold the reported SNR collapses with the CNR (the
clicks count as noise), which is what keeps it safe. It also leaves more on
the table there, because the ladder compresses: at 6 dB CNR, MCS 0 reports
8.4 dB while MCS 2 decodes at 1% BLER, but stepping to MCS 2 needs 11.5 dB,
so the adapter holds MCS 0 at half the best rate. Over 5-20 dB CNR it gets
68-91% of the best fixed rung at 24 and 32 tones (50% at 6 dB, 24 tones).
Per-link thresholds would recover most of this too.

### How these numbers were made

`python -m cairn_sim.weak_fm_eval {captures,canceller,calibrate} CAPTURE_DIR`
on the session folder, and `weak_fm_eval fm_reported` (40 frames of 16
blocks per rung and CNR); `python -m cairn_sim.experiments {fm_cnr,comb}`
with fixed seeds, at least 3,200 blocks and 60 block errors per point
(capped at 8,000), 16 blocks per frame, thresholds interpolated as above.
The `fm_cnr` rows and the `comb` rows without the canceller were run before
the receiver had one (`cancel_lines` false; on the FM model it finds no line
to remove). `cargo test --release -p cairn-link --test weak_fm --
--nocapture` for the adapter. About 15 CPU-hours on the remote host.
