## Preamble and detector (SPEC 6.1, 20.3)

Measured with `cairn_sim.preamble_eval`; the statistical tests are marked
`slow` in `sim/tests/test_preamble.py`.

**Sequences (Stage 1 choices, to publish as test vectors).**

- S1: constant amplitude, phases in units of 2π/256 (`S1_PHASE_UNITS`), from
  a search over Newman phases plus 400 random starts, smoothed-PAPR descent,
  then an integer-grid polish. PAPR of the real symbol: **2.75 dB** with the
  short prefix and **2.80 dB** with the long one (2.84 / 2.88 dB evaluated 4×
  oversampled). Newman phases alone give 4.95 dB. This meets the §21.2 aim
  of ≤ 3 dB.
- S2: BPSK from the 38.211 Gold sequence with c_init = 5237. PAPR 5.07 dB;
  peak cross-correlation with S1 0.374. It's the lowest-PAPR segment whose
  cross-correlation with S1 is at most 0.40. Low PAPR and low cross-correlation
  pull against each other across all 65,536 candidates.

**Detector.** Band-limited to 300–2700 Hz and decimated to 8 kHz.
Schmidl–Cox plateau metric M at both repeat lags (540 and 600 samples),
confirmed by normalized cross-correlation ρ with the stored S1 waveform over
a ±100 ppm grid; detection needs ρ ≥ 0.55 and M ≥ 0.45; fine timing from S2
at 48 kHz. About 0.4 s of compute per minute of audio.

| Target (§21.2) | Measured |
| --- | --- |
| ≥ 99% detection at 6 dB audio SNR | 100% (18,000 trials: both prefixes, −100/0/+100 ppm); 99.8% at 3 dB, about 75% at 0 dB |
| ≤ 1 false alarm per 10 min of open-squelch noise | 0 in 10 min of white and 10 min of f²-tilted noise (largest ρ 0.43 / 0.46 against the 0.55 threshold) |
| Prefix identified ≥ 99.9% | 100% at every SNR tested |
| Steady tones never trigger | 0 false alarms from 1000 Hz courtesy beeps, 1750 Hz, all 16 DTMF pairs and CW, alone and mixed with noise from −3 to +30 dB (tones alone reach ρ ≤ 0.30) |

Timing error at 6 dB is about 0.55 samples RMS (worst 2.4), well inside the
prefix. The sample-clock grid buys almost nothing at ±100 ppm (the loss
without it is 0.0003 dB); it is kept because SPEC 6.1 describes it, but a
later revision could drop it.
