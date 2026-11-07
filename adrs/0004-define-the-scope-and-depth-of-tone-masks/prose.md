---
title: "Define the scope and depth of tone masks"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0002 adopted a tone mask: the receiver reports which probed tones are
usable, and the transmitter "sends zero energy on masked tones, reassigns
coded bits to the remaining tones, and keeps total transmit power constant"
(`masked-tone-power-reassignment`, SPEC §5.4). Building the mask into the
Stage 1 simulation (`sim/`, `sim/findings.md`) showed that the statement
cannot be met as written and leaves four questions open.

- **Zero energy is not reachable.** Clip-and-filter (SPEC §7.1) clips each
  symbol, filters it back to the active tones and clips it once more. The
  last clip regrows energy on every tone outside the kept set, masked tones
  as much as the tones above the band:

  | Clip level | Masked tones below the active tones |
  | --- | --- |
  | 7 dB (MCS 0–3) | about 30 dB |
  | 8 dB (MCS 4) | about 35 dB |
  | 9 dB (MCS 5) | about 42 dB |
  | 10 dB (MCS 6) | about 47 dB |

  Pushing the 7 dB case to 40 dB takes two or three more clip-and-filter
  passes and costs 3–4 dB of in-band signal-to-distortion ratio, which the
  low rungs would pay for nothing: the regrowth wastes about 0.1% of power,
  on tones the channel is already losing.
- **Which symbols does the mask cover?** SPEC §5.4 does not say whether the
  preamble, header and width probe follow it.
- **What does "total power constant" mean?** Raising only the data tones
  would contradict SPEC §6.2's rule that the transmitter scales the whole
  symbol, never individual tones.
- **What happens to pilots 37 and 52 when masked?** Only pilots 3 and 20 are
  protected from masking.
- **What does a 56-bit mask mean at a narrower width?**

The masks matter in practice. On the simulated repeater cascade, a 5 ms
delay spread leaves tones 0–2 near −10 dB SINR. MCS 5 never decodes and
MCS 6 stops at a 9% error floor until those tones are masked; with them
masked, MCS 3–6 come within 0.2–1.5 dB of white noise.

## Decision

Masked tones carry no data and no pilots, and the energy left on them after
clip-and-filter may be no higher than the regrowth just above the band
(`masked-tone-content`). Total power is defined as the power of `width`
unit-power tones; every active tone, pilots included, is scaled by the same
factor.

The mask applies to the symbols that carry data and control blocks. The
preamble and header always use every base tone, and the width probe every
tone up to the sender's cap, so a receiver can find and decode a frame
without knowing the mask, and can keep measuring masked tones. A 56-bit mask
applies at any width; bits at or above the width are ignored
(`tone-mask-scope`).

Pilots 37 and 52 may be masked. A masked upper pilot is absent from data and
control symbols, and tracking uses the remaining pilots; both stay in the
probe (`masked-upper-pilots`).

## Consequences

- **No transmitter change.** Clip-and-filter already behaves this way; the
  spec now says what it actually does, and the limit is testable against the
  out-of-band regrowth the transmitter produces anyway.
- **The distortion index stays consistent.** It counts energy on masked
  tones as intermodulation (SPEC §7.1, B.2). ADR 0003
  (`distortion-index-reference`) already excludes the rung's designed
  clipping, which is exactly this regrowth, so a correctly driven radio
  still reads 0.
- **Headers do not benefit from masks.** A header still uses the base
  tones a mask has dropped. On the repeater profile that leaves the header,
  not the data, limiting MCS 0, by about 0.9 dB. The rate-1/3 contingency
  header (decision 20.4) is the remedy if Stage 2 shows it matters.
- **Test vectors must cover a masked case.** The bit-mapping ADR's vectors
  include at least one masked configuration, so implementations agree on the
  scale factor and the bit layout around masked tones.

## Alternatives Considered

- **Require deeper nulls.** Extra clip-and-filter passes on masked frames to
  reach about 40 dB. Rejected: 3–4 dB of in-band quality at the low rungs to
  remove 0.1% of wasted power.
- **State a fixed depth (for example 25 dB below the active tones).** A
  number is easy to test, but it would be either too loose at the high clip
  levels or too tight for a future clip change. Tying the limit to the
  out-of-band regrowth moves with the clip level.
- **Apply the mask to the header too.** The receiver would need the mask
  before decoding the header that tells it which mask is in force, and a
  station hearing the frame without a session could not decode it.
- **Scale only the data tones.** It keeps pilot power fixed, but breaks the
  whole-symbol scaling of SPEC §6.2 and with it the receiver's assumption
  that per-tone gains from the preamble hold across the frame.
