---
title: "Adopt the Cairn v0.1 protocol specification"
date: 2026-11-05
authors: ["Jeremy Banker"]
---

## Context

Packet radio on VHF/UHF FM still mostly runs Bell 202 AFSK/AX.25 at 1200 bps
or G3RUH at 9600 bps. VARA FM is far faster, but it is closed. Cairn is an
open, adaptive data modem and link protocol for FM amateur voice channels. It
aims at roughly 5–12 kbps through a microphone/speaker path and 15–28 kbps
through a 9600-baud data port, using unmodified radios and ordinary sound
interfaces (SPEC §1).

The operating environment drives the design (SPEC §3):

- **The FM audio channel.** It is a fixed filter with a cliff. There is no
  frequency offset, so mistuning becomes a DC shift. Dispersion is short
  (1–2 ms). The detector collapses below about 10 dB carrier-to-noise, and
  above that threshold the audio SNR sits in a 12–40 dB window. The radio's
  limiter clips peaks.
- **Turnaround.** PTT-to-RF and receive recovery take 50–300 ms per
  transmission, so fixed costs have to be amortized over long frames.
- **Audio-path width.** Usable width ranges from 24 tones on a mic path to 56
  tones through a flat port on a 25 kHz channel, and often differs between
  the two directions.
- **Shared channels.** Channels carry legacy packet stations and voice
  repeaters, so the mode has to be a good neighbor.
- **Regulation (US Part 97).** Codes must not obscure meaning
  (§97.113(a)(4), §97.309(b)), identification is required (§97.119), and
  symbol-rate and bandwidth limits apply (§97.307(f)). Together these rule
  out encryption and require every decoder artifact to be published.

Two more constraints apply. The modem is software that runs on PCs and the
Raspberry Pi (a microcontroller TNC is a non-goal for v1, but stays possible).
It must also present a KISS TNC so that existing AX.25 software works
unchanged. Digital voice, HF/SSB, encryption, and relaying beyond simple hooks
are non-goals for v1.

## Decision

We adopt `docs/SPEC.md`, draft v0.1 as revised 2026-11-05, as the Cairn
protocol specification. Its normative content is SPEC Sections 1–16 and
Appendices A–C, including the review additions (§17–19) and the decisions
record (§20) folded into them. Each normative rule is recorded as a separate
decision in `decisions.yaml`, grouped by `category`. Where a change record or
the decisions record amends earlier text, the decision records the final,
resolved rule. Where the specification contradicted itself or left a gap,
the decision records the resolution of 2026-11-06 (see Resolutions below).
Each decision cites the SPEC sections it comes from. Acceptance of this ADR
also confirms the assumed defaults of SPEC §2
(`architectural-defaults-confirmed`), which is the v0.1 exit criterion. Each
area in summary:

- **Regulatory (§3.3, §20.7).** Nothing is encrypted. Every header carries
  the source callsign, and an ASCII ID control block is sent in CONNECT,
  DISCONNECT, every BEACON, and at least every 10 minutes in a session.
- **Physical layer (§5–6).** OFDM uses 100 Hz tones, 10 ms symbols and a
  1.25 ms cyclic prefix. The 2.5 ms prefix is used in repeater mode or when
  the receiver measures excess dispersion and asks for it, and receivers
  detect either prefix without configuration. Width is negotiated per
  direction from 24 to 56 tones, within a cap set by the configured channel
  width. The preamble, header and control blocks always use the 24 base
  tones at MCS 0, so every frame is self-describing. The preamble is 45 ms
  (S1 twice, then S2) with a performance requirement for detection. A
  seven-rung ladder runs from QPSK 1/2 to 256-QAM 3/4, with no BPSK rung.
  All three block codes are lifted, shortened 5G NR LDPC codes, with a
  CRC-16/CCITT-FALSE in every block. Decoders are free to use any algorithm
  that meets the conformance and performance targets.
- **Levels, carrier detect and timing (§7).** Clip-and-filter holds PAPR to
  7 dB or less, with peaks at −3 dBFS. Carrier detect is the OR of quieting,
  preamble, AFSK and COS detectors. Timing defaults are TXDelay 150 ms and a
  maximum frame time of 4 s, which is also a hard ceiling.
- **Link layer (§8–9).** A frame has a 144-bit header and up to 128 blocks.
  Addresses are 40-bit packed callsigns, with a hash for callsigns of seven
  to ten characters. Broadcast mode has fragmentation, optional
  soft-combined repeats and v1.x digipeating. Sessions are a strict
  ping-pong with one frame in flight. Every frame in a session leads with
  the ACK control block. ARQ is selective repeat on a 128-bit block bitmap,
  sessions resume within 10 minutes, and a two-transmission PROBE/SOUND
  link test measures both directions.
- **Link adaptation (§10).** The receiver picks the rung, stepping down at
  once and up deliberately. Frame length depends on the rung and on the
  lower of the two stations' maximum frame times. Tone masks are trimmed
  and restored by re-probing.
- **Channel access (§11).** New transmissions use p-persistent CSMA. A
  session turn-holder replies after a 50 ms DCD check and opens a yield
  window every 30 s. When legacy stations are active, frames shrink to 2 s
  and p is halved, and there is no courtesy preamble. Repeater mode has
  mandatory etiquette and is required on both ends of a session through a
  repeater.
- **Session layer (§12).** PDU framing uses streams and priorities. zstd is
  used with published, never-withdrawn CC0 dictionaries under a two-signer
  registry, trained only on public-domain or CC0 material. Ed25519
  signatures are optional. Delivery is in order and exactly once per stream.
- **Host interface and audio level (§13).** The modem provides KISS on 8001,
  a JSON API on 8002 and `cairn.sock`, and PTT by CAT, serial, GPIO or CM108
  (no VOX for sessions). It sets receive gain automatically, calibrates drive
  with a peer using stepped tones, and trims drive in session from the far
  end's distortion report.
- **Performance, conformance, versioning and governance (§14–15).** The
  decisions adopt the v1.0 targets and the definition of conformance. The
  on-air version changes only on incompatible change, reserved fields are
  zero, unknown control blocks are skipped, and identifiers held for planned
  v2 features are used for nothing else. Licensing is CC BY 4.0 for the
  specification, MIT or Apache-2.0 for the code, and CC0 for dictionaries.
  All artifacts are published and archived off the repository host, and an
  on-air change needs two implementations.

`interpretations.yaml` records the clarifications the specification itself
makes. Where a decision states an outcome that any implementation may meet
by its own method, an interpretation also records how the reference
implementation meets it ("How does the reference implementation meet
this?"). These interpretations add no requirements.

### Milestone tags

Every decision carries exactly one milestone tag: `v0.1`, `v0.2`, `v0.5`,
`v1.0`, `v1.x` or `v2`. The tag is the earliest milestone in the SPEC §16.2
roadmap that names the feature. Anything the roadmap does not name, such as
the core waveform parameters, the wire formats, governance, licensing and
process, is tagged `v0.1`. The handbook can be filtered by these tags to see
what each milestone has to deliver.

Decisions tagged `v0.2` cover what the roadmap says Stage 1 simulation
fixes: the NR matrices, the S1/S2 preambles, the rung thresholds, and the
bit-exact scrambler, interleaver and constellation labeling, with their test
vectors. They hold nominal values, or defer details to a later ADR, and are
expected to be superseded after Stage 1 simulation. The single `v2` decision,
`reserved-identifiers`, holds wire identifiers for the v2 designs.

### Resolutions

Drafting this ADR turned up conflicts, gaps and drafting calls in the
specification. Each was settled on 2026-11-06 as follows, and
`docs/SPEC.md` is corrected to match:

1. **Dictionary license.** CC0 only. §15's "CC0 or CC BY" was the outlier
   (`dictionary-registry`).
2. **Link test.** Two transmissions, under 3 s of air time, as in §9.6. §18's
   "four" was wrong (`link-test`).
3. **Repeater mode.** Receivers need no configuration, because they detect
   the prefix blind. Sessions through a repeater still need repeater mode on
   both stations, because both transmit, and a mismatch fails. That is the
   real reason for "set on both ends"
   (`repeater-mode-required-for-sessions`).
4. **Probe position.** The probe follows the header in every frame type
   whose flag bit 4 is set (§8.1). CONNECT is the header, the width probe,
   the B.3 block, an ID control block and an optional second
   dictionary-ID block (`width-probe`, `session-establishment`).
5. **Long callsigns.** Code 63 in position one, a 30-bit hash and the SSID.
   The hash is the first 30 bits of SHA-256 over the callsign text as
   carried in the ID control block, with all ones replaced by all ones minus
   one. The full text, at most ten characters, travels in the ID control
   block (`long-callsign-hash`, `long-callsign-hash-function`).
6. **Maximum frame time.** 4.0 s is a hard ceiling, and the configurable
   range is 0.5–4.0 s. Each station advertises its maximum in B.3, and
   session frames never exceed the lower of the two or the per-rung limit
   (`timing-parameters`, `max-frame-on-air-time`,
   `max-frame-time-negotiation`).
7. **Long prefix.** The receiver measures dispersion from the probe's
   per-tone phase and requests the long prefix with ACK flag bit 3, for
   that direction only. Repeater mode always uses it
   (`long-prefix-request`, which replaces a rule tied to Stage 2 group-delay
   logging).
8. **Scrambler, interleaver and CRC.** The §6.4 properties stay normative.
   The exact PRBS, the interleaver for every tone mask, and the
   constellation labeling and bit-to-tone order go to a later ADR with test
   vectors before v0.2 (`bit-mapping-adr-before-v0-2`). The CRC is pinned
   now as CRC-16/CCITT-FALSE (`crc-16-ccitt-false`).
9. **Header counts.** The data block count is 0 when flag bit 1 is clear.
   The control block count is authoritative, and a header whose flag bit 0
   disagrees with it is discarded (`data-block-count-encoding`,
   `control-block-count-authoritative`).
10. **Control types.** Types 2 and 7–15 are reserved, and receivers skip
    unknown types (`reserved-control-types`, `skip-unknown-control-types`).
11. **ACK frames and CAL-REPORT.** An ACK frame is the ACK control block
    plus optional other control blocks, never data. This also lets the ID
    block ride on an ACK. In a session, CAL-REPORT rides in the peer's next
    ACK or DATA frame. Outside a session it goes in a UI frame addressed to
    the initiator (`ack-frame-contents`, `cal-report-carriage`).
12. **Session acknowledgment.** Every frame sent in a session after setup
    (DATA, ACK, SOUND, DISCONNECT, CAL-REQUEST) leads with the ACK control
    block (`session-frames-lead-with-ack`).
13. **Dictionary corpora.** Training material must be public domain, CC0 or
    under a rights-holder CC0 waiver. Each source is recorded in
    `REGISTRY.md` and reviewed in the dictionary's pull request. Captured
    traffic is limited to material meant for redistribution
    (`dictionary-corpus-licensing`, `dictionary-corpora-public`).
14. **Future features.** The optional (MAY) decisions stay. HARQ and
    predistortion leave the decision record and become planned work (see
    Consequences). Their wire identifiers are held by
    `reserved-identifiers`.
15. **Implementation methods.** Rules that described the reference
    implementation's method now state the outcome every implementation must
    reach, and the method moves to an interpretation. This applies to
    decoding (`decoder-performance`), prefix detection
    (`blind-prefix-detection`) and legacy-mode reception, which becomes
    optional (`multi-mode-receiver`).
16. **§2 defaults and reference-implementation features.** The §2 defaults
    are confirmed (`architectural-defaults-confirmed`). The bundled
    reference-implementation feature table was removed. Its heard list,
    diagnostic capture, unattended operation, guided setup and rig profile
    library are recorded as interpretations on `native-api`,
    `conformance-suite`, `station-identification`, `calibrate-command` and
    `timing-parameters`. The UI-only features (link dashboard, transfer
    progress, etiquette status, coverage maps, CAT channel presets and the
    chat client) are not decisions and stay in SPEC §13.6.
17. **v2 contingencies.** One decision tagged `v2` holds the identifiers
    reserved for planned features (`reserved-identifiers`). The rate-1/3
    header contingency is a Consequence (below).

A second pass over the applied resolutions settled five more points:

18. **Header flag bit 3** is a live v0.1 session-frame flag, set on every
    frame sent as part of a session, so that SOUND and CAL-REQUEST can be
    read without tracking session state (`header-fields`). Only capability
    bit 4 is held for short session headers.
19. **Capability bit 3** is held for schema encoding, alongside content
    encoding 3 (`reserved-identifiers`).
20. **Blind prefix detection** arrives with the first real-time receiver
    (v0.5), not v1.x, so no conforming v1.0 decoder can be broken by a v1.x
    station sending the long prefix. Repeater mode and the long-prefix
    request stay v1.x.
21. **Legacy-active policy** triggers on the AFSK detector that every
    implementation has, or on a decoded legacy frame, instead of on the
    heard list (`legacy-active-policy`).
22. **Native API.** The full command set is mandatory; `capture` and
    `calibrate` may answer a defined `unsupported` error (`native-api`). The
    conformance captures come from the reference implementation, and other
    implementations only have to decode them.

## Consequences

- Implementers get one citable rule set. Code, tests and test vectors can
  reference `0002-adopt-the-cairn-v0-1-protocol-specification/<slug>`, and
  the milestone tags show what each release has to deliver.
- Many parameter values are nominal, and Stage 1 simulation and Stage 2
  bench work (SPEC §14) are meant to confirm them. Any number that moves will
  need a superseding ADR, since this `decisions.yaml` is frozen once merged.
  The `v0.2`-tagged decisions are expected to be superseded after Stage 1.
  The measurement steps that are already expected are:
  - S1 chosen by PAPR search, with S1 and S2 published as test vectors at
    v0.2 (§20.3).
  - Rung thresholds confirmed within 3 dB, and the Stage 2 margin set
    (§10.1, §14.2).
  - The exact scrambler, interleaver and constellation labeling, in their
    own ADR before v0.2 (`bit-mapping-adr-before-v0-2`).
  - The level-setting values confirmed on the bench (§7.1).
  - The calibrated drive margin, which moves from 3 dB to 1.5 dB below the
    knee if predistortion delivers (§20.6).
  - Dictionary compression gains measured (§20.9).
- **The rate-1/3 header contingency** (432 coded bits, 10 symbols, +34 ms
  per frame; §6.3, §20.4) is not in force. Moving the header code is an
  incompatible on-air change (§15), so whether to trigger it must be
  settled by Stage 2, before v1.0. If Stage 2 shows header losses above the
  FM threshold, a superseding ADR adopts it.
- The lifted parity-check matrices, rate-matching tables and known-answer
  vectors are deliverables at v0.2. Until they exist, `ldpc-codes` cannot be
  implemented bit-exactly.
- **Planned work with its own future ADRs.** These designs are described in
  the specification but carry no decisions here:
  - Incremental-redundancy HARQ (§9.4). The rate-compatible buffer
    (`rate-compatible-buffer`) keeps it possible, and capability bit 1 and
    ACK flag bit 1 are held for it.
  - Audio-chain predistortion from a 16-step calibration sweep (§13.5,
    v1.x experiment). The sweep does not fit the current calibration
    blocks: CAL-REPORT carries eight steps and the CAL-REQUEST step count
    is 4 bits. The predistortion ADR has to extend B.5.
  - Rateless broadcast on RaptorQ, with LT codes as the fallback, subject
    to a patent check at design time (§20.12).
  - The full-duplex session mode (§20.13) and the polled gateway mode
    (§20.14), whose v2 parameters are recorded in the specification.
  - Other v2 work: schema-encoded forms, short session headers, session
    relay with via addressing, per-tone bit loading, and propagation
    prediction (§16.2). The short-session-headers ADR must say how a
    monitor that missed the CONNECT can parse an abbreviated header, likely
    with a header version bump.
- **Candidates noted during resolution.** A responder could mirror the
  initiator's repeater mode automatically, signaled by a reserved
  capability bit. This is a v1.x candidate and would remove the mismatch
  failure. Frames longer than 4.0 s on dedicated channels could come in a
  future ADR.
- **Dictionaries.** ARRL bulletins are dropped from the first dictionary
  corpus (§20.9) unless the ARRL gives permission, because they do not meet
  `dictionary-corpus-licensing`.
- Some items are still undecided or outside this ADR:
  - The working name Cairn is still pending a collision check, with
    Timberline and Talus as alternates (§20.1).
  - The ARRL regulatory desk has not yet confirmed the ID approach (§20.7),
    and the Part 97 symbol-rate status must be re-checked at each revision
    (§3.3).
- Roadmap scope lines (§16.2) and the implementation plan (§21) guide the
  order of work but are not decisions, apart from the milestone tags.
  Reference-implementation conveniences without protocol meaning remain in
  SPEC §13.6.
- Some decisions add costs, for example two signers for every dictionary
  release, two code bases for every on-air change, and mandatory repeater
  etiquette. These slow change and usage on purpose.

## Alternatives Considered

These come from the specification's assumed-decisions table (§2), its "why
these values" notes (§5.3), and the options it rejected (§19, §20).

- **Two fixed width profiles (NARROW/WIDE)** instead of per-direction
  negotiated width. Rejected in §17.3: radios between the two profiles would
  get no intermediate width, and a mismatched pair would run at the narrower
  width both ways.
- **Broadcast-first (APRS-like) design.** Rejected in favor of connected ARQ
  sessions first.
- **A microcontroller TNC platform.** It would shrink the LDPC blocks and
  tone count, and drop 256-QAM, multi-mode receive and iterative decoding.
- **A BPSK 1/2 rung below QPSK 1/2.** It would roughly double header time,
  and it is nearly useless at the FM cliff (§20.4).
- **50 Hz tone spacing (20 ms symbols, FreeDV-style).** It would tolerate
  more dispersion but double the fixed per-frame overhead. The cyclic prefix
  is the dispersion knob instead (§20.5).
- **Reed–Solomon FEC (IL2P-style), codec2's (2048, 1024) LDPC, or 802.11n
  LDPC.** None of these allows rate-compatible incremental-redundancy HARQ
  with a fixed-size block (§20.2). Longer LDPC blocks (8k–16k bits) were also
  rejected: they gain under 0.5 dB and give worse HARQ granularity (§19).
- **A sliding window of frames.** On a half-duplex ping-pong link it gains
  little for the extra state and larger bitmaps.
- **Simplex only (no repeater mode)**, and **operator-set levels by meter
  only.** Both were rejected in favor of optional repeater mode and
  modem-managed levels. Metering remains the fallback.
- **No compression, or no authentication, in v1.** Rejected in favor of
  optional zstd dictionaries and Ed25519 signatures.
- **Copyleft licensing for the reference code.** Rejected in favor of a
  permissive license.
- **A 1200-baud HDLC courtesy preamble.** It would protect at most the first
  300 ms of a frame (§20.8).
- **Co-channel interference cancellation, machine-learned detectors and
  equalizers, and overhearing caches or cooperative relay in the modem.**
  All rejected in §19.
