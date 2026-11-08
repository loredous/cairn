---
title: "Run sessions on a defined state machine and channel-access timing"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0002 adopted the session and channel-access rules of SPEC §9–§11 at the
level of a design sketch. The pre-implementation review of the link layer
found that two careful implementers would build sessions that do not work
together:

- **The ACK timer was undefined.** "Expected peer frame time + 1.0 s"
  named neither the expected time nor when the timer starts, the peer's
  TXDelay (up to 500 ms, plus up to 1 s of access delay in repeater mode)
  cannot be learned, and the action on the first two expirations and the
  meaning of "8 consecutive failures" were missing. Equal timers on both
  sides would let two stations that lost each other collide again and
  again.
- **No rule said which frames need a reply.** If an ACK frame needed one,
  an idle session would never go quiet.
- **Three idle rules disagreed.** §9.3 let the session idle "up to 60 s",
  §10.1 sent a SOUND after 30 s with no frame from the peer, and the §10
  figure re-measured "after 30 s idle".
- **The turn-holder's DCD check could never pass.** The turn-holder checks
  DCD for 50 ms before replying within 100 ms of the peer's frame, but DCD
  holds for 100–300 ms after the peer's own carrier, so a literal check
  always reads busy.
- **The yield window broke the timers.** A 1.0 s yield (2 s in repeater
  mode), plus turnaround and TXDelay, exceeds the 1.0 s margin of the
  peer's ACK timer.
- **Setup and teardown left gaps.** There was no CONNECT-ACK wait,
  "increasing random intervals" was undefined, and duplicate CONNECTs,
  refusals, a lost DISCONNECT reply, CONNECT's requested MCS before any
  measurement, and the responder's identification were not specified.
- **Smaller gaps.** The ID cadence and control-block budget, edge cases of
  the adaptation table, whether the maximum frame time includes TXDelay,
  how p-persistence restarts and maps to KISS, the broadcast frame counter,
  KISS UI addressing, broadcasts during a session, and CAL bursts that
  outlast the 4.0 s frame ceiling were also undecided.

## Decision

**Setup and teardown.** CONNECT, CONNECT-ACK and the initiator's first
frame (DATA, or ACK with no data) have fixed control-block contents and
order, and CONNECT-ACK now carries the responder's ID block
(`session-establishment-steps`). CONNECT's requested MCS is 0
(`connect-requested-mcs-zero`). CONNECT waits 3 s and retries with backoffs
of k × U(1, 5) s (`connect-retry-schedule`, `retry-backoff-uniform`).
Repeated CONNECTs and CONNECT-ACKs are answered again
(`duplicate-connect-handling`), a refusal is a DISCONNECT
(`connect-refusal`), a station holds one session at a time
(`one-session-at-a-time`), and DISCONNECT is retried and answered once
(`disconnect-handshake`).

**Turn-taking.** Data, SOUND, DISCONNECT and CAL-REQUEST need a reply, and
an ACK frame does not (`reply-required-frames`). The ACK timer has a
formula covering the slowest legal reply, pauses on DCD, has an offset and
jitter, and escalates from retransmission through SOUND to link loss
(`ack-timer-rules`). The turn-holder's 50 ms check uses the raw detectors
once the peer's carrier has dropped (`turn-holder-reply-check`). A session
transmission from idle uses carrier sense (`transmission-from-idle`).

**Idle and keepalives.** An idle session exchanges a keepalive SOUND every
30 s: the original initiator sends it, and the responder sends one only as
a fallback after 35 s plus jitter (`idle-keepalive`). The reply is the
ordinary SOUND reply (`sound-reply`). Closing an idle session is a host
setting, off by default, that keepalives do not reset
(`idle-disconnect-policy`). This replaces all three idle rules: there is no
60 s silence, and the keepalive is the 30 s SOUND of §10.1 and the
re-measurement of the §10 figure.

**Yield windows.** Each station keeps its own 30 s yield clock, and the
peer's timer allows for a yield at any turn (`yield-window-clock`).

**Identification and adaptation.** Each station identifies at least every
9 minutes (`session-id-cadence`), and control blocks follow a fixed order,
at most three per frame (`control-block-order`). The adaptation table's
edge cases are fixed (`adaptation-rules-revised`), and the sender uses
exactly the requested rung (`sender-uses-requested-rung`).

**Channel access and broadcasts.** Frame time is PTT-on time
(`frame-time-definition`), and a calibration sweep may run to 6 s
(`calibration-transmission-limit`). The p-persistent procedure has a
restart rule and a KISS mapping (`csma-procedure-details`). The broadcast
frame counter starts at a random value (`broadcast-frame-counter`). KISS
traffic goes to the broadcast address with several PDUs per UI frame
(`kiss-ui-frames`), and a station in a session broadcasts only while the
session is idle (`broadcast-during-session`).

## Consequences

- **Every session state has a defined exit.** A frame that needs a reply
  either gets one, is retransmitted, escalates to SOUND, or ends in link
  loss and resume, with the same timing at both ends.
- **Recovery is slower than the ideal.** At 4 s frames the ACK timer runs
  about 6.6 s (7.1 s at the responder), so a lost frame costs a few
  seconds. Most failures stop the timer early: any decoded peer header
  stops it, and DCD from the peer's frame pauses it.
- **Idle sessions use the channel.** A keepalive and its reply take about
  1 s of air time every 30 s, roughly 3% of a shared channel, in exchange
  for fresh measurements and loss detection within about a minute.
  Operators who want quiet close idle sessions with the idle-disconnect
  setting.
- **The responder always identifies** from its first transmission, and a
  long callsign's full text reaches the initiator at connect.
- **Appendix A's frame-time accounting becomes normative**:
  TXDelay and TXTail count against the 4 s.
- **Turnaround can exceed 100 ms** when the peer has a long TXTail or slow
  unkeying, because the 50 ms check starts only when the peer's carrier
  drops. The peer's ACK timer has ample margin for this.
- **Follow-up.** The native API's `channel` event can report the yield
  clock and the keepalive schedule. Conformance needs exchange-transcript
  vectors with timing windows for each scenario: retries, refusal, a lost
  ACK, link loss, keepalive and yield.

## Alternatives Considered

- **No keepalive.** Idle sessions stay silent and the first frame after a
  long idle is a SOUND. Quieter, but a lost link is found only at the next
  transfer, and measurements age without bound.
- **Both stations send keepalives on equal timers.** Simpler, but they
  collide or double the traffic.
- **Exchange TXDelay in CONNECT to tighten the timer.** B.3 has no free
  bits, and the gain is a second or so on a rare event.
- **Make timer expiries use p-persistence.** It would protect better
  against hidden third stations, but it adds random delay at the moment the
  peer is most likely waiting; the 50 ms raw-DCD check and the pause on DCD
  already avoid talking over a station that can be heard.
- **A shared yield schedule.** A session clock agreed at setup would align
  the stations' yields, but frames cross and clocks drift. Making the
  timers tolerate an independent yield is simpler and safe.
- **Release PTT between calibration bursts.** It would keep within 4 s,
  but every key-up disturbs the level chain the sweep measures.
