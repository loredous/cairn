---
title: "Measure SNR without the transmitter's designed clipping"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0003 set the clip level by rung (7 dB at MCS 0–3, then 8, 9 and 10 dB)
and referenced every reported SNR to a 7 dB clip (`snr-reference`): the
receiver measures the effective SNR over the active tones and adds the clip
level minus 7 dB. It did not say whether the distortion clip-and-filter
produces by design counts as noise in that measurement. A receiver that
simply compares received tones with the ideal constellation counts it.

The first end-to-end test of the bench tool (`cairn-bench`) showed what that
does. On a perfect digital link, an MCS 0 frame reported 24.3 dB and the
rung pick stopped at MCS 4, although MCS 6 decodes without error. The
receiver cannot report more than the frame's own signal-to-distortion ratio
(SPEC §7.1):

| Frame sent at | Clip level | Ceiling on reported SNR (7 dB-equivalent) |
| --- | --- | --- |
| MCS 0–3 | 7 dB | about 21.7 dB |
| MCS 4 | 8 dB | about 26.9 dB |
| MCS 5 | 9 dB | about 33 dB |
| MCS 6 | 10 dB | about 40 dB |

The MCS 6 threshold is 25.5 dB, so no MCS 0 frame can ever justify it.
CONNECT, SOUND and PROBE replies are sent at MCS 0, so:

- the first-frame rule of SPEC §10.1 ("the highest rung whose threshold is
  3 dB below the measured SNR") tops out at MCS 4 however good the link,
  and the session climbs one rung per three clean frames from there;
- the link test reports strong links as only MCS 4–5 capable;
- the step from MCS 4 to MCS 6 depends on a 1.4 dB gap between the MCS 4
  ceiling and the MCS 6 threshold, smaller than the Stage 2 margin may be.

The thresholds in SPEC §6.2 were measured against the channel's SNR, so a
reported SNR that includes the designed clipping does not even compare
with them on the same basis.

Three estimators were compared in the Stage 1 simulation (white noise,
perfect timing, five frames per point, 7 dB-equivalent dB):

| Rung | Channel SNR | Counting the clipping | Subtracting a fixed allowance | Rebuilding the sent symbols |
| --- | --- | --- | --- | --- |
| MCS 0 | 15 | 14.2 | 15.0 | 14.9 |
| MCS 0 | 25 | 20.4 | 26.4 ± 1.4 | 25.0 |
| MCS 0 | 35 | 22.0 | 55.9 ± 8.3 | 35.0 |
| MCS 4 | 35 | 26.9 | 47.2 ± 11.3 | 34.9 |
| MCS 6 | 45 | 39.1 | 51.5 ± 9.6 | 45.0 |

Clipping distortion varies from frame to frame, so a fixed allowance cannot
be subtracted once the channel is cleaner than the clipping. Rebuilding the
transmitted symbols from the decoded bits (re-encode, map, repeat
clip-and-filter) and comparing the received tones with those recovers the
channel SNR within 0.1 dB everywhere.

## Decision

The reported SNR is the channel's: signal over everything the channel and
both radios add, including a radio's own clipping, but not the distortion
clip-and-filter produces by design at the frame's clip level. It stays on
the 7 dB-equivalent scale of ADR 0003 (`snr-reference-channel`, superseding
`snr-reference`).

Accuracy is required where the receiver knows what was sent: the preamble,
the width probe, and every symbol whose blocks all decoded. There the figure
must be within 1 dB of the channel SNR up to at least 40 dB. Elsewhere the
receiver may count the designed clipping as noise, so the figure errs low,
but it must never over-report by more than 1 dB (`snr-measurement-accuracy`).
How the receiver meets this is its own choice; rebuilding the sent symbols is
the method Stage 1 validated.

## Consequences

- **Strong links are found on the first frame.** A CONNECT or SOUND at
  MCS 0 can justify MCS 6, and the link test shows a strong link's real
  capacity.
- **The distortion index and the SNR now agree on what counts.** ADR 0003
  `distortion-index-reference` already excludes the designed clipping; a
  radio that clips beyond it shows up in both.
- **Receivers do more work per frame.** Rebuilding means re-encoding the
  decoded blocks and repeating clip-and-filter: about the cost of
  transmitting the frame, well within the real-time budget.
- **Rebuilding works best when clip-and-filter is reproducible.** SPEC §7.1
  specifies it (clip, filter to the active tones, clip again), but ADR 0005's
  vectors stop before it. Two implementations whose filters differ slightly
  will see the difference as noise, a residue far below the designed
  clipping. Stage 2 and the conformance suite should confirm that the 40 dB
  requirement holds between independent implementations, and a later ADR can
  add clip-and-filter vectors if it does not.
- **Weak frames lose almost nothing.** A frame that fails to decode is near
  or below its threshold, where the designed clipping is at least 9 dB below
  the channel noise; counting it lowers the figure by under 0.5 dB.
- **`cairn-bench rx` must follow.** The bench tool reports SNR counting the
  designed clipping today; it should switch to rebuilding so its rung picks
  match the protocol.

## Alternatives Considered

- **Keep counting the designed clipping and accept the climb.** Simpler
  receivers, but every session on a strong link starts at MCS 4 and needs
  about six clean frames to reach MCS 6, the link test under-reports, and
  reported figures do not compare with thresholds measured on the channel.
- **Subtract a fixed allowance per clip level.** No rebuilding needed, but
  Stage 1 showed it unusable above about 3 dB beyond the clipping level: it
  over-reports by up to 20 dB, which would push links onto rungs they
  cannot sustain.
- **Send CONNECT, SOUND and PROBE replies at a higher clip level.** Raises
  the ceiling for those frames only, at the cost of up to 3 dB of average
  power on exactly the frames that must work near the FM threshold.
- **Interpret ADR 0003's statement instead of superseding it.** The figure
  two stations exchange changes meaning, which affects interoperability, so
  it belongs in a decision rather than an editable interpretation.
