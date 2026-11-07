---
title: "Set the clip level by rung and reference SNR to a 7 dB clip"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0002 adopted a single clip-and-filter level: every symbol is clipped at
7 dB above its RMS level (`papr-clip-and-filter`). It also adopted a ladder
of seven rungs whose SNR column was a set of textbook AWGN figures, expected
to be 1–3 dB optimistic once a real audio chain was added (`mcs-ladder`).
Both were nominal values for Stage 1 simulation to check (SPEC §14.2, §16.2).

The Stage 1 simulation (`sim/`, `sim/report.md`) modelled the whole chain:
framing, the three LDPC codes, clip-and-filter, the preamble detector, a
pilot-tracking receiver and the §14.2 FM channel model. Two results bear on
these decisions.

- **The 7 dB clip limits the top rungs.** Clipping at 7 dB leaves in-band
  distortion 21.7 dB below the signal. MCS 0–4 do not notice it. MCS 5
  (64-QAM 5/6) stops improving at about 1% block error, and MCS 6 (256-QAM
  3/4) at about 27%, however strong the signal.
- **The SNR column is pessimistic.** MCS 0–4 reach 1% block error 1.5–3 dB
  below the column in white noise, with every receiver impairment included.
  Link adaptation starts from that column (`default-rung-thresholds`), so a
  link would sit about one rung lower than it should.

A higher clip level means less clipping distortion, but on a radio whose
deviation limit is a peak limit, each extra dB of clip level costs a dB of
average power. Swept against that cost, the best levels were:

| Rung | Clip level | Threshold at that clip (average-power SNR) | 7 dB-equivalent threshold |
| --- | --- | --- | --- |
| MCS 3 | 7 dB | 12.0 dB | 12.0 dB |
| MCS 4 | 8 dB | 16.1 dB | 17.1 dB |
| MCS 5 | 9 dB | 19.0 dB | 21.0 dB |
| MCS 6 | 10 dB | 22.5 dB | 25.5 dB |

MCS 0–3 do best at 7 dB.

## Decision

The clip level rises with the rung: 7 dB at MCS 0–3, 8 dB at MCS 4, 9 dB at
MCS 5 and 10 dB at MCS 6 (`clip-level-by-rung`). Every symbol of a frame
uses the level of its data rung, so the per-tone scale stays constant across
the frame and the channel estimate from the preamble remains valid. Peaks
stay at −3 dBFS. The average level therefore falls by up to 3 dB at the top
rungs (`transmit-peak-level-by-rung`).

Because average power now differs between rungs, every SNR the protocol
reports or compares with a threshold is referenced to a 7 dB clip
(`snr-reference`). The receiver adds the clip level of the received frame's
rung, minus 7 dB, to the SNR it measures; it knows the rung from the header.
An SNR measured at MCS 2 can then be compared directly with the MCS 6
threshold.

The ladder's SNR column is replaced by thresholds measured in Stage 1, on
that scale (`mcs-ladder-thresholds`). Link adaptation defaults to them plus
the Stage 2 margin (`rung-threshold-defaults`). The distortion index
measures only distortion beyond what the rung's clip level produces by
design (`distortion-index-reference`), so a correctly driven radio reads 0
at every rung.

## Consequences

- **Top rungs work.** MCS 5 and MCS 6 reach 1% block error at 21.0 and
  25.5 dB (7 dB-equivalent), 2.0 and 1.5 dB under the draft column. The
  56-tone throughput target (§14.1: 16 kbps at 30 dB) depends on MCS 5,
  because MCS 4 tops out at 15.0 kbps; it now has margin.
- **Decoding is unchanged.** The receiver needs no new information to decode:
  it already measures distortion from the pilots. The rung, and so the clip
  level, is in the header.
- **Interoperability now depends on the SNR reference.** Two implementations
  must apply the same offset, or their rung choices will disagree by up to
  3 dB. The offset is fixed by this ADR.
- **Headers in MCS 4–6 frames are received at up to 3 dB less average power.**
  Those frames are only sent on links far above the header's threshold, so
  the loss does not matter in practice.
- **The v0.2 exit criterion changes meaning.** "Rung thresholds within 3 dB
  of the table" (§14.2, §16.2) becomes circular once the table comes from
  simulation. The criterion becomes: thresholds set from Stage 1, with
  Stage 2 to confirm them within 3 dB.
- **Stage 2 has a new check.** The cost model assumes the radio's peak limit
  binds. Stage 2 must confirm that against the measured limiter knees
  (`calibrated-drive-target`).

## Alternatives Considered

- **One higher clip level for every rung (for example 9 dB).** This is
  simpler, with no SNR offset to apply, but MCS 0–3 lose up to 1.6 dB.
  Those are the rungs used near the FM threshold.
- **Keep 7 dB and accept the floors.** MCS 5's 1% floor is tolerable, since
  link adaptation targets 5–10% block error (§10.1). MCS 6 would be unusable,
  though, and the 56-tone target would have no margin.
- **Cancel clipping noise in the receiver.** Iterative reconstruction can
  recover much of the loss without changing the transmitter, but it costs
  receiver compute and makes the top rungs depend on an optional receiver
  feature. It remains open as a v1.x receiver improvement.
- **Keep the textbook SNR column and absorb the difference in the Stage 2
  margin.** That would hide a known 1.5–3 dB error in a margin meant for
  hardware effects.
