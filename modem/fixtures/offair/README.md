# Off-air receiver fixtures

Real Cairn frames received over the air, used as regression tests for the
receivers' clock-offset estimate and channel SNR on short frames, their
level tracking, and the width of a broadcast wider than the radios pass
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
  frames from one station, sent with `cairn-modem tx`. `w48_rxB.wav` of the
  same folder (K0JLB-2 receiving a 48-tone UI broadcast from K0JLB-1, logged
  in `w48.log`) for the wide frame.
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
| `w48_ui.wav` | UI, MCS 0, 48 tones with the width probe, ID control block, 1 block (0.54 s) | the radios pass audio to about 4.25 kHz: tones 42-47 arrive 30 dB below the noise, so the probe's extent reads 38; the live listener decoded it at 38 and failed the block ("x38 from probe"). It decodes at 48, the only width at which its block does |
| `w48_ref.json` | the `cairn-bench` manifest of the transmitted wide frame | |
| `{AtoB,BtoA}_ref{1..4}.json` | the `cairn-bench` manifests of the transmitted frames (ref1 1 block, ref2 4 s, ref3 PROBE, ref4 UI) | the transmitted WAVs are in the session folder |
| `offair.json` | index: each WAV's frame, its header and control block bits, its payloads (decoded from the clean transmitted reference), each direction's 4 s frame clock offset and SNR (measured on the full capture); per frame the data blocks the simulator decodes with and without level tracking, its level fit (`python_level`, which the Rust receiver must reproduce) and the data symbols' gain against the S2 estimate with every sent symbol known (`data_gain_db`); under `wide`, the wide frame with the width it was sent at, the probe's extent and the width the simulator decodes it at | 1.2 MB with the B to A 4 s frame, which is the one frame whose decoding the level tracking changes |

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

## A weak 70 cm link (`w70_*`, `weak70.json`)

- **When:** 2026-11-08, the three-band session (`2026-11-08-3band`), 70 cm
  (441.000 MHz), K0JLB-2 receiving K0JLB-1, the weak direction. Same stations
  and AIOCs as above, wide FM, squelch open; recorded by
  `cairn-modem listen --record` on the receiving station.
- **Frames:** sent by `cairn-modem tx --type data` (two frames per run,
  default seed: frame i carries the random payloads of seed 1000 + i) at 32
  tones with tones 0-2 masked under mask ID 1, and one `t1f` UI frame at 24
  tones. The listener decoded them with that link state.
- **What impaired them:** the sender's audio carried steady lines at 1, 2, 3
  and 4 kHz, 13-26 dB below the data depending on the setup, each midway
  between two tones; the receiver was fully quieted (`sim/findings.md`, "A
  weak 70 cm link"). Both receivers now subtract the lines
  (`frame.remove_lines`, `cairn_phy::rx::remove_lines`).
- **Cut by** `modem/tools/cut_weak70_fixtures.py`, as above: 0.1 s before the
  first preamble sample to 0.05 s after the frame. 827,412 bytes of WAV and a
  22,722-byte index.

| File | Frame | Python and Rust receivers: blocks (SNR, dB) with the lines subtracted / without |
| --- | --- | --- |
| `w70_l2_m5.wav` | DATA, MCS 5, 41 blocks (3.96 s); a new antenna on the sender, lines 21-26 dB down | 41/41 (26.7) / 6/41 (19.4) |
| `w70_l4_m4.wav` | DATA, MCS 4, 33 blocks (3.98 s); the strongest lines, 14-20 dB down | 28/33 (19.3) / 0/33 (8.8) |
| `w70_t1f.wav` | UI, MCS 2, 24 tones, 1 block (0.68 s) | 1/1 (13.1) / 1/1 (10.5) |

`weak70.json` holds each frame's link state, header and control block bits,
payloads, and what the Python receiver decodes with and without the lines
removed; `sim/tests/test_offair_weak70.py` and `modem/phy/tests/offair.rs`
hold both receivers to at least those blocks with the canceller, exactly
those without, and the SNR to 0.5 dB.
