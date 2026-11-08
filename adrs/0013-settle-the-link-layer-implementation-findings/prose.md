---
title: "Settle the link-layer implementation findings"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

Two pieces of implementation work tested ADRs 0008–0012 against code. The
PHY field work (PR #21, merged) built every new header bit, control block
and PDU rule bit-exactly in Python and Rust and published vectors for
them. The link layer (PR #22, `modem/link`) built the session state
machine as a sans-IO crate and ran it in a frame-level simulation with
loss, collisions, outages and third stations. Both kept a list of places
where the specification left room, and PR #22 named each reading it took
in a `READING_*` constant.

Two findings were conflicts with §14.1, not gaps:

- **Upward adaptation could not meet its target.** §14.1 asks for the
  correct rung within 2 frames of a 6 dB change, but ADR 0010
  `adaptation-rules-revised` steps up one rung per three clean frames. In
  simulation a 6 dB drop settled in 1 frame and a 6 dB rise took 6–7.
- **The resume target never tested resume.** Link loss is declared on the
  8th ACK-timer expiry, about 55–90 s after the last decoded header, so a
  60 s outage normally ends inside the live session. Resume itself needs an
  outage of about 85 s or more.

The rest were gaps:

- **Timers and cadences.** "One ACK-timer period" of DISCONNECT linger,
  counted from the reply, ended before the closer's first resend arrived.
  No cadence was given for asking for a re-probe when a masked tone might
  have recovered.
- **Setup and resume edges.** Nothing covered two stations sending each
  other new CONNECTs at once, or which roles survive a resume. A responder
  that gave up on a half-open session answered the initiator's later resume
  as a new session, so the initiator reported its whole first transfer
  undelivered although nothing had been lost.
- **Smaller readings.** Yield-window details, the step-up run with clipped
  or distorted frames, the per-rung frame limit for broadcasts, an empty
  resend after DATA, slot timing in carrier sense, the dictionary-only B.3
  block, the ACK block inside a resume CONNECT-ACK, CONNECT-ACK's probe
  width and mask, Mask IDs outside a session, the block map's position,
  the CRC of a reassembled broadcast message, and the control-block order
  of BEACON and UI frames.

## Decision

**Adaptation and targets (decided by the project owner).** After three
consecutive clean frames that each clear the next rung by 2 dB, the
receiver requests the highest rung whose threshold is at least 2 dB below
the lowest SNR of the three, not just the next rung; every other row of the
table is unchanged (`adaptation-rules-multi-rung`). The §14.1 targets now
ask for the correct rung within 2 frames after a 6 dB drop and within 4
after a 6 dB rise, and test resume twice: a 60 s outage recovers with no
block lost or delivered twice, and a 120 s outage, past link loss, resumes
with no acknowledged block resent (`v1-performance-targets-revised`).

**Setup, resume and teardown.** A half-open session that the responder
gives up on is kept for resume like a lost link, so a later resume
continues it (`duplicate-connect-and-half-open-retention`). A station
already connecting to a peer refuses that peer's crossing new CONNECT and
carries on with its own attempt (`crossing-new-connects`). A resumed
session keeps its original roles: keepalive duty and the responder's
timer offset (`resume-keeps-roles`). After replying to DISCONNECT, the
peer answers repeats until a linger time, the closer's longest timer plus
the carrier-gone deadline and one DISCONNECT on the air, has passed since
its latest reply (`disconnect-handshake-linger`).

**Masks and broadcasts.** A receiver with a dropped tone asks for a width
probe when it has seen none for 10 s (`reprobe-request-cadence`). BEACON
and UI frames carry the ID block, then the path block, then others
(`broadcast-control-block-order`).

**Interpretations.** The readings that only clarify a decision are
recorded as interpretations of it:

| ADR | Decision | Interpretation |
| --- | --- | --- |
| 0002 | `frame-length-by-rung` | `rung-limit-covers-broadcasts`: the per-rung limit binds broadcasts too |
| 0008 | `block-map-control-block` | `block-map-only-second`: a block map counts only as the second control block |
| 0009 | `broadcast-message-fragments` | `reassembled-message-crc`: slices are checked one by one; the delivered CRC is recomputed |
| 0010 | `yield-window-clock` | `yield-window-fails-on-raw-dcd`: any raw-detector rise fails the window |
| 0010 | `transmission-from-idle` | `yield-then-carrier-sense`: the yield comes first, then full carrier sense |
| 0010 | `idle-keepalive` | `keepalive-takes-a-yield`: the effective interval is about 31–33 s |
| 0010 | `ack-timer-rules` | `empty-resend-after-data-is-sound`: an emptied DATA resend goes as SOUND |
| 0010 | `csma-procedure-details` | `csma-draws-start-now`: draws start when the transmission is queued |
| 0010 | `session-establishment-steps` | `dictionary-only-b3-marker`: width cap and frame time both 0 mark the second B.3 |
| 0011 | `mask-in-every-session-ack` | `resume-ack-carries-setup-mask`: the resume ACK repeats B.3's mask under ID 0 |
| 0011 | `width-probe-extent` | `connect-ack-probe-width`: the initiator finds the width from the B.3 block or the probe |
| 0011 | `mask-id` | `connect-ack-decoded-unmasked`: CONNECT-ACK is unmasked by frame type |
| 0011 | `mask-id` | `mask-id-outside-session`: a nonzero Mask ID there is ignored and never sent |
| 0013 | `adaptation-rules-multi-rung` | `step-up-run-conditions`: every frame of the run meets the SNR condition; clipped and distorted frames restart it |

## Consequences

- **Adaptation follows a rising channel in 4 frames** instead of one rung
  per three frames. A jump of several rungs rests on three clean frames
  and the lowest of three readings, and an overshoot is corrected by the
  unchanged immediate step-down.
- **The §14.1 targets match what the protocol can do** and now test resume
  itself. The link-layer simulation already meets the new rows for drops,
  outages and resume; rises need the multi-rung step-up.
- **Half-open sessions cost the responder 10 minutes of retained state**,
  the same as a link loss, and save the initiator's host a resubmission.
- **The implementation changes slightly.** PR #22 already follows every
  decision here except the multi-rung step-up, the lowest-of-three SNR,
  restarting the DISCONNECT linger at each reply, and keeping half-open
  state; those are small changes to `adapt`, `station` and `session`.
- **The vectors README's last simulator reading** (the dictionary-only B.3
  block) is now ADR 0010 `dictionary-only-b3-marker`.
- **Follow-up.** Exchange-transcript vectors for the crossing CONNECT, the
  half-open resume and the DISCONNECT linger; a bench test of the 4-frame
  rise target on real radios.

## Alternatives Considered

- **Step up one rung at a time, faster** (after one or two clean frames).
  Meets the rise target only with a run too short to resist a noisy
  reading, and still takes one round trip per rung on a large rise.
- **Use the SNR of the last frame of the run** for the multi-rung jump.
  Simpler, and normally the same figure, but one optimistic reading could
  carry the request several rungs too high; the lowest of three costs
  nothing at constant SNR.
- **Keep the 2-frame target in both directions.** It would require
  stepping up on a single clean frame, which the hysteresis of §10 exists
  to prevent.
- **Answer a crossing new CONNECT and abandon one's own attempt.** It
  saves one exchange and avoids telling a host "refused" just before a
  session arrives, but needs a tie-break when each station hears the
  other's CONNECT, while refusing is already a defined answer.
- **Confirm the half-open behaviour as it was.** It is safe, because the
  undelivered data is reported, but it turns a lost first frame into a
  resubmission for no gain: the responder already has everything a resume
  needs.
- **A fixed DISCONNECT linger, such as 10 s.** Simpler, but too short for
  a repeater-mode session with 4 s frames and longer than needed at short
  frames; deriving it from the timer tracks both.
- **Leave the re-probe cadence to implementations.** It does not affect
  interoperability, but the §14.1 tone-restore target needs some cadence,
  and a stated default keeps implementations comparable.
- **Put the path block first in broadcast frames.** A relay rewrites it,
  but it re-encodes the whole frame either way; the ID block, present in
  every BEACON, leads instead.
