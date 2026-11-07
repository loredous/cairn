---
title: "Specify the scrambler, interleaver and bit-to-tone mapping"
date: 2026-11-07
authors: ["Jeremy Banker"]
---

## Context

ADR 0002 adopted the properties of Cairn's bit mapping but not the mapping
itself. Coded bits are spread across all active tones and the symbols a
block occupies (`interleaving`), scrambled with a published PRBS seeded from
the block's position (`scrambling`), Gray-mapped at unit power
(`constellation-mapping`), and packed back to back with the last symbol
padded (`block-packing`). Decision `bit-mapping-adr-before-v0-2` required a
later ADR to fix the exact scrambler, the interleaver (defined for every
tone mask) and the Gray labeling with its bit-to-tone order, with test
vectors, before v0.2. Two implementations that differ in any of these
cannot decode each other, however closely they follow the rest of the
specification.

The Stage 1 simulation (`sim/`) had to choose all of them to run at all.
It also chose the other fixed sequences on air, S1, S2, the pilots and the
width probe, and found that the prefix must be the negated tail of the
symbol (`prefix-is-negated-tail`, an interpretation of `ofdm-parameters`).
`sim/findings.md` lists them as "still provisional". With them, every rung
decodes end to end at every width from 24 to 56 tones and reaches 1% block
error on every channel profile.

The interleaver was the one choice that needed evidence. SPEC §6.4 wants a
dead tone or a short burst to cost scattered bits rather than a run, under
any tone mask. The simulation uses the simplest permutation that might do
this, π(i) = a·i mod E with a ≈ 0.618·E, and `interleaver_eval` compared it
with a uniformly random permutation, the idealised reference (results in
`sim/results/interleaver.json`, summarised in `sim/findings.md`):

- **Structure.** For every data-tone count from 12 to 52, where one dead
  tone or one or two lost symbols land in the codeword, the linear
  permutation's worst case (LDPC parity checks that see two or more of the
  damaged bits) matches or beats the random one's.
- **Block error.** With one dead tone not yet masked, and with a 10 ms
  noise burst 20 dB above the signal, at MCS 2 and MCS 4 and at 24 and 56
  tones, the two agree within the Monte Carlo spread (3,000–6,000 blocks per
  point).

What a dead tone or a burst costs turned out to depend on the receiver
(how it smooths the channel estimate across a dead tone, whether it
discounts a burst) and, at the top rungs on wide links, on how few symbols
a block spans. It did not depend on the interleaver.

## Decision

The Stage 1 choices become the specification, written out so that a second
implementation reproduces the simulator bit for bit.

- **Waveform** (`symbol-waveform`). A symbol is
  A·Re{Σ X[k]·exp(j2π(3.5 + k)n/480)} over its prefix and useful part. That
  makes the prefix the negated tail of the useful symbol and fixes the
  direction of the quadrature axis. The interpretation
  `prefix-is-negated-tail` is now a decision. `ofdm-parameters` is not
  superseded, because its statement never said what the prefix contains.
- **Symbol content** (`frame-symbol-content`). One table gives the tones
  and the scale of each kind of symbol: preamble, header, probe, control
  and data. For mask handling it defers to ADR 0004 (`tone-mask-scope`,
  `masked-upper-pilots`, `masked-tone-content`). It also settles the one
  case ADR 0004 leaves implicit. A masked control symbol keeps the power of
  24 unit-power tones, with every active tone scaled by √(24 / active
  tones), the same rule ADR 0004 applies to data symbols at width W.
- **Fixed sequences** (`preamble-sequences`, `pilot-sequence`,
  `probe-sequence`). S1 is 24 phases in units of 2π/256. S2, the pilots and
  the probe are segments of one generator, the 3GPP Gold sequence, with
  seeds 5237, 0x0B1D and 0x0D0E. A pilot's value depends only on the symbol
  index and which of the four pilot tones it is on. A probe tone's value
  depends only on the probe symbol and the tone index, not on the sender's
  cap.
- **Scrambler** (`scrambler-sequence`). It uses the same Gold sequence,
  with c_init = 1 + position. The header has position 0, and the frame's
  blocks take positions 1, 2, … in transmission order, control blocks
  first. Padding uses position 255. It applies to the rate-matched bits,
  before interleaving.
- **Interleaver** (`block-interleaver`). Bit i of a block goes to position
  a·i mod E, where a is 0.618·E rounded half up and then raised to the
  nearest value coprime to E. A table lists a for every coded length in
  use. The permutation depends only on E, so it is defined for every mask
  and width without knowing either. It never spans blocks.
- **Labeling** (`constellation-labeling`). This is 3GPP TS 38.211 §5.1:
  even bits on I, odd bits on Q, the first bit of each axis giving the
  sign.
- **Bit-to-tone order** (`bit-to-tone-order`). The header, the control
  blocks and the data blocks each fill their own run of symbols, frequency
  first: point m goes to symbol ⌊m/D⌋ on the (m mod D)-th lowest data tone.
  Blocks follow each other with no gap, so a block usually starts
  mid-symbol, and each run is padded to a whole symbol. This supersedes
  `block-packing`. Its rule padded only the last symbol of the frame, but
  the header and control runs use different tones and a different rung
  from the data, so they are padded too. Appendix A.1 already counted it
  that way: 7 header symbols for 288 bits, 12 control symbols for 512.
- **Test vectors** (`bit-mapping-test-vectors`). `vectors/bitmap/` follows
  13 frames through every stage, from payload to the time-domain samples
  of three whole frames. The frames cover every rung at 24 tones, MCS 1 at
  30 tones, MCS 2, 4 and 6 at 56 tones, masked cases at 24, 30 and 56 tones
  (including a masked pilot 37 and mask bits above the width), and both
  prefixes. The vectors are normative. A simulation test regenerates them
  byte for byte and checks them against the rules above, which it
  implements independently of the simulator.

This fulfils `bit-mapping-adr-before-v0-2`. That decision is not superseded,
since its requirement stands and is now met. `interleaving`, `scrambling`
and `constellation-mapping` remain in force. The decisions here add the
exact rules beneath those properties and do not change them.

SPEC §6.4 also gains a statement of the burst limit. At MCS 4 and above on
wide links a block spans only about five symbols, so a 10 ms burst can take
two of them, and such links rely on ARQ for burst errors. A new §6.5 holds
two non-normative receiver notes.

## Consequences

- **Bit-exact interoperability is defined.** A second implementation can
  check every stage against the vectors, from the CRC through to the
  samples, and locate the first place it diverges.
- **Bursts at the top rungs are left to ARQ.** At MCS 4–6 on a 56-tone link
  a block spans 3–5 symbols, so a 10 ms burst can erase two of them, and
  Stage 1 measured about 35% block error at any SNR under such bursts. No
  permutation within a block can fix that. Interleaving across blocks could,
  but it would break per-block decoding and acknowledgement, which selective
  retransmission and planned HARQ depend on. Such links rely on ARQ.
- **Receiver quality matters more than the interleaver.** Two receiver
  techniques recorded in SPEC §6.5 as guidance made large differences in
  Stage 1. Dropping burst-hit symbols from pilot tracking and raising their
  noise variance took MCS 2 at 24 tones from 65% to 1% block error under a
  10 ms, +20 dB burst, with no change on clean channels. An outlier-robust
  channel smoother would avoid the roughly 2 dB that the 7-tone smoother
  loses at MCS 4 when one dead, not-yet-masked tone spreads into its
  neighbours. Neither changes what is transmitted.
- **The width probe does not need the sender's cap.** In the Stage 1
  simulation, tone k of probe symbol r took element r·W_p + k of the probe
  sequence, so its value depended on the cap W_p. This ADR indexes it as
  56·r + k instead, so a receiver can use the probe, and see where the
  probed tones stop, before it knows the cap. At a 56-tone cap the two
  are identical. The simulation was changed to match, and its 56-tone
  results are unaffected.
- **Control blocks have no vectors yet.** The Stage 1 simulation does not
  build control blocks, so the control rows of `frame-symbol-content` and
  the control positions of `scrambler-sequence` are specified but not yet
  covered by vectors. Stage 2 adds them when the reference implementation
  builds control blocks.
- **The SPEC no longer defers the bit mapping.** The deferral bullet in
  §6.4 is replaced by the rules themselves, and §6.1 now gives S1 and S2
  explicitly.

## Alternatives Considered

- **A random permutation per block length, published as a table.** It
  performs no better than the linear rule (`sim/results/interleaver.json`),
  needs a table per coded length, including any future HARQ segment
  lengths, and cannot be checked by eye.
- **A mask-aware interleaver** (for example a row-column interleaver sized
  to the active tones). The linear permutation is already defined for every
  mask, since the bit-to-tone order carries the mask, and measured as well
  as random under masks. A mask-aware design would add a dependency
  between the interleaver and the mask report with nothing measured to
  gain.
- **Interleaving across blocks.** It would spread a burst over more symbols
  at the top rungs, but a block could then be decoded only with its
  neighbours, which breaks per-block acknowledgement, selective
  retransmission and incremental-redundancy HARQ.
- **Time-first filling** (each point to the next symbol, then the next
  tone). It needs the block's symbol count up front, and blocks would no
  longer pack back to back across symbol boundaries without a rule for the
  partial columns. The linear permutation already spreads consecutive
  coded bits across symbols.
- **Packing control and data blocks into one run of symbols.** Control
  blocks use the base tones at MCS 0, and data blocks use the full width at
  their rung. Sharing a symbol would mix two tone sets and two
  constellations, a special case that saves at most one symbol per frame.
- **A different PRBS for scrambling** (for example the 802.11 7-bit
  scrambler). Its period of 127 bits is shorter than a block. The Gold
  sequence is already needed for S2, the pilots and the probe, so a single
  generator serves all of them.
