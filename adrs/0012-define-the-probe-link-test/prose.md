---
title: "Define the PROBE link test"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

The link test (SPEC §9.6, ADR 0002 `link-test`) lets an operator ask "how
fast could I talk to that station?" without opening a session: a PROBE, and
a SOUND reply carrying the target's measurements. The pre-implementation
review of the link layer found that the frames were never defined:

- SPEC §8.4 says a PROBE asks for "a link-test reply or a BEACON", but no
  field says which.
- Nothing says the PROBE carries a width probe, yet the target must report
  a usable tone mask, which needs the upper tones measured.
- The reply's ACK-type block has base-sequence and bitmap fields with no
  meaning outside a session, and its timing (carrier sense or turn-holder
  reply) was unstated.
- Nothing says whether a station in a session should answer, which would
  break its ping-pong.

## Decision

A PROBE carrying one B.3 block is a link test, with a width probe when the
sender is wider than 24 tones; a PROBE with no control blocks asks for a
BEACON (`probe-frame-contents`). The link-test reply is a SOUND with the
target's measurements in an ACK block whose base and bitmap are zero, sent
as a turn-holder reply, with the 5 s unreachable rule kept
(`probe-link-test-reply`). A BEACON request is answered with a BEACON in
the same way (`probe-beacon-request-reply`). A station in a session answers
neither (`probe-not-answered-in-session`).

## Consequences

- **The link test works between any two implementations** with fields that
  already exist: a B.3 block, an ACK block, and the width probe.
- **The target learns the prober's capabilities and cap** from the B.3
  block, so its suggested rung and mask cover the right tones.
- **A busy station looks unreachable.** The host may retry later, and the
  `heard` list shows the station's recent session traffic.
- **Follow-up.** Frame vectors for both PROBE kinds and the SOUND reply,
  and an exchange transcript with its timing window.

## Alternatives Considered

- **A flag for the BEACON request.** Clearer, but the header has no free
  flag bits left (ADR 0008, ADR 0009), and the control-block count already
  distinguishes the two.
- **Mark the link test by the width probe alone.** It fails at 24 tones,
  where no probe is needed.
- **Answer PROBEs with carrier sense.** It adds random delay to an exchange
  whose result is reported as a throughput estimate, and the reply is
  expected right away, just as a session reply is.
