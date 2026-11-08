---
title: "Synchronize tone masks by ID and set the session width"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0004 applies the tone mask to control blocks as well as data blocks
(`tone-mask-scope`), and ADR 0002 sends a mask change "in the ACK with the
mask-present flag", applied by the sender "to its next frame"
(`tone-mask-updates`). The pre-implementation review of the link layer found
three problems with this:

- **A lost ACK desynchronizes the mask.** If the ACK carrying a new mask is
  lost, the sender keeps the old mask while the receiver decodes with the
  new one. The leading ACK block of every following frame then fails along
  with the data. The mask was only sent "when it changed", so the change
  might never arrive and the link would be lost.
- **The session width W was undefined.** Data tones (0 … W − 1) and the
  per-tone scale √(W / active tones) that QAM demapping needs both depend
  on W. ADR 0007's prose notes that the probe runs to the sender's cap, not
  the link width, and that both implementations assumed otherwise. The
  header does not carry W.
- **A probe wider than the data costs the data.** The frame's peak is set
  once per frame (−3 dBFS with a fixed per-tone level, SPEC §7.1). The
  broadcast-readiness work showed that a 56-tone probe in a 24-tone frame
  sets that peak and lowers every data and control tone by up to 3.7 dB,
  contradicting §5.4's "no penalty for having probed".

ADR 0002 and ADR 0004 also tag the mask machinery v1.0, while v0.5
sessions already exchange masks in CONNECT-ACK and ADR 0005 masks control
blocks. What a v0.5 station owes was unclear.

## Decision

Every tone mask carries a 2-bit **mask ID**. It travels in the ACK block's
reserved bits, and the sender echoes it in the header's reserved bits to
name the mask it applied. One change is outstanding at a time, so the
receiver needs only its current and pending masks. Setup masks have ID 0,
and setup frames are unmasked (`mask-id`). Every session ACK carries the
current mask; the drop and restore rules are unchanged
(`mask-in-every-session-ack`).

A station's session width is its width cap plus one when the peer can take
wide frames, and 24 otherwise (`session-width`). The width probe covers
exactly the transmit width, so in sessions and broadcasts it no longer sets
the frame's peak above the data (`width-probe-extent`, superseding ADR 0007
`width-probe-every-wide-frame` to say so).

The header and ACK layouts are restated with the newly allocated bits:
header flag bit 6 (first fragment, ADR 0009), flag bit 7 (block map, ADR
0008), and the Mask ID fields (`header-field-layout`,
`ack-control-block-layout`). A v0.5 station honours masks and the long-prefix
flag but need not produce either (`v0-5-mask-obligations`).

## Consequences

- **The mask can no longer desynchronize.** The header, which is never
  masked, names the mask in force; a lost ACK only delays a change.
- **Masks change at most once per round trip.** A receiver waits for the
  echo before issuing another change. Masks track slow audio-chain
  behaviour, so that is fast enough.
- **W is fixed for the session** at the configured cap (or 24). Dead upper
  tones are masked, not cut off, so a 56-tone cap through a 30-tone path
  scales the surviving tones as ADR 0004 already requires.
- **No probe penalty in sessions or broadcasts.** CONNECT and the link
  test still probe to the cap, but they carry no data blocks.
- **The header has one reserved bit left**, and the ACK block six.
- **Follow-up.** Both implementations take W and the probe extent from the
  session's B.3 blocks (`LinkState` in the Rust PHY), receive a mask ID from
  the header, and keep two masks per peer. Vectors are needed for a frame
  with a non-zero mask ID and for a mask change whose ACK is lost.

## Alternatives Considered

- **Send the mask in every ACK and trial-decode.** The receiver tries the
  first control block against its last few masks. No wire change, but it
  makes trial decoding normative, costs extra decodes on every failure,
  and can still guess wrong.
- **Leave control blocks unmasked.** The ACK would always decode, but it
  would supersede ADR 0004 and put control blocks on tones the receiver
  has said are dead.
- **Width from the mask** (highest usable tone + 1). It adapts W to the
  path, but W then changes with every mask change, and its scale would
  depend on the mask synchronization this ADR adds.
- **Carry W in the header.** It needs 6 bits, which the header does not
  have.
