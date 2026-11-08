---
title: "Set field encoding conventions and probe every wide frame"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

Implementing real headers and control blocks (SPEC §8.2, Appendix B) in both
the Python reference and the Rust modem, with test vectors in
`vectors/header/`, `vectors/control/` and `vectors/frame/`, needed answers
the SPEC did not give. Each answer was chosen once and used in both
implementations and the vectors; this ADR makes them normative so a third
implementation cannot guess differently.

- **Bit order.** The tables say "bit 0", "bit 4" and so on without saying
  which end bit 0 is or in what order bits go on air. Every flag field,
  bitmap and tone mask depends on it.
- **Absent and reserved fields, header versions.** What to send in an
  optional field whose flag is clear, and what a receiver does with reserved
  bits or a version it does not know.
- **Header MCS with no data blocks**, malformed addresses, SNR rounding and
  the grid locator's padding: small points where two careful readings could
  differ.
- **Frames wider than 24 tones.** The preamble's S2 pair measures only the
  base tones 0–23. The width probe measures the rest, but ADR 0002
  (`width-probe`) requires it only in CONNECT, CONNECT-ACK, SOUND and
  re-probe frames. Nothing said how a receiver estimates tones 24–55 in an
  ordinary wide DATA frame. Both Stage 1 receivers need a probe in the same
  frame and refuse a wide frame without one.
- **Wide broadcasts.** ADR 0002 (`broadcast-fixed-width`) lets the operator
  set a broadcast width above 24 tones and says broadcasts never probe. The
  header does not carry the width, and a listener outside a session has no
  link state, so it could neither know the width nor measure the upper
  tones.

## Decision

Field bits are numbered by weight (bit n is worth 2^n), sent most
significant first; one-bit-per-item fields put item i at bit i; sequences of
multi-bit items put the first item most significant (`field-bit-order`).
Absent optional fields and reserved bits are zero and ignored, and a header
with a version other than 1 is discarded (`absent-and-reserved-fields`). The
header's MCS is ignored when there are no data blocks
(`header-mcs-without-data`), malformed addresses are refused
(`malformed-addresses`), the ACK's SNR is rounded to the nearest 0.25 dB
(`snr-field-quantization`), and the grid locator is space-padded
(`grid-locator-padding`).

Every frame whose data blocks use tones above 23 carries the width probe,
and the receiver estimates those tones from it (`width-probe-every-wide-frame`,
superseding `width-probe`). Broadcasts wider than 24 tones do the same, and
a listener takes the width from the probe's extent
(`broadcast-width-from-probe`, superseding `broadcast-fixed-width`).

## Consequences

- **The vectors are normative as they stand.** `vectors/header/`,
  `vectors/control/` and `vectors/frame/` already follow these rules; no
  change to either implementation's field encoding.
- **Wide frames cost 22.5 ms more.** Two probe symbols per wide frame: about
  0.5% of a 4 s frame and 1.1% of a 2 s frame. Appendix A's net rates at
  widths above 24 tones fall by that much; the budget model
  (`sim/cairn_sim/link_sim.Budget`) should count the probe in every wide
  frame.
- **Every wide frame gets a fresh channel estimate.** No dependence on the
  audio chain staying the same between frames, and the receiver sees tones
  above the current width in every frame, so §10.3 can restore masked tones
  without waiting for a re-probe. The re-probe flag still matters on 24-tone
  links.
- **The probe extends to the sender's cap.** Receivers must take the probe's
  extent from the sender's cap (B.3 width cap in a session, the probe's own
  extent in a broadcast), not assume it equals the link width. Both
  implementations currently assume the link width and need that change.
- **Wide broadcasts are decodable by anyone.** A listener with no session
  finds the width from the probe. The listener must test each tone above 23
  for the probe sequence; with two known symbols per tone that is a simple
  correlation, robust well below the data's threshold.
- **Prefix length stays out of the header.** Receivers detect it from the
  preamble (SPEC §6.1), as both implementations do.

## Alternatives Considered

- **Reuse the most recent probe's estimate, corrected by the pilots.** Saves
  the 22.5 ms, but depends on the audio chain's per-tone gain and phase
  staying the same between frames, through key-ups, AGC and level changes,
  which Stage 2 has not measured. It can be added later as an option if
  Stage 2 shows the chain is stable.
- **Extend S2 to the full width.** Zero extra air time, since S2 is already
  sent, but it changes the preamble fixed by ADR 0005 and its vectors to
  save about 0.5%.
- **Restrict broadcasts to 24 tones.** Simpler for listeners, but it removes
  a configuration the SPEC offers to operators who know their audience.
- **Number bits the other way (bit 0 first on air).** Equally workable; the
  chosen order matches how B.1 already orders callsign characters and how
  both implementations and the vectors were written.
