---
title: "Identify blocks by a block map and acknowledge cumulatively"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

A review of the link layer before implementation (SPEC §8–§12) found that
the selective-repeat rules of ADR 0002 contradict each other:

- `block-sequence-numbering` says a frame carries up to 128 **consecutive**
  blocks starting at the header's sequence number, while `selective-repeat`
  says the next frame retransmits **exactly the unacknowledged blocks**
  followed by new ones. The unacknowledged blocks are rarely consecutive,
  so both cannot hold.
- `ack-bitmap` and the B.2 layout give the ACK base as "the sequence
  number of the peer's last frame" with bit i meaning block base + i, which
  only works for consecutive frames. SPEC §9.2 uses the same field on
  resume for something else: "its last acknowledged sequence".
- A data block (B.4) carries no sequence number, and its CRC-16 does not
  cover one. A block mapped to the wrong sequence passes its CRC and is
  delivered silently; only the PDU CRC-32 notices, after delivery.
- `in-order-delivery-hold-limit` asks the receiver to "NACK everything
  beyond the gap" after 256 held blocks, which a 128-bit bitmap cannot say.
- Nothing fixed the first sequence number, the wraparound rule, the header
  sequence of a frame without data, or what to acknowledge after a header
  that did not decode.
- Resume did not say which negotiated state survives, who may resume, or
  how an initiator learns that the responder started a new session instead.

Two implementations would have built incompatible retransmission frames
from the text as it stood.

## Decision

Blocks are numbered from 0 in each direction, compared with RFC 1982
serial arithmetic, and sent in ascending order; the header names the first
block (`block-sequence-rules`). A frame whose blocks are not one
consecutive run carries a **block map**, a new control block of type 7, as
its second control block, flagged by header flag bit 7
(`block-map-control-block`, `reserved-control-types-after-block-map`).

The ACK becomes **cumulative**: its base is the receiver's next in-order
sequence, and bit i says whether the receiver holds block base + i
(`cumulative-ack`). A sender stays within 128 blocks of its lowest
unacknowledged block (`send-window`), so one map and one bitmap always
cover everything outstanding. After a lost header, the next ACK repeats the
previous one, with only the requested MCS updated (`ack-after-lost-header`).
In-order delivery keeps its hold-behind-a-gap rule, with no separate limit
(`in-order-delivery`).

Resume keeps the session's negotiated state and sequence state and
re-measures the channel; the responder's CONNECT-ACK carries a cumulative
ACK block, and the initiator's ordinary leading ACK block covers the other
direction (`session-resume-rules`). Either station may resume, the original
initiator winning a crossing (`resume-by-either-station`), and CONNECT-ACK
says whether it resumed (`resume-flag-echo`).

## Consequences

- **Frames describe their own blocks.** A receiver, or a monitoring
  station, maps every block from the header and control blocks of the
  frame alone. No state shared between the ends decides what a block is.
- **Stale acknowledgments are harmless.** A repeated or crossed ACK can
  only understate what the receiver holds, which costs a duplicate block.
  The ARQ needs no frame counter or alternating bit.
- **A frame with retransmissions costs 135 ms more.** At the 5–10% target
  block error rate most data frames carry a retransmission and so a block
  map: about 3.5% of a 4 s frame. Appendix A's net rates assume clean
  frames and do not include it.
- **The window caps the hold at 127 blocks**, well inside the receiver's
  memory, and limits how far ahead new blocks can run past a block that
  keeps failing. A wide link carries at most about 100 blocks per frame, so
  the window rarely binds.
- **Header flag bit 7 is used.** The header has no flag bits left; the
  three reserved bits are allocated by ADR 0011.
- **Follow-up.** Both implementations need the block map control block,
  the new ACK semantics and frame-level vectors (`vectors/link/`) for a
  retransmission frame, a stale ACK and a resume handshake.

## Alternatives Considered

- **Consecutive runs only.** Each frame resends everything from the lowest
  missing block onward. No wire change, but at a 7% block error rate on a
  40-block frame it resends about 25 blocks per frame.
- **Implicit mapping.** The blocks the last ACK reported missing come
  first, in an agreed order, then new blocks, with a small generation
  number in reserved bits telling the receiver which ACK the sender
  answered. No air time, but both ends must track identical state, any
  disagreement corrupts data silently, and a monitoring station cannot
  follow the stream.
- **A sequence number in every data block.** Robust, but it changes the
  126-byte block of B.4 and the published vectors, and costs 2 bytes in
  every block of every frame.
- **Keep the per-frame ACK and add an alternating frame bit.** It fixes
  stale ACKs but not the mapping of non-consecutive retransmissions, and
  resume would still need a different meaning for the same field.
