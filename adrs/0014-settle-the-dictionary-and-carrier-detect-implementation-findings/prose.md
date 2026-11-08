---
title: "Settle the dictionary and carrier-detect implementation findings"
date: 2026-11-08
authors: ["Jeremy Banker"]
---

## Context

Two more pieces of implementation work tested the specification, and each
kept a list of places where it left room.

The dictionary evaluation (PR #25, `dict_eval`) trained zstd dictionaries
on public-domain text and measured them on held-out 100-500 byte messages.
Three findings came out of it:

- **The zstd frame format is unspecified.** SPEC §12.1 defines encodings 1
  (zstd) and 2 (zstd with dictionary) and §12.2 says "zstd", but a zstd
  frame can carry a magic number, a content size, a checksum and a
  dictionary ID, and two implementations that choose differently cannot
  trust each other's bodies to be the same size, or in some cases to
  decode. On messages of 100-500 bytes the choice is worth bytes: with the
  64 kB dictionary the mean body was 87.4 bytes with library defaults,
  82.7 with no content size and no in-frame dictionary ID, and 78.7 with a
  magicless frame as well.
- **Training has no seed.** SPEC §12.2, §20.9 and ADR 0002
  `dictionary-registry` ask the registry for a "reproducible training
  command (zstd version, parameters, seed)". libzstd's trainer takes no
  seed. The evaluation got reproducibility from identical samples in
  identical order (checked by a test, and by separate runs producing
  identical files), with seeds only for the corpus split, chunk sizes,
  synthetic messages and sample order.
- **The roadmap's compression claims were estimates.** §12.2 says "2.5-4×
  on short text, 5-20× on structured forms once schema encoding exists",
  to be measured before v0.2. The measurements: natural held-out text
  (FEMA form text, 47 CFR 97 and NWS products from documents the
  dictionary never saw) compresses 1.9-3.1× by body size with 32-112 kB
  dictionaries on 100-500 byte messages (2.4-2.7× over the whole range,
  1.5× without a dictionary). Synthetic form messages, which share their
  templates with the training data, reached about 5× with a dictionary;
  that is an upper bound for layouts the dictionary has seen, and 5-20×
  is not shown by any measurement without schema encoding.

The `cairn-modem run` work (PR #24, merged) built carrier detect and the
runtime around it and ran two stations against each other in simulation.
Its ambiguities, besides those already settled by ADR 0013:

- **The quieting band** of §7.2 (2700-3100 Hz at 24 tones) sees the frame's
  own spectrum about 19 dB below its mean, so quieting stopped reading
  carrier during the peer's own frame unless the squelch noise was loud.
  2800-3200 Hz leaks about 5 dB less.
- **The preamble correlator's hold** was unspecified. The runtime holds
  it to the end of the frame the header describes.
- **The turn-holder check** (ADR 0010 `turn-holder-reply-check`) starts its
  50 ms window when quieting shows the peer's carrier has gone. With a
  closed squelch or no open-squelch noise, quieting is unarmed and there
  is no start event.
- **`HeaderFailed`.** §10.1 and ADR 0013 speak of "a header that fails"
  while awaiting the peer; the runtime sends the event for any failed
  header.
- **Control-block packing.** Appendix A.1 gives 12 symbols per control
  block, and frame arithmetic built on "12 per block" gives 36 symbols for
  three. The PHY packs three blocks into 35.
- **The §14.1 false-DCD row** says nothing about the noise it is measured
  in; the runtime saw no false assert in 80 minutes of stationary
  open-squelch noise, and a sudden level drop of 3 dB or more read as
  carrier about one time in four.
- **A receive guard after PTT release.** The runtime ignores its own audio
  path for 5 ms after unkeying.

## Decision

**One decision changes bits on the air and needs the project owner's
explicit agreement.** Everything else affects a single station's behavior
or the wording of the specification.

**The zstd frame (`zstd-frame-format`, on the air).** A body of encoding 1
or 2 is one standard zstd frame (RFC 8878). Senders set no content
checksum, omit the in-frame dictionary ID (the PDU carries it for encoding
2) and should omit the content size; the magic number stays. Receivers
accept any of those fields being present, require the magic number, and
bound the decompressed size. The rest of the PDU is unchanged. Saving: about
5 bytes per message against library defaults, from the measurement above.

**Dictionary training (`dictionary-registry-reproducible-training`).** The
registry records the zstd version, the parameters, and the exact ordered
sample set with its corpus seeds, in place of a "seed". This supersedes
only the wording of the training command in ADR 0002
`dictionary-registry`; signing, license, pull-request and held-out rules
are copied unchanged.

**Carrier detect (`carrier-detect-band-and-hold`, local behavior).** The
quieting band starts at least 100 Hz above the upper edge of the highest
active tone (centre + 50 Hz): default 2800-3200 Hz at 24 tones, 6000-7000
Hz at 56. The preamble correlator's raw output is held to the end of the
frame its header describes, or falls 500 ms after the header's last symbol
when the header fails. This supersedes ADR 0002 `carrier-detect`.

**Turn-holder check (`turn-holder-check-without-quieting`, local behavior).**
When quieting is unarmed and there is no COS, the 50 ms window starts when
the other detectors' raw outputs first read clear after the peer's frame.
This supersedes ADR 0010 `turn-holder-reply-check`; the rest of that
decision is copied unchanged.

**Interpretations and errata.** These record readings and corrections that
do not change a decision, as interpretations of the decisions they belong
to:

| ADR | Decision | Interpretation |
| --- | --- | --- |
| 0002 | `fixed-per-frame-costs` | `control-blocks-pack-by-the-run` (erratum): one block is 12 symbols, two are 24, three are 35; the control run is padded to whole symbols once |
| 0002 | `reference-dictionary-set` | `measured-compression-gains`: 1.9-3.1× on natural short text; 5-20× only with schema encoding (v1.x) |
| 0002 | `timing-parameters` | `receive-guard-after-ptt-release`: non-normative; about 5 ms is used |
| 0013 | `adaptation-rules-multi-rung` | `lost-header-is-any-failed-header`: any header that fails while awaiting the peer counts |
| 0013 | `v1-performance-targets-revised` | `false-dcd-test-conditions`: stationary open-squelch noise; level steps are a separate robustness case |

**Control-block arithmetic.** A control block is 512 coded bits; at MCS 0
on the 22 data tones of the base set, a symbol carries 44 bits, so a block
is 11.64 symbols. Control blocks, like the header and the data blocks, are
packed back to back into one run that ends at a symbol boundary
(ADR 0005 `bit-to-tone-order`). The run is therefore ⌈512 n ÷ 44⌉ symbols
for n blocks: 12 for one (135 ms), 24 for two, and 35 for three (393.75
ms), not 36. The §7.4 and A.2 figures count one ACK block, and a block map
with it (24 symbols, exactly 12 more); both stand. A frame carrying three
blocks has 23 fewer data-block symbols than one with only the ACK block, not
24. The 12-symbol figure assumes no masked tones among the 22: the control
run is masked like the data run, so with k masked base tones a block is
⌈512 n ÷ (2 × (22 − k))⌉ symbols in all.

## Consequences

- **Messages cost about 5 bytes less than with library defaults** and
  every implementation sends the same bytes for a given frame setting.
  Bodies stay decodable by standard zstd tools. The `dict_eval` tool already
  writes frames this way; a standard zstd binding needs its content-size
  and dictionary-ID writing turned off, and the checksum stays off (the
  default).
- **Registry entries become checkable.** A reviewer can retrain from the
  listed sample set and compare SHA-256, as the evaluation does.
- **The quieting band moves 100 Hz up at 24 tones.** A radio whose receive
  audio rolls off before 3200 Hz needs the band moved at setup, and the
  setup procedure already measures the roll-off; the quieting detector
  stays the receiver's only detector of non-Cairn carriers, so its band
  must stay clear of the frame's own spectrum.
- **A failed header holds the channel for 500 ms after its end** where an
  implementation might have released at once. That delays a station's own
  transmission after a false correlator confirmation, never a reply.
- **Appendix A.1 and the frame-time arithmetic are corrected** for
  frames with three control blocks (CONNECT, CONNECT-ACK with a dictionary
  block, DATA with ACK, map and ID blocks). Net throughput figures were not
  affected: they count the ACK block alone, or ACK and block map.
- **The implementations follow later.** `cairn_phy::dcd` and its README
  state the old quieting default and no failed-header hold; the PHY's
  control-block packing already gives 35 symbols. This ADR changes no code.
- **Follow-up.** Test vectors for a zstd body (compressed bytes for
  fixed input, with and without a dictionary), vectors for a three-block
  control run, and a bench measurement of the quieting band at 2800-3200
  Hz on three radio makes.

## Alternatives Considered

**The zstd frame, for the owner to choose** (the only on-air choice here):

| Option | Mean PDU saving vs library defaults (64 kB dictionary, 100-500 B) | Notes |
| --- | --- | --- |
| **A. Standard frame, no checksum, dictionary ID omitted, content size optional, magic present (recommended)** | about 5 bytes (100.4 to 95.7) | Any standard zstd decoder reads it; only the writer needs settings |
| B. A, with the magic number removed (magicless format) | about 9 bytes (100.4 to 91.7), 4 more than A | Standard tools cannot decode it; magicless is an advanced libzstd option (an experimental parameter that some bindings do not expose) |
| C. Library defaults (content size and dictionary ID in the frame) | 0 | Nothing to state, but bytes repeated on every message; the in-frame dictionary ID is zstd's own 32-bit ID, different from the PDU's, so two numbers name one dictionary |
| D. A, with the content size required | slightly less than A (0.6 byte measured without a dictionary) | Lets a receiver size its buffer up front, but a sender-declared size cannot be trusted until the CRC-32 passes, so the receiver needs a bound anyway |
| E. A, with the checksum on | 4 bytes less than A | Repeats the PDU's CRC-32, which already covers the decoded body |

A is recommended because it takes almost all of the available saving with
no change to what a standard decoder expects. B's extra 4 bytes is about
4% of a 100-byte PDU and 0.03 of a data block; it
is the choice to make if every byte matters more than being able to open a
body with `zstd -d`. A later version can add B as a new Encoding value
without disturbing A.

**Dictionary training.**

- **Pin a trainer seed.** libzstd has none; a replacement trainer would
  have to be specified in full.
- **Specify the trainer's thread count and algorithm instead of the
  samples.** Their output depends on the samples anyway, so the samples
  have to be listed.
- **Interpret the wording instead of superseding.** The decision's words
  ("seed") cannot be done as written; the registry would have kept a
  requirement no one could meet.

**Roadmap claims.** Keep 2.5-4× and 5-20× as targets. The measurement
shows 4× is above the natural-text range at every dictionary size, and
5-20× would hold only for schema encoding, which does not exist before
v1.x.

**Quieting band.**

- **Keep 2700-3100 Hz.** The frame's own leakage 19 dB down makes quieting
  unreliable during the peer's own frame; the preamble correlator covers
  the frame, but the turn-holder check relies on quieting for the
  carrier-gone event.
- **A fixed 2800-3200 Hz at every width.** Wrong at 56 tones, where the
  occupied band ends at 5900 Hz and the existing band is right.
- **A guard larger than 100 Hz.** Buys a few dB more rejection but moves
  the band toward the radio's receive roll-off.

**Correlator hold on a failed header.**

- **Release at once.** Safe against false confirmations, but a real frame
  with a damaged header would show a clear channel while the peer is
  still sending (open-squelch quieting is the only protection then).
- **Hold for the longest permitted frame (4 s).** Always covers the frame,
  but one false confirmation then blocks the channel for 4 s.
- **A shorter or longer fixed time.** 500 ms is the project's choice and
  can be tuned at v0.5 on air; it affects only a station's own deferral.

**Turn-holder check without quieting.**

- **Key at the end of the peer's last symbol.** Simplest, and skips the
  50 ms look at a third station, which the check exists for.
- **Fail closed (never key without quieting).** Safe, but a station on a
  closed squelch or a test bench could never reply.
- **Use the 100 ms-after-last-symbol deadline only.** It is a ceiling,
  not a start event.

**Control blocks.** Pad each control block to a whole symbol (36 for
three). It would keep A.1's figure, but costs 1 symbol per three blocks
and contradicts ADR 0005, which packs the run.
