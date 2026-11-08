---
title: "Step up after one clean frame following a reply-rule request"
date: 2026-11-08
authors: ["Jeremy Banker"]
---

## Context

A session's first rung comes from the reply rule (SPEC §10.1): the station
answering a CONNECT, CONNECT-ACK or SOUND requests the highest rung whose
threshold is at least 3 dB below the SNR it measured on that frame. Those
frames are sent before the receiver's tone mask is in use, because the mask
is chosen from that very measurement (ADR 0011). PR #34 made the reply rule
count only the tones the new mask keeps, which moved the first rung on the
2026-11-08 1.25 m tests from MCS 2 to MCS 4.

It still opens a rung or two below what the link carries, and the reason is
physical, not a measurement error. The radios' high-pass filter delays
tones 0-2 by 1.4-2.8 ms, longer than the 1.25 ms cyclic prefix. While those
tones are sent, their energy spills into the next symbol on tones 3-7 and
reads as noise there; masking tones 0-2 removes it (tone 3's noise drops
5-5.7 dB). On the same kept tones (3-31) at MCS 4, the recordings read:

| Direction | Unmasked | Masked |
| --- | --- | --- |
| A→B | 23.9-24.2 dB | 26.3-26.9 dB |
| B→A | 24.5-24.8 dB | 28.6 dB |

Simulation shows short frames carry no frame-length bias (at most 0.33 dB),
so the gap is what the unmasked frame really sees. A perfect prediction of
the masked link from the unmasked frame would need a model of each radio's
impulse response; an offline fit over-predicted by 1.5-3.5 dB.

After the reply rule, ADR 0013 `adaptation-rules-multi-rung` needs three
clean frames before a step up. With 9.6 s cycles that is about 30 s at the
start of every session spent a rung or two low, although the first data
frame already shows the masked link as it is.

## Decision

The first step-up run after a reply-rule request is one frame instead of
three (`step-up-after-reply-rule`). The first frame with data blocks that
the receiver judges after replying to a CONNECT, CONNECT-ACK or SOUND steps
up on its own if it has no block errors and its SNR is at least 2 dB above
the next rung's threshold, to the highest rung whose threshold is at least
2 dB below that frame's SNR. Whatever that frame does, the short run is
used up: if it holds or steps down, or a header is lost first, the next run
needs three frames as before.

A frame without data blocks does not count, and neither does one the
receiver measured with its ADC clipped or at distortion index 3 or higher;
the short run waits for the next judged frame. The step-down rules, the
margins and everything else in ADR 0013 are unchanged. The decision
restates ADR 0013's table in full with the one row changed.

This changes only how a receiver picks the rung it requests. Nothing on
the air changes, and a station following the old rule still interoperates.

## Consequences

- **Sessions reach their working rung about two frames sooner.** In the
  link simulation with the radios' spill modelled (unmasked frames reading
  4 dB low, `first_masked_data_frame_steps_up_alone`), the second data
  frame of each direction goes at MCS 6 instead of the fourth. On the 1.25
  m pair the expected effect is: B→A (28.6 dB masked) steps from MCS 4
  straight to MCS 6; A→B (26.3 dB) steps to MCS 5, then to MCS 6 after
  three more clean frames as now. Either way about 20 s is saved per
  session on these radios.
- **After an idle period, the same applies.** A keepalive SOUND's reply is
  a reply-rule request, so the first data frame after it can restore the
  rung the SOUND under-read. Keepalives are sent only when the session is
  idle, so on a busy link the rule changes nothing.
- **One optimistic reading can overshoot once per request.** The run's
  lowest SNR was the protection against that; with a run of one there is
  only the 2 dB margin. An overshoot costs one frame: more than 20% of its
  blocks fail and the rung steps down at once. The link simulation's
  no-oscillation and adaptation tests pass unchanged.
- **A deference test changed.** With the new timing, the two-station
  simulation's Bell 202 burst landed 40 ms into a station's 50 ms
  turn-holder window (ADR 0010), before any detector could see it: quieting
  needs about 50 ms to see a new carrier, the AFSK detector 80-90 ms. The
  station keyed over the burst's start. That race existed before; the old
  timing happened to place the burst where quieting caught it. The test
  now checks what SPEC §14.1 asks, that no station starts a transmission
  after it has detected the burst, until 100 ms after it ends, and allows
  an undetected start only within 60 ms of the burst's start.
- **Implemented** in `cairn_link::adapt::RateAdapter`
  (`params::STEP_UP_RUN_AFTER_REPLY_RULE`), with SPEC §10.1 updated.

## Alternatives Considered

- **Keep three frames (ADR 0013 as it is).** Costs about 30 s per session
  on radios like these, for no gain in safety that the first masked data
  frame does not already provide.
- **Predict the masked link from the unmasked frame.** Estimating each
  tone's spill needs the radio's impulse response; the offline fit
  over-predicted by 1.5-3.5 dB, which would overshoot instead of
  undershoot. It would also change what the ADR 0006 SNR means.
- **Lower the reply rule's 3 dB margin.** It would raise the first rung on
  every link, including those where the unmasked measurement is accurate
  and the margin is needed for the frame-type difference.
- **Send CONNECT-ACK and SOUND masked.** The CONNECT-ACK's sender does not
  yet know the mask for its own direction, which its peer chooses from that
  frame, and a SOUND exists to measure all tones.
- **Remember each peer's mask and rung from the last session.** Helps only
  repeat contacts, needs stored per-peer state and a rule for when it is
  stale. It remains possible later as a local optimization.
