# Off-air receiver fixtures

Real Cairn frames received over the air, used as regression tests for the
receivers' clock-offset estimate and channel SNR on short frames, and their
level tracking
(`modem/phy/tests/offair.rs`, `sim/tests/test_offair.py`).

## Provenance

- **When:** 2026-11-07, the first two-radio over-the-air session.
- **Stations:** K0JLB-1 (laptop, AIOC serial 6d14682c, output -9 dB) and
  K0JLB-2 (AIOC serial e5a2ed9d, output -10 dB), each an AIOC (All-In-One-Cable
  USB sound card and PTT) into a handheld FM radio, squelch open.
- **Captures:** `cap/cap_AtoB.wav` (K0JLB-2 receiving K0JLB-1) and
  `cap/cap_BtoA.wav` (K0JLB-1 receiving K0JLB-2) of the session folder
  `2026-11-07-two-radio`, recorded by `cairn-modem listen --record` on the
  receiving station: 48 kHz mono 16-bit, idle open-squelch audio and then four
  frames from one station, sent with `cairn-modem tx`.
- **Cut by** `modem/tools/cut_offair_fixtures.py`: each WAV holds one frame
  exactly as captured, from 0.1 s before its first preamble sample to 0.05 s
  after its end. Nothing is resampled, filtered or rescaled.

## Files

| File | Frame | Notes |
| --- | --- | --- |
| `AtoB_1blk.wav` | DATA, 1 block, MCS 2, 24 tones, ACK control block (0.53 s) | |
| `AtoB_4s.wav` | DATA, 13 blocks, MCS 2 (3.6 s) | the A to B path's reference |
| `AtoB_probe.wav` | PROBE, header only (0.12 s) | |
| `AtoB_ui.wav` | UI, MCS 0, ID control block (0.79 s) | |
| `BtoA_1blk.wav` | DATA, 1 block, as above | 1276 samples (26.6 ms) missing inside the data block: an input xrun on the receiving station (the live log: `xruns 2 gaps 2 (64.8 ms)`); its data block cannot decode |
| `BtoA_4s.wav` | DATA, 13 blocks, MCS 2 (3.6 s) | the B to A path's reference; without level tracking 2 of its 13 blocks fail |
| `BtoA_probe.wav` | PROBE, header only | |
| `BtoA_ui.wav` | UI, MCS 0, ID control block | |
| `{AtoB,BtoA}_ref{1..4}.json` | the `cairn-bench` manifests of the transmitted frames (ref1 1 block, ref2 4 s, ref3 PROBE, ref4 UI) | the transmitted WAVs are in the session folder |
| `offair.json` | index: each WAV's frame, its header and control block bits, its payloads (decoded from the clean transmitted reference), each direction's 4 s frame clock offset and SNR (measured on the full capture); per frame the data blocks the simulator decodes with and without level tracking, its level fit (`python_level`, which the Rust receiver must reproduce) and the data symbols' gain against the S2 estimate with every sent symbol known (`data_gain_db`) | 1.2 MB with the B to A 4 s frame, which is the one frame whose decoding the level tracking changes |

## What they show

- Every frame's received level rises over its first 60 ms or so, from 13 to
  22 dB below the steady level at the first preamble sample, with a 19 ms time
  constant, the same on every tone and with no phase change. Measured with one
  gain per tone over the preamble, a header-only frame's SNR read 6 to 7 dB
  below the 4 s frame on the same path.
- The noise on these links is mostly the radios' own clipping (it follows the
  signal level): the preamble, built for a low crest factor, reads 5 to 8 dB
  cleaner than the data.
- The level the S2 pair measures (the channel estimate) is 3.3 to 4.0 dB
  (A to B) and 6.2 dB (B to A) below the data's: equalised by it alone, the
  data symbols sat that far above unit power (16-QAM decisions off, LLRs
  claiming twice the errors the A to B symbols had, 19% raw bit errors B to
  A). After S2 the level settles with a 12 to 26 ms time constant. The
  receivers' level tracking finds the data's level to within 0.25 dB
  (`data_gain_db`).
- Two continuous pilots over a header-only frame's 7 symbols measure the
  clock offset to about +-300 ppm: before the fix the receivers reported
  -115 and -206 ppm for frames whose 4 s neighbours read -3.2 and +2.2 ppm.

Values with the receivers of this commit (default clock prior; data level:
the tracked level of the data against the S2 estimate, and what every known
symbol gives):

| Frame | Blocks (untracked) | Clock, ppm | SNR, dB | Data level, dB |
| --- | --- | --- | --- | --- |
| A to B 1 block | 1/1 (1/1) | -17.5 +- 11.6 | 13.3 | +4.15 (+3.91) |
| A to B 4 s | 13/13 (13/13) | -3.2 +- 0.8 | 13.4 | +3.77 (+3.75) |
| A to B PROBE | - | -1.8 +- 49.7 | 12.0 | +3.95 |
| A to B UI | 1/1 (1/1) | -9.3 +- 8.3 | 13.7 | +3.25 (+3.20) |
| B to A 1 block | 0/1 (gap) | -49.7 +- 48.2 | 8.4 (fallback) | - |
| B to A 4 s | 13/13 (11/13) | +2.2 +- 0.8 | 13.6 | +6.24 (+6.21) |
| B to A PROBE | - | -24.6 +- 48.7 | 14.5 | +6.16 |
| B to A UI | 1/1 (1/1) | -3.8 +- 7.7 | 13.9 | +6.22 (+6.20) |
