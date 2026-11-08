---
title: "Frame PDUs in the block stream"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

The link layer carries session-layer PDUs (SPEC §12.1) as a byte stream
cut into 126-byte data blocks (B.4). The pre-implementation review of the
link layer found that the boundary between the two was not specified
tightly enough for two implementations to parse each other's streams:

- **Filler.** A block is fixed once it is sent, and a session's stream is
  continuous, so when a sender has less than a block to send, the unused
  bytes would be read as the next PDU's Length.
- **Byte order and CRC.** Appendix B's big-endian rule covers control
  blocks, not PDUs. The CRC-32 variant (polynomial, reflection, initial
  value, final XOR), its byte order, and the bit order of data-block bytes
  were not stated. The vectors use most significant bit first, but the SPEC
  never says so.
- **Broadcast fragmentation contradicts itself.** SPEC §12.1 says "in
  broadcast mode each frame's PDUs are complete within that frame", while
  §9.1 says "a PDU larger than one frame is split across frames". A
  receiver that joins in the middle cannot tell a first fragment from a
  later one, so "discard on any gap" could not be applied.
- **Stream interleaving contradicts packing.** SPEC §12.4 interleaves
  streams "at block boundaries", but PDUs "may span blocks" and carry no
  per-block stream tag, so a receiver could not parse a stream switched in
  the middle of a PDU.

## Decision

A Length of 0x0000 marks the rest of a block as filler (`pdu-filler`), and a
Length field never straddles a block boundary (`pdu-length-within-block`).
PDU fields are big-endian (`pdu-byte-order`), the CRC-32 is CRC-32/ISO-HDLC
sent most significant byte first (`pdu-crc32`), and data-block bytes go
first byte first, most significant bit first (`data-block-byte-order`).

A broadcast message larger than one frame is split into slices, each sent
as one complete PDU alone in its frame, on consecutive frame counters, with
the more-fragments flag on all but the last (`broadcast-message-fragments`).
Header flag bit 6 is set on every BEACON and UI frame that starts a
message, including a message that fits in one frame, so every broadcast
frame's role is visible in its header (`first-fragment-flag`).

Senders switch streams only between PDUs, highest priority first; mid-PDU
interleaving waits for the stream-multiplexing ADR
(`stream-interleaving-at-pdu-boundaries`).

## Consequences

- **Every parser agrees on where PDUs start**, in a session stream and in a
  broadcast frame, including after filler and across resumes.
- **The CRC-32 is available everywhere**: zlib, Python's `zlib.crc32`, the
  Rust `crc` crate's `CRC_32_ISO_HDLC`.
- **Broadcast frames stay self-contained.** A lost frame loses its
  message, never the next one; a late listener ignores continuation frames
  until the next message starts.
- **Header flag bit 6 is set on ordinary broadcasts.** Existing header
  vectors keep their encodings; vectors that describe complete BEACON or UI
  frames should be regenerated with bit 6 set.
- **Head-of-line blocking.** A short high-priority PDU can wait behind a
  long PDU already started. Bulk senders keep this short by cutting large
  objects into FILE-CHUNK PDUs; true mid-PDU multiplexing is the v1.x
  stream feature.
- **Follow-up.** PDU-packing vectors (PDUs to block payloads, with filler
  and CRC-32) and a fragmented UI message vector.

## Alternatives Considered

- **A dedicated PAD PDU type.** It works, but it spends 5 or more bytes of
  header where a zero Length already says enough, and it cannot fill a
  single leftover byte.
- **Hold data until a block fills.** Impossible at the end of a message,
  and it adds latency to every short exchange.
- **Fragments as pieces of one PDU spanning frames.** It matches the old
  §9.1 wording, but breaks §12.1's self-contained broadcast frames: a lost
  middle frame would leave a PDU whose CRC-32 and decompression cannot be
  checked until the end.
- **Mark only the first fragment, or require the previous frame.** The
  first leaves a last fragment looking like a complete message; the second
  makes a listener lose any fragmented message after missing an unrelated
  frame.
- **A per-block stream tag now.** It would allow true multiplexing, but it
  changes the 126-byte block of B.4 for a v1.x feature.
