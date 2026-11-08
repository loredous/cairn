---
title: "Bound decoded zstd bodies at the raw PDU limit"
date: 2026-11-08
authors: ["Jeremy Banker"]
---

## Context

ADR 0014 `zstd-frame-format` made PDU bodies of encodings 1 and 2 one
standard zstd frame, and told senders to omit the frame's content size. A
receiver then cannot know how large a body will decode before it decodes
it, so the decision also says a receiver MUST bound the decompressed size
and discard a body that exceeds the bound. It does not say what the bound
is.

Implementing the encodings (PR #37) exposed the gap. A receiver that picks a
small bound discards bodies another implementation sends in good faith; one
that picks none can be made to allocate without limit by a few hundred bytes
of compressed input. Interoperability needs one number that senders respect
and receivers honor.

The same work found one more place the specification leaves to the reader:
which dictionary IDs a CONNECT-ACK lists. SPEC §12.2 says a session
negotiates the intersection of both stations' lists, and §8.4 describes the
CONNECT-ACK's block as carrying the accepted parameters, but the
connect-control-block decision (ADR 0002) does not say whether the responder
lists its own dictionaries or only the shared ones.

## Decision

**The bound (`zstd-decoded-body-bound`).** A zstd body decodes to at most
65,528 bytes, the largest body a raw PDU can carry: the 16-bit Length
counts at most 65,535 bytes, of which the Type, Stream and Encoding bytes
and the CRC-32 take 7. Senders must not exceed it and receivers must accept
anything up to it, stopping as soon as decoding would go past it.

**The CONNECT-ACK list (an interpretation of ADR 0002
`connect-control-block`).** The responder lists the dictionaries both
stations have, which are the accepted parameters. The initiator uses the
intersection of its own list and whatever the CONNECT-ACK lists, so a
responder that lists all of its dictionaries reaches the same agreed set.
This is recorded in ADR 0002's interpretations; it changes nothing on the
air.

## Consequences

- Every implementation needs at most a 64 kB buffer per decoded body, the
  same as for a raw PDU, and a malicious or broken frame costs at most that.
- Senders compress MESSAGE PDUs only up to the raw limit, which they could
  not exceed anyway. FILE-CHUNK sizes are chosen by the sender and are far
  smaller in practice.
- The reference implementation in PR #37 already uses this bound
  (`compress::DECODED_BODY_MAX`) and the shared-list CONNECT-ACK.
- SPEC §12.2 states the number.

## Alternatives Considered

- **A smaller bound, such as 4 kB, to suit small embedded receivers.** It
  would forbid compressing large MESSAGE PDUs that are legal raw, and 64 kB
  is affordable on any platform that runs an OFDM modem.
- **Require senders to include the content size.** A receiver could then
  refuse early, but it still could not trust the declared size until the
  CRC-32 passed, so it would need the bound anyway, and ADR 0014 chose to
  omit the field to save bytes.
- **Negotiate the bound in CONNECT.** It would need a field in the B.3
  block and would not cover broadcasts, for no gain over a fixed number.
