# Open FM Data Modem — Specification Draft v0.1

Working name: Cairn · Draft v0.1, 2026-10-29, revised 2026-11-05 · Jeremy Banker

Source of record: the Claude Docs copy of this specification; this file is its Markdown export. Sections 1-16 and Appendices A-C are normative; Sections 17-19 are change records; Section 20 is the decisions record; Section 21 is the implementation plan. The Section 2 defaults are confirmed by ADR 0002 (`adrs/0002-adopt-the-cairn-v0-1-protocol-specification`); parameter values remain nominal until Stage 1 simulation.

## 1. Purpose and scope

This specification defines an open, adaptive data modem and link protocol for FM amateur radio voice channels. It is intended to replace Bell 202 AFSK/AX.25 (1200 bps) and G3RUH (9600 bps) packet with an interoperable mode in the throughput class of VARA FM, using unmodified radios and ordinary sound interfaces.

**Goals**

- Throughput of roughly 5–12 kbps through a microphone/speaker path and 15–28 kbps through a 9600-baud data port, adapting automatically to conditions.
- Works through the mic and speaker of any FM radio, including handhelds; faster through a flat (discriminator) data port.
- Fully open: waveform, framing, codes, test vectors and compression dictionaries are published; the reference implementation carries a permissive license; two independent interoperable implementations are the bar for v1.0.
- Backward compatible at the host: presents a KISS TNC to existing AX.25 software, plus a native API for new applications. The reference modem also receives and transmits 1200 AFSK, G3RUH 9600 and IL2P, so one installation talks to every station on the channel (13.7).
- A good neighbor on shared channels: carrier sense, detection of legacy 1200-baud packet, and a bounded channel hold time.
- Cross-platform software modem (Linux, macOS, Windows, Raspberry Pi) with any sound interface; no special hardware. An SDR receive front end is supported, not required.

**Non-goals for v1**

- Digital voice.
- HF/SSB operation (the waveform assumes an FM audio channel: no frequency offset, no multipath).
- Encryption of any kind. Authentication is supported; obscuring content is not.
- Relaying or digipeating beyond a simple store-and-forward hook (deferred to v2).
- A microcontroller TNC implementation. Parameters are chosen so one remains possible later.

**Design principles**

1. The FM audio channel is a fixed filter with a cliff: design for the SNR window above the FM threshold, not for the floor.
2. Every fixed cost (PTT delay, preamble, header, turnaround) is paid per transmission, so transmissions are long and few.
3. The receiver knows the channel; the receiver drives adaptation.
4. Every frame is self-describing at the base rate: a receiver that can decode the header can always learn how to decode the payload.
5. The block, not the frame, is the unit of error recovery.
6. Nothing in the protocol depends on a single vendor, maintainer, or closed artifact.

**Status.** Draft v0.1, revised 2026-11-05 with the decisions of Section 20 and the implementation plan of Section 21. Working name: Cairn (20.1). Parameter values are nominal and are to be confirmed by simulation (Section 14) before v0.2.

## 2. Assumed decisions

The draft takes the defaults below where no decision has been made yet. These defaults are confirmed by the acceptance of ADR 0002 (`adrs/0002-adopt-the-cairn-v0-1-protocol-specification`). Each one would change the design if reversed. Parameter values (rung thresholds, timings, margins, sizes) remain nominal and are expected to be superseded where Stage 1 simulation (v0.2) says so.

| Decision | Default assumed | Alternative | What changes if reversed |
| --- | --- | --- | --- |
| Link width | Negotiated per direction, 24–56 tones, within a cap set by the configured channel width; 24-tone links first | Two fixed profiles (NARROW / WIDE) | Width probe and cap drop out; a mismatched pair runs at the narrower width both ways; intermediate radios get no intermediate width |
| Primary use | Connected ARQ sessions first, broadcast second | Broadcast first (APRS-like) | ARQ and link adaptation become v2; broadcast mode gets a per-frame adaptation policy instead |
| Platform | Software modem on PC/Raspberry Pi | Microcontroller TNC | LDPC block size and OFDM tone count shrink; 256-QAM rung dropped; multi-mode and SDR receive, iterative decoding and the raw-audio buffer drop out |
| AX.25 compatibility | Yes, via KISS; AX.25 frames tunneled as broadcast PDUs | Native API only | KISS interface, AX.25 PDU type and legacy detection priority drop |
| Legacy mode reception | Reference modem demodulates 1200 AFSK, G3RUH and IL2P alongside this mode and answers each station in its own mode (13.7) | This mode only | Per-destination mode policy and the legacy KISS path drop; coexistence relies on carrier sense alone |
| Lowest rate | QPSK rate 1/2 (no BPSK rung) | Add BPSK 1/2 below it | Header time per frame roughly doubles; \~3 dB more margin at the cliff |
| Tone spacing | 100 Hz (10 ms symbols) | 50 Hz (20 ms symbols, FreeDV-style) | Halves symbol overhead granularity; 50 Hz tolerates more time dispersion, costs latency |
| Error correction | Rate-compatible LDPC, 1024-bit blocks | Reed–Solomon (IL2P-style) | HARQ with incremental redundancy becomes impractical; simpler decoder |
| Session ARQ | One frame in flight, selective repeat at block level; sessions resume after link loss | Sliding window of frames | Window adds sequence state and bigger ACK bitmaps; little gain on a half-duplex ping-pong link |
| Repeater operation | Optional operator-selected mode (11.5), v1.x; receivers detect the prefix length blind | Simplex only | Repeater access delay, 2.5 ms prefix variant and modulation DCD drop out |
| Host level control | Modem sets receive gain and calibrates transmit drive where the OS allows (13.5) | Operator sets levels by meter | Calibration frames and the automatic response to the distortion report drop out |
| Compression | Session-layer option: zstd with published dictionaries, fetchable over the air | None in v1 | Section 12 content-encoding negotiation drops out |
| Authentication | Optional Ed25519 signatures on PDUs | None | Section 12 signature PDU and key handling drop out |
| Licensing | Spec CC BY 4.0; reference code Apache-2.0 | Copyleft | Changes who can embed it; no technical effect |

## 3. Operating environment

The modem runs through the audio path of a conventional FM transceiver on 2 m or 70 cm, in a 12.5 kHz or 25 kHz channel. Everything below follows from the properties of that path.

### 3.1 Radio audio paths

| Path | Typical measured width | Usable band | Characteristics |
| --- | --- | --- | --- |
| Microphone in / speaker out, or 1200-baud data port | 24 tones | 300–2700 Hz | TX pre-emphasis and RX de-emphasis (net roughly flat), voice band-pass filters, limiter and sometimes compression that cannot be disabled; 12.5 or 25 kHz channel |
| 9600-baud (flat / discriminator) data port, 12.5 kHz channel | up to 30 tones | 300–3300 Hz | Flat TX input and RX discriminator output; the channel width caps the audio band (Carson's rule) |
| 9600-baud (flat / discriminator) data port, 25 kHz channel | up to 56 tones | 300–5900 Hz | Flat; demodulated noise rises with frequency, so upper tones have lower SNR; the IF filter sets the practical top |
| SDR receive front end (13.7), any transceiver for transmit | up to 56 tones on receive | 300–5900 Hz | Flat software demodulation on receive; the transmit width is still set by the transceiver's path, so the link is wide one way and as narrow as the radio the other |
| Voice repeater (repeater mode, 11.5) | 24 tones | 300–2700 Hz | Two cascaded audio chains plus controller filters; access delay, hang time, courtesy tone |

No particular audio path is assumed. Width is measured per direction at connect (Section 5.4) and trimmed during the session (10.3); the operator configures only the channel width, which caps the tones the modem will ever transmit.

### 3.2 FM channel characteristics the design relies on

- **No frequency offset.** FM demodulation returns baseband audio exactly; radio mistuning becomes a DC shift (blocked by AC coupling), not a tone shift. The modem therefore has no carrier-frequency tracking; only sample-clock offset between sound cards (tens of ppm) is tracked.
- **A fixed filter, not a fading channel.** For fixed stations the channel is the two radios' audio chains: a static, radio-dependent frequency response with short time dispersion (about 1–2 ms). It is measured once per transmission from the preamble and equalized per tone.
- **A threshold, then a wide window.** Below roughly 10 dB carrier-to-noise the FM detector collapses. Just above it the audio SNR is already about 12–20 dB depending on deviation, rising to about 40 dB at full quieting. The rate ladder (Section 6) is sized for that 12–40 dB window.
- **Clipping.** Peak deviation is limited by the radio. Audio peaks above the limiter are clipped, which the receiver sees as noise. The modem controls its own peak-to-average ratio and never relies on the radio's limiter (Section 7).
- **Turnaround.** PTT-to-RF and receive recovery take 50–300 ms depending on the radio, plus squelch settling. The protocol pays this once per transmission, so it amortizes it over long frames and piggybacks acknowledgments on data (Section 9).
- **Mobile paths.** Flutter and shadowing on mobile VHF paths create brief dropouts. The design handles them by stepping down fast and recovering fast (Section 10), not by designing the waveform for fading.

### 3.3 Regulatory constraints (US Part 97; other administrations differ)

| Rule | Effect on this design |
| --- | --- |
| §97.307(f): on 2 m, symbol rate ≤ 19.6 kbaud and bandwidth ≤ 20 kHz; on 70 cm, ≤ 56 kbaud and ≤ 100 kHz | The waveform occupies at most 5.6 kHz of audio inside one FM channel; per-tone symbol rate is 88.9 baud. The FCC removed HF symbol-rate limits in 2023 and has proposed removing the VHF/UHF limits; status to be re-checked at each revision. |
| §97.309(b): unspecified digital codes are permitted but must not obscure meaning, and the FCC may require records convertible to the original | The complete waveform, framing, codes and compression dictionaries are published; a reference decoder is freely available; dictionaries are versioned and kept available for archived traffic. |
| §97.113(a)(4): no messages encoded to obscure meaning | No encryption. Signatures are permitted because content stays readable. |
| §97.119: station identification at the end of each communication and at least every 10 minutes, by a permitted method including a data emission using a specified digital code | Source callsign in every frame header in the published format, plus an ID control block (B.6) carrying the callsign in 7-bit ASCII, a specified code under §97.309(a)(3), sent in CONNECT, DISCONNECT, every BEACON and at least every 10 minutes within a session (decision 20.7). A configurable CW or 1200-baud AX.25 ID remains available. |
| §97.221: automatically controlled digital stations | Unattended gateways and relays must observe the sub-band and control rules; out of scope for the modem itself. |

## 4. Architecture overview

The protocol is four layers over a radio, with two host interfaces on top and a negotiated link width at the bottom. The layers are independent so dictionaries, rungs and ARQ can evolve without changing the waveform.

```
+-------------------------------+   +-------------------------------+
| KISS TNC (TCP 8001)           |   | Native API (TCP 8002)         |
| AX.25 apps unchanged:         |   | Sessions, link quality,       |
| BBS, Winlink, APRS            |   | encoding, signatures          |
+---------------+---------------+   +---------------+---------------+
                |                                   |
                v                                   v
+-------------------------------------------------------------------+
| Session layer                                                     |
| PDU framing . streams . zstd with published dictionaries .        |
| optional Ed25519 signatures                                       |
+---------------------------------+---------------------------------+
                                  |
                                  v
+-------------------------------------------------------------------+
| Link layer                                                        |
| Frames with 40-bit addresses . broadcast and sessions .           |
| selective-repeat ARQ . resume . receiver-driven link adaptation . |
| CSMA with yield windows . legacy packet detection                 |
+---------------------------------+---------------------------------+
                                  |
                                  v
+-------------------------------------------------------------------+
| Physical layer                                                    |
| OFDM on 100 Hz tones . 45 ms preamble . header at MCS 0 on the    |
| base tones . 7-rung ladder to 256-QAM . NR LDPC blocks .          |
| clip-and-filter PAPR . quieting carrier detect                    |
+---------------+-----------------------------------+---------------+
                |                                   |
                v                                   v
+-------------------------------+   +-------------------------------+
| Base width: 24 tones          |   | Negotiated width: to 56 tones |
| 300-2700 Hz; carries every    |   | Measured per direction by the |
| preamble, header and control  |   | width probe; 30 tones on      |
| block; any FM radio's mic or  |   | 12.5 kHz, 56 on 25 kHz flat   |
| 1200-baud port                |   | data ports                    |
+---------------+---------------+   +---------------+---------------+
                |                                   |
                v                                   v
+-------------------------------------------------------------------+
| FM transceiver . PTT by CAT, serial, GPIO or CM108 . squelch open |
+-------------------------------------------------------------------+
```

Applications enter through KISS or the native API; everything from the session layer down is shared, and width is the only thing that differs between a handheld's mic path and a flat data port.

### 4.1 What is fixed, negotiated or configured

| Element | Decided by | Notes |
| --- | --- | --- |
| Tone spacing, symbol timing, preamble, header format, block codes | The specification | Changing any of these bumps the on-air version |
| Width cap | Operator configuration (channel width) | 30 tones on 12.5 kHz, 56 on 25 kHz; never exceeded by probe or data |
| Width and tone mask | Negotiated per direction | Width probe at connect and sounding (5.4); mask trimmed and restored in session (10.3) |
| MCS rung | Negotiated continuously | Receiver-driven, per direction (Section 10) |
| Repeater mode, maximum frame time, broadcast rung and width, TXDelay, persistence | Operator configuration | Capped by the specification's limits; receivers need no repeater-mode configuration, but a session through a repeater needs both stations in repeater mode because both transmit |
| Dictionaries, signatures, level calibration (HARQ when specified, 9.4) | Negotiated from capability bits | Intersection of what both stations support |

## 5. Physical layer: waveform

The waveform is OFDM on 100 Hz-spaced tones in the FM audio band, with 10 ms symbols and a 1.25 ms cyclic prefix. Width is negotiated per direction from 24 to 56 tones; the preamble, header and control blocks always use the base 24 tones, so the time structure and the framing machinery are identical at every width.

### 5.1 Sampling and audio interface

- The waveform is defined at 48,000 samples/s, 16-bit, mono. Implementations may process at a lower internal rate (8 kHz for widths up to 30 tones, 16 kHz up to 56) and resample.
- The modem outputs a flat spectrum; it never applies pre-emphasis. Through a mic path the radios' pre- and de-emphasis cancel; through a flat port the per-tone channel estimate absorbs the noise tilt.
- Transmit peak level is fixed by the modem (Section 7); the operator sets radio drive once using the built-in level-setting tone.

### 5.2 OFDM parameters

| Parameter | Value |
| --- | --- |
| Tone spacing | 100 Hz |
| Useful symbol | 10.0 ms (480 samples at 48 kHz) |
| Cyclic prefix | 1.25 ms (60 samples); 2.5 ms (120 samples) in repeater mode or when the receiver requests it (ACK flag bit 3, B.2) |
| Symbol period | 11.25 ms, 88.89 symbols/s; 12.5 ms, 80 symbols/s in repeater mode |
| Tone centers | 350 + 100·k Hz, k = 0 … 55 |
| Base tone set | Tones 0–23 (350–2650 Hz): preamble, header and control blocks at every width |
| Link width | 24–56 tones, negotiated per direction (5.4); occupied band 300 Hz to the highest active tone + 50 Hz (2700 Hz at 24 tones, 5900 Hz at 56) |
| Width cap by channel | 30 tones (to 3300 Hz) on 12.5 kHz; 56 tones (to 5900 Hz) on 25 kHz |
| Pilot tones (continuous) | Tones 3 and 20 (650, 2350 Hz) always; tones 37 and 52 (4050, 5550 Hz) when active and not masked (5.4) |
| Data tones | Width minus active pilots: 22 at 24 tones, 52 at 56; fewer when tones are masked (5.4) |
| Data tone-symbols per second | 1,956 at 24 tones, 4,622 at 56 (1,760 and 4,160 in repeater mode) |
| Internal FFT | 80 points at 8 kHz (up to 30 tones) or 160 points at 16 kHz (up to 56 tones) |

The receiver measures dispersion from the width probe's per-tone phase; if it exceeds about 1.25 ms it sets the "use long prefix" flag (ACK flag bit 3, B.2) and the sender switches to the long prefix in that direction only. Repeater mode forces the long prefix regardless. Receivers detect either prefix without configuration (6.1).

The prefix is the last 60 (or 120) samples of the useful symbol, negated. Every tone completes 3.5 + k cycles in the 10 ms useful symbol, so the waveform is anti-periodic over the useful symbol rather than periodic. The negated tail is the prefix that joins the useful symbol without a step, and it keeps the channel's effect a per-tone complex gain at the receiver, as a cyclic prefix does for whole-cycle tones. A plain copy of the tail would put a phase jump into every tone at the start of the useful symbol.

Tone index k = 0…N−1 sits at 350 + 100·k Hz. Tones are numbered from the low end so that a receiver masking upper tones degrades toward the 24-tone base set rather than toward nothing.

### 5.3 Why these values

- **100 Hz spacing.** The channel has no multipath, so symbol length is set by cyclic-prefix overhead against the audio chain's 1–2 ms dispersion. 10 ms symbols keep that overhead at 12.5% while making the fixed per-frame overhead (preamble, header, ACK) half what 20 ms symbols would cost.
- **Continuous pilots.** Two (or four) known tones in every symbol track the slow phase rotation caused by sample-clock offset between sound cards and the common phase of the whole frame. The full per-tone channel estimate comes from the preamble, not from pilots, because the channel is static within a frame.
- **Band edges.** 300 Hz avoids the DC-blocking and hum region; 2700 Hz is the safe upper edge of a mic path with voice filtering; 5900 Hz is the practical limit of a 25 kHz-channel IF filter at the discriminator.

### 5.4 Width negotiation and tone mask

Width is discovered by measurement, not configured. The operator sets only the channel width, which fixes the cap; the link finds how many tones each direction actually passes.

- **Width probe.** Two symbols of known pilots on every tone up to the sender's cap, sent after the header when header flag bit 4 is set: always in CONNECT, CONNECT-ACK and SOUND, and in a DATA frame when the peer has requested a re-probe (ACK flag bit 2). Because headers use the base tones, a probe is never needed to decode the frame that carries it.
- **Mask report.** The receiver measures which probed tones arrive above threshold and returns a bitmap of usable tones (56 bits, tones 0–55) in its next control block. Pilot tones 3 and 20 cannot be masked; pilots 37 and 52 can, and tracking then uses the remaining pilots. The transmitter puts no data or pilot on masked tones, reassigns coded bits to the remainder, and keeps total power constant: a data symbol carries the power of `width` unit-power tones, with every active tone, pilots included, scaled by the same factor, so a 24-tone link pays no penalty for having probed at 56. Clip-and-filter (7.1) regrows a little energy on masked tones, as it does above the band; that residue may be no higher than the regrowth just above the highest active tone (about 30 dB below the active tones at a 7 dB clip).
- **Mask scope.** The mask applies to the symbols carrying data and control blocks. The preamble and header always use every base tone, and the width probe every tone up to the sender's cap, including masked ones, so a frame decodes without knowing the mask and masked tones keep being measured (10.3). The 56-bit mask applies at any width; bits at or above the width are ignored.
- **Caps.** Neither the probe nor the data ever exceeds the sender's own cap, whatever the far end reports. By Carson's rule, 56 tones through a flat port would not fit a 12.5 kHz channel; the cap is the spectral safety limit and is configuration, not negotiation.
- **Per direction.** Each direction's width is the product of one station's transmit chain and the other's receive chain, so a station with a flat data port talking to a handheld runs wide one way and narrow the other.
- **Broadcast.** Broadcast frames do not probe; they use the configured broadcast width (default 24 tones).

## 6. Physical layer: synchronization, modulation and coding

Every transmission starts with a 45 ms preamble, then a header at the base rate, then zero or more coded blocks at the rung the header names. Seven rungs span QPSK rate 1/2 to 256-QAM rate 3/4.

### 6.1 Preamble (4 symbols, 45 ms)

| Symbols | Content | Purpose |
| --- | --- | --- |
| 1–2 | Sequence S1: constant-amplitude, low-PAPR values on the base 24 tones, sent twice | Detection and coarse timing by autocorrelation at the repeat lag; the lag that peaks (11.25 ms or 12.5 ms) identifies the cyclic prefix; confirmed by cross-correlation with S1, so a steady tone cannot trigger it |
| 3–4 | Sequence S2: BPSK values from a 24-bit PN segment on the base 24 tones, distinct from S1 | Fine timing, per-tone channel estimate (gain and phase) for the base tones, initial SNR estimate, sample-clock offset estimate from the phase slope |

S1 and S2 are fixed and published as test vectors (20.3). The specification sets a performance requirement rather than a metric threshold: at least 99% detection at 6 dB audio SNR and at most one false alarm per 10 minutes on open-squelch noise, measured with the conformance captures (14.5). The width probe (5.4) extends the channel estimate to the tones above the base set.

Detection requires both the autocorrelation plateau and a cross-correlation match against the stored sequence. A steady tone (a repeater courtesy tone, a CW identifier, a DTMF digit) produces the plateau on its own and must not trigger detection.

Receivers run the autocorrelation at both repeat lags and over a small grid of sample-clock offsets in parallel, so repeater mode needs no configuration at the receiving end and weak preambles lock more often; the cost is a few extra correlators.

### 6.2 Modulation and coding scheme (MCS) ladder

Rates scale with the negotiated width; the two extremes are shown. Rates are raw coded-information rates before preamble, header and ACK overhead: they count each block's 16 CRC bits, so payload alone is 1,008/1,024 of each figure (1,925 bps at MCS 0 and 24 tones). Thresholds are 7 dB-equivalent audio SNRs (7.1) for 1% block error in white noise at 24 tones, measured in Stage 1 simulation with the rung's clip level and rounded up to the next 0.5 dB (ADR 0003). Stage 2 adds the hardware margin (10.1).

| MCS | Modulation | Code rate | Payload bits per data tone-symbol | bps at 24 tones | bps at 56 tones | Threshold |
| --- | --- | --- | --- | --- | --- | --- |
| 0 (base) | QPSK | 1/2 | 1.0 | 1,956 | 4,622 | 3.5 dB |
| 1 | QPSK | 3/4 | 1.5 | 2,933 | 6,933 | 6.0 dB |
| 2 | 16-QAM | 1/2 | 2.0 | 3,911 | 9,244 | 8.5 dB |
| 3 | 16-QAM | 3/4 | 3.0 | 5,867 | 13,867 | 12.5 dB |
| 4 | 64-QAM | 2/3 | 4.0 | 7,822 | 18,489 | 17.5 dB |
| 5 | 64-QAM | 5/6 | 5.0 | 9,778 | 23,111 | 21.0 dB |
| 6 | 256-QAM | 3/4 | 6.0 | 11,733 | 27,733 | 25.5 dB |

- Constellations are Gray-mapped, unit average power per tone. The transmitter scales the whole symbol, never individual tones, so the receiver's per-tone estimate stays valid across rungs once the clip-level offset of 7.1 is applied.
- MCS 0 is the base rate: headers, control blocks, link setup and broadcast frames by default. A BPSK rung below it is deliberately omitted (Section 2; decision 20.4): the FM cliff makes it nearly useless, and header robustness, if ever short, comes from a rate-1/3 header code instead.
- MCS 6 requires a linear path and full quieting. It is expected to work through good data ports and rarely through a handheld mic path.
- Rung IDs 7–15 are reserved. Future rungs must not renumber existing ones.

### 6.3 Forward error correction

| Block type | Information bits | Code | Coded lengths | Used for |
| --- | --- | --- | --- | --- |
| Header | 144 (incl. CRC-16) | NR BG2, Z = 15 (k = 150, shortened to 144) | 288 (rate 1/2); 432 (rate 1/3) held as contingency | Every frame, always MCS 0 on the base tones |
| Control | 256 (incl. CRC-16) | NR BG2, Z = 26 (k = 260, shortened to 256) | 512 (rate 1/2) | ACK/NACK, link setup, sounding and calibration reports, ID, path; always MCS 0 on the base tones |
| Data | 1,024 (126 payload bytes + CRC-16) | NR BG1, Z = 48 (k = 1,056, shortened to 1,024); 3,168-bit rate-matching buffer | 2,048 (1/2), 1,536 (2/3), 1,368 (3/4), 1,232 (5/6) | Payload blocks at MCS 0–6 |

- All three codes are 5G NR LDPC base graphs, lifted and shortened (3GPP TS 38.212 §5.3.2; decision 20.2). Shortened information bits are zeros known to both ends and are never transmitted. Every coded length is a prefix of the same rate-matching buffer, so a retransmission can send the next unsent segment of the buffer (hybrid ARQ, 9.4), down to rate 1/3 for data. The three lifted parity-check matrices and the rate-matching tables are published as files at v0.2.
- Each block, header, control and data alike, carries its own CRC-16/CCITT-FALSE (polynomial 0x1021, initial value 0xFFFF, no input or output reflection, no final XOR) inside the information bits. The CRC decides block success; the decoder's syndrome check is not trusted alone.
- Decoding is soft-decision (log-likelihood ratios from the demapper, scaled by the per-tone SNR estimate), layered normalized min-sum, 20–50 iterations with early termination on CRC pass.
- A receiver may re-estimate the channel from blocks that decoded and then re-run demapping and decoding on the ones that failed (decision-directed, iterative). This is an implementation choice worth 0.5–1.5 dB at the high rungs; it never changes what is transmitted.
- A block that fails its CRC is reported individually in the ACK bitmap; the frame is never retransmitted as a whole.
- Contingency (20.4): if Stage 2 shows header losses above the FM threshold, the header moves to rate 1/3 (432 coded bits, 10 symbols, +34 ms per frame) rather than adding a BPSK rung. The contingency must be settled by Stage 2, before v1.0: it is an incompatible on-air change (Section 15), so it cannot be adopted after v1.0.

### 6.4 Interleaving and scrambling

- Coded bits of each block are spread across all active tones and across the symbols the block occupies, so a dead tone or a short burst (squelch tail, impulse noise) costs scattered bits rather than a run.
- All bits after coding are XORed with a published PRBS seeded from the block's position in the frame. This removes repeating patterns that would raise the peak-to-average ratio or bias the pilots.
- The exact scrambler PRBS, the interleaver (defined for every tone mask) and the constellation Gray labeling with its bit-to-tone order are not yet specified here. A later ADR MUST specify them, with test vectors, before v0.2; the properties above are normative meanwhile.
- Blocks are packed back-to-back across symbols; the last symbol of a frame is padded with scrambled zeros.

## 7. Physical layer: levels, carrier detect and timing

The modem bounds its own peak-to-average ratio to 7–10 dB depending on the rung, sets deviation once with a reference tone, detects the channel from FM quieting rather than audio energy, and budgets fixed overhead so that 4 s frames run above 80% efficiency.

### 7.1 Transmit level and PAPR

- OFDM peaks are tamed in the modem by clip-and-filter: each symbol is clipped above its RMS level, re-filtered to the occupied band (the active, unmasked tones), and clipped once more, so PAPR at the output is at most the clip level. The radio's limiter is never the mechanism. The clip level rises with the rung, because the clipper's own distortion (about 22 dB below the signal at 7 dB) would otherwise cap the top rungs (ADR 0003):

| Rung | Clip level above symbol RMS |
| --- | --- |
| MCS 0–3 | 7 dB |
| MCS 4 | 8 dB |
| MCS 5 | 9 dB |
| MCS 6 | 10 dB |

- Every symbol of a frame (preamble, header, probe, control and data blocks) uses the level of the frame's data rung, so the per-tone scale is constant across the frame. A frame with no data blocks uses 7 dB.
- The digital output is scaled so that waveform peaks sit at −3 dBFS at every rung and width. Average level is then about −10 dBFS at a 7 dB clip, and 1 dB lower for each dB of clip level above that (about −13 dBFS at MCS 6). It does not change with width.
- **SNR reference.** Every SNR the protocol reports or compares with a threshold (the ACK's measured SNR, link-test and PROBE results, the thresholds of 6.2 and 10.1) is the 7 dB-equivalent channel SNR: signal over everything the channel and both radios add (noise, interference, dispersion, a radio's own clipping) on the active tones, excluding the distortion clip-and-filter produces by design at the frame's clip level, plus that clip level minus 7 dB (ADR 0006). The receiver knows the rung from the header. An SNR measured at one rung can then be compared directly with another rung's threshold. Where the receiver knows what was sent (preamble, width probe, symbols whose blocks all decoded) the figure is accurate within 1 dB up to at least 40 dB, for example by rebuilding the sent symbols from the decoded bits; elsewhere it may count the designed clipping and err low, but never over-reports by more than 1 dB.
- **Level-setting tone.** A single 1000 Hz tone with the same peak amplitude as the data waveform. The operator sets radio drive so this tone produces 70% of the channel's maximum deviation: 3.5 kHz on a 25 kHz channel, 1.75 kHz on 12.5 kHz. The 30% margin covers level drift and the residual peaks above the clip point. Values to be confirmed on the bench (Section 14).
- Receive side: digital AGC accepts input anywhere from −30 to −3 dBFS. The receiver flags input clipping to the operator; a clipped receive path is the most common cause of a link stuck at low rungs.
- Where the host allows it, the modem sets levels itself (Section 13.5): receive gain automatically between frames, transmit drive by a stepped-tone calibration with a peer.
- The far end reports transmit-side distortion in every acknowledgment (distortion index, Appendix B.2); the sender lowers its drive or warns the operator (13.5) rather than letting link adaptation chase clipping with slower rungs.

### 7.2 Carrier detect (DCD)

The radio runs with squelch open in connected mode, so audio energy alone cannot indicate a busy channel. DCD is the OR of four detectors, each with a 100 ms hold:

| Detector | How | Catches |
| --- | --- | --- |
| Quieting | Noise power in a band just above the highest active tone (2700–3100 Hz on a 24-tone link, 6000–7000 Hz at 56 tones; chosen at setup from the channel width and the measured receive roll-off) drops when any carrier is present | Any FM signal: voice, legacy packet, other modes, this mode |
| Preamble correlator | Autocorrelation of the repeated preamble symbol, confirmed by cross-correlation with the stored sequence | This mode, including weak signals near the cliff; never a steady tone |
| AFSK detector | Energy at 1200 Hz and 2200 Hz alternating at 1200 baud (Goertzel pair) | Bell 202 packet and APRS |
| Radio COS (optional) | Squelch/COS line via the sound interface's GPIO, when wired | Anything the radio's squelch opens for |

A station must not start a transmission while DCD is asserted, except the turn-holder of a session replying to its peer (Section 11).

In repeater mode (11.5) the repeater's carrier is up for the whole session, so quieting only reports whether the repeater is keyed. DCD then uses a modulation detector (voice-band energy above the level measured during hang-time silence) together with the preamble and AFSK detectors, and a session turn-holder treats a keyed repeater with no modulation as clear.

### 7.3 Timing parameters

| Parameter | Default | Range | Notes |
| --- | --- | --- | --- |
| TXDelay (PTT to first preamble sample) | 150 ms | 20–500 ms | Radio-dependent; the biggest avoidable overhead. Measured per radio, not guessed. |
| Repeater access delay | 400 ms | 0–1,000 ms | Repeater mode only: PTT, silence for this long, then the preamble; measured by `calibrate` |
| TXTail (silence after last symbol before PTT release) | 10 ms | 0–50 ms | Lets the last symbol clear the audio chain |
| Receive turnaround (end of peer frame to own PTT) | ≤ 100 ms | — | Processing budget for decode plus ACK assembly |
| DCD hold | 100 ms | 50–300 ms |  |
| Maximum frame on-air time | 4.0 s | 0.5–4.0 s | 4.0 s is a hard ceiling. Also the channel-sharing bound (Section 11); stations exchange their configured maximum in CONNECT (B.3) |

### 7.4 Worked budget: 24 tones, MCS 3, one direction

At MCS 3 (16-QAM 3/4) a symbol carries 88 coded bits and a data block takes 15.5 symbols.

| Item | Time |
| --- | --- |
| TXDelay | 150 ms |
| Preamble | 45 ms |
| Header (7 symbols at MCS 0) | 79 ms |
| 21 data blocks (2,646 payload bytes) | 3,665 ms |
| TXTail | 10 ms |
| Peer turnaround + its TXDelay | 250 ms |
| Peer ACK: preamble + header + control block | 259 ms |
| **Cycle** | **4.46 s for 21,168 payload bits: 4.75 kbps net, 81% of the 5,867 bps raw rate** |

With 2 s frames the same link nets about 4.1 kbps (70%). With a 300 ms TXDelay radio and 2 s frames it nets about 3.6 kbps. TXDelay and frame length matter more than any rung above MCS 4 on a mic path.

## 8. Link layer: frames, addressing and frame types

A frame is one transmission: preamble, one 144-bit header at MCS 0, then up to 128 coded blocks at the header's MCS. The header alone tells a receiver who sent it, to whom, what kind of frame it is, and how to decode the rest.

### 8.1 Frame layout

```
| Preamble | Header | [Probe] | Block 0 | Block 1 | ... | Block N-1 | pad |
| 4 sym    | 7 sym  | 2 sym   | control and/or data blocks at header MCS   |
```

Preamble, header and control blocks use the base 24 tones at every width, so the header is always 7 symbols and any station can decode it. Control blocks, when present, come first and are always coded at MCS 0; data blocks follow at the payload MCS on the negotiated tone set. When header flag bit 4 is set, a 2-symbol width probe (5.4) sits immediately after the header and before the first block, in every frame type.

### 8.2 Header fields (144 bits)

| Field | Bits | Meaning |
| --- | --- | --- |
| Version | 3 | Protocol version, 1 for this spec |
| Frame type | 5 | See 8.4 |
| Flags | 8 | Bit 0: control block present (set if and only if the control block count is 1–3). Bit 1: data blocks present. Bit 2: more fragments (broadcast). Bit 3: session frame: set on every frame a station sends as part of a session, from CONNECT-ACK onward (including SOUND and CAL-REQUEST sent in a session), clear otherwise; it tells a receiver or monitor how to read a SOUND or CAL-REQUEST without tracking session state. Abbreviating session-frame addresses to the session ID is the planned short-session-headers feature (capability bit 4, v2), used only in sessions that negotiate it. Bit 4: width probe follows the header. Bit 5: identical repeat of a broadcast frame with the same frame counter (soft combining, 9.1). Bits 6–7 reserved, zero. |
| Source address | 40 | Callsign + SSID (8.3) |
| Destination address | 40 | Callsign + SSID; all-ones = broadcast |
| Sequence | 16 | Block sequence number of the first data block in this frame (sessions); frame counter (broadcast) |
| Payload MCS | 4 | Rung of the data blocks |
| Control block count | 2 | 0–3, authoritative; a receiver discards a header whose flag bit 0 disagrees with it |
| Data block count | 7 | Count minus 1 (giving 1–128 blocks) when flag bit 1 is set; sent as 0 when flag bit 1 is clear and ignored on receipt |
| Reserved | 3 | Zero |
| CRC-16 | 16 | CRC-16/CCITT-FALSE (6.3) over the preceding 128 bits |

### 8.3 Addressing

- A callsign is up to 6 characters from a 6-bit alphabet (A–Z, 0–9, '/', '-', and padding), packed into 36 bits, plus a 4-bit SSID: 40 bits per address against AX.25's 56. The alphabet and padding rules are in Appendix B.
- Callsigns longer than 6 characters (up to 10) set code 63 in the first character position and carry a 30-bit hash, plus the SSID, in the address field of every header; the full text travels in the ID control block (B.6) sent in CONNECT. The hash is the first 30 bits of SHA-256 over the callsign text as in B.6 (upper-case ASCII including any '/' indicator, no SSID, no padding); an all-ones result is replaced by all-ones minus one so a hash never collides with broadcast. Rare enough that the common case is not taxed for it.
- Station identification: the source callsign is present in every header in this published format. A station may additionally send a CW or 1200-baud AX.25 identification on a timer; the modem provides this as a configurable option (Section 13).

### 8.4 Frame types

| Type | Name | Mode | Carries |
| --- | --- | --- | --- |
| 0 | BEACON | Broadcast | Station presence, capabilities, ID control block (B.6), optional position or status PDU, optional path control block (B.7, v1.x) |
| 1 | UI | Broadcast | Unnumbered data PDUs (APRS-like traffic, AX.25 UI frames via KISS); optional path control block for digipeating (B.7, v1.x) |
| 2 | CONNECT | Session setup | Capabilities, dictionary IDs, width cap, requested MCS, and an ID control block; the width probe follows the header. With flag bit 1 (B.3): resume an interrupted session |
| 3 | CONNECT-ACK | Session setup | Accepted parameters, peer's measurements, usable tone mask; the width probe follows the header. On resume, a second ACK-type control block with the last acknowledged sequence |
| 4 | DATA | Session | Acknowledgment control block for the peer's last frame plus data blocks; an ID control block at least every 10 minutes |
| 5 | ACK | Session | ACK control block first, optionally followed by other control blocks (ID, CAL-REPORT), never data blocks (nothing to send) |
| 6 | SOUND | Session or link test | Preamble, header and width probe; re-measures the channel after idle. In a session, leads with the ACK control block (below). As a PROBE reply, an ACK-type control block with the measurements (9.6) |
| 7 | DISCONNECT | Session | Orderly close: ACK control block first (final acknowledgment), then an ID control block |
| 8 | PROBE | Broadcast | Asks a named station for a link-test reply (9.6) or a BEACON |
| 9 | CAL-REQUEST | Any | Calibration parameters (B.5); the tone bursts of 13.5 follow the frame. In a session, the ACK control block comes first |
| 10 | POLL | Polled mode | Reserved for the v2 polled gateway mode (20.14) |
| 11–30 | reserved |  |  |
| 31 | EXTENDED | any | Type carried in the first control block |

In a session, after setup, every frame a station sends (DATA, ACK, SOUND, DISCONNECT, CAL-REQUEST) carries an ACK control block for the peer's last frame as its first control block; other control blocks and data blocks follow if there are any. A station with data never sends a separate ACK frame. An ACK frame is the ACK block alone or followed by other control blocks (ID, CAL-REPORT), never data blocks.

## 9. Link layer: broadcast mode, sessions and ARQ

Broadcast frames are one-way and self-contained. Sessions are a strict ping-pong between two stations with one frame in flight each way, acknowledged block by block.

### 9.1 Broadcast mode

- BEACON and UI frames carry PDUs at a configured rung (default MCS 0; MCS 1–3 permitted where the operator knows the audience's conditions) and the configured broadcast width. No acknowledgment, no adaptation.
- A PDU larger than one frame is split across frames with the more-fragments flag; receivers reassemble by source and frame counter and discard on any gap. Each fragment is independently decodable and, if compressed, independently decompressed (Section 12).
- A frame may be sent again, bit-identical, with the same frame counter and header flag bit 5 set. A receiver that failed on the original keeps its soft decisions, adds the repeat's and decodes the sum, so three copies decode about 4.8 dB below where one would; stored decisions are discarded after 10 minutes or 64 frames. A receiver that already passed the frame suppresses the repeat by frame counter.
- Digipeating (v1.x, decision 20.11): a BEACON or UI frame may carry a path control block (B.7) naming up to two relay stations or WIDE-style aliases. A relay that matches the next entry retransmits the frame once with its own callsign written into that entry and the hop count decremented, and suppresses duplicates by source and frame counter for 60 s. Sessions are never digipeated; relay for sessions is a host application (20.11).
- AX.25 frames from a KISS host are sent as UI frames. AX.25's own link layer keeps working on top; it simply gets a faster, error-corrected channel.

### 9.2 Session establishment

1. Initiator sends CONNECT: header, width probe (8.1), the B.3 control block, an ID control block (B.6), and optionally a second B.3 block of dictionary IDs. The B.3 block carries protocol capabilities, supported dictionary IDs, its width cap, initial MCS, max frame time and a 16-bit session ID.
2. Responder measures the CONNECT's preamble and probe, and replies CONNECT-ACK with its own capabilities, the usable tone mask for the initiator-to-responder direction, the MCS it wants to receive at, and its own width probe.
3. Initiator's first DATA frame confirms the session and carries the mask for the responder-to-initiator direction in its acknowledgment block. Either side may abort with DISCONNECT. CONNECT is retried at most 3 times at increasing random intervals.

**Resume.** A CONNECT with flag bit 1 (B.3) and a session ID seen within the last 10 minutes continues that session instead of starting one. The responder's CONNECT-ACK adds a second, ACK-type control block whose base sequence is its last acknowledged sequence; the initiator's first DATA frame does the same for the other direction; both continue from there with the negotiated dictionaries, width and signing counters intact. A responder without state answers as a new session. Session state is kept for 10 minutes after link loss and discarded on DISCONNECT.

### 9.3 Selective-repeat ARQ

- Each direction numbers its data blocks with a 16-bit sequence. A frame carries up to 128 consecutive blocks starting at the header's sequence number.
- Every frame sent in a session after setup leads with the acknowledgment control block (8.4). It carries the base sequence and a 128-bit bitmap: one bit per block of the peer's last frame, set if the block's CRC passed.
- The sender's next frame retransmits exactly the unacknowledged blocks (lowest sequence first) followed by new blocks up to the frame limit. One frame in flight per direction keeps the state machine small; on a half-duplex ping-pong link a deeper window gains almost nothing.
- Turn-taking: a station transmits when it has received a frame from its peer or when its ACK timer expires. ACK timer = expected peer frame time + 1.0 s. After 3 expirations the station sends SOUND at MCS 0; after 8 consecutive failures it declares the link lost, keeps the session state for 10 minutes for resume (9.2), and reports the loss to the host.
- A station with nothing to send replies ACK (header plus control block, about 420 ms including TXDelay at 24 tones). If both sides have nothing to send, the session idles: no transmission for up to 60 s, then either side may SOUND or DISCONNECT.
- Delivered blocks are passed to the session layer in order; blocks behind a missing one are held. The hold limit is 256 blocks, after which the receiver NACKs everything beyond the gap to force resynchronization.
- An ID control block (B.6) is sent in CONNECT, in DISCONNECT, and at least every 10 minutes within a session, piggybacked on whichever frame is next (decision 20.7).

### 9.4 Hybrid ARQ with incremental redundancy (v2)

Planned, not final: this section describes intended behavior and will get its own ADR before it is specified. Only the wire reservations are fixed now: capability bit 1 (B.3) and ACK flag bit 1 (B.2) are reserved for HARQ and sent as zero.

- The receiver keeps the soft decisions (per-bit confidence values) of blocks that failed, up to a configurable memory budget (default 64 blocks, about 1 MB).
- For a NACKed block the sender may send only the parity bits that were punctured out at the current rung, marked by a control flag, instead of the whole block. The receiver combines them with the stored soft decisions and decodes the mother code.
- A retransmission at rung 5 (rate 5/6) then costs about 40% of a full block; at rung 3 (3/4) about 50%. The receiver signals support in CONNECT; a sender never uses it unsignaled.

### 9.5 Disconnect

DISCONNECT carries the final acknowledgment in its first control block (8.4). The peer replies DISCONNECT once; no further exchange. Unacknowledged blocks at disconnect are reported to the host as undelivered, never silently dropped.

### 9.6 Link test

A PROBE naming a station asks for a two-way measurement without opening a session. The target replies with a SOUND frame that carries an ACK-type control block holding its measurements of the PROBE (SNR, usable tone mask, suggested rung, distortion index) and a width probe; the initiator measures the reply for the reverse direction. Two transmissions, under 3 s of air time. The host presents the result as expected net kbps in each direction rather than raw figures, since that is the question the operator asked. A PROBE to a station that does not answer within 5 s is reported as unreachable; it may be repeated after the backoff of 11.1.

## 10. Link adaptation

The receiving station measures the channel, chooses the rung it wants to receive at, and tells the sender in every acknowledgment. Each direction runs its own loop, because the two radios, antennas and deviation settings rarely match.

```
+--------------------------------+        +--------------------------------+
| 1. Measure (receiver)          |        | 2. Decide (receiver)           |
| Per-tone SNR from preamble     |        | Down now: header lost or       |
|   and pilots                   | -----> |   block errors over 20%        |
| Block CRC pass/fail per frame  |        | Up: 3 clean frames and SNR     |
| SOUND re-measures after        |        |   2 dB above the next rung     |
|   30 s idle                    |        | Otherwise hold; target 5-10%   |
+--------------------------------+        +---------------+----------------+
               ^                                          |
               | next frame received                      | in the next ACK
               |                                          v
+--------------+-----------------+        +--------------------------------+
| 4. Apply (sender)              |        | 3. Report (in the ACK)         |
| Next frame coded at the        |        | Requested MCS, measured SNR,   |
|   requested rung               | <----- |   error count, distortion      |
| Frame length scaled to rung    |        | Tone mask when it changed      |
| Header still at MCS 0, so it   |        | Rides in the control block;    |
|   always decodes               |        |   no extra frame               |
+--------------------------------+        +--------------------------------+
```

The loop runs once per received frame; steps down are immediate and steps up are deliberate, which is what keeps it from oscillating.

### 10.1 Rules

| Trigger | Action |
| --- | --- |
| Header of a frame not decoded, or more than 20% of its blocks failed | Request one rung lower immediately (two lower if more than 50% failed) |
| 3 consecutive frames with zero block errors and measured SNR at least 2 dB above the next rung's threshold | Request one rung higher |
| Anything else | Hold |
| First frame after CONNECT or SOUND | Request the highest rung whose threshold is 3 dB below the measured SNR |
| No frame from the peer for 30 s | Send SOUND; the reply's preamble and probe refresh the estimate |
| Receive-side clipping detected | Hold the current rung and warn the operator; adaptation cannot fix an overdriven path |
| Frames reported at distortion index 3 or higher (B.2) | Excluded from the step-down count; the sender lowers its drive (13.5) or warns the operator, since a slower rung would only hide clipping |

Thresholds default to the Section 6.2 thresholds plus a margin measured in Stage 2 testing, all on the 7 dB-equivalent scale of 7.1. A receiver may instead compute per-link thresholds at connect by simulating each rung over the measured per-tone channel (a one-second Monte Carlo on the probe's estimate), which recovers the 1–2 dB a blanket margin wastes on good radios. The 5–10% target block error rate is deliberate: throughput is rate × (1 − error rate), and the maximum sits above zero errors.

### 10.2 Frame length by rung

| Rung | Maximum frame time |
| --- | --- |
| 0–1 | 2.0 s |
| 2–6 | 4.0 s (or the configured maximum, whichever is lower) |

Higher rungs shrink payload time while preamble, header and turnaround stay fixed, so they earn longer frames; lower rungs are used where conditions are poor and a lost frame costs more.

### 10.3 Tone mask updates

The receiver drops a tone from the mask when its estimated SNR falls more than 6 dB below the active-tone average for two consecutive frames. A dropped tone is restored only after a width probe shows it recovered: the receiver sets the re-probe flag in an ACK, the sender includes a probe in its next frame, and SOUND frames always carry one. Mask changes travel in the ACK with the mask-present flag; the sender applies them to its next frame.

### 10.4 Broadcast mode

No feedback exists, so broadcast frames use the configured rung (default MCS 0) and the configured broadcast width (default 24 tones). A station that also runs sessions may use its most recent session measurements with that audience as a hint, never as a rule.

## 11. Channel access and coexistence

New transmissions use p-persistent carrier sense; a session's turn-holder may reply without it but must open a listening gap every 30 s and never hold the channel more than 4 s at a time. Legacy 1200-baud packet is detected and deferred to, which is more than existing high-speed modes do.

### 11.1 Starting a transmission

1. Channel must show DCD clear for at least 250 ms.
2. p-persistence: each 100 ms slot, transmit with probability p (default 0.25) if still clear; otherwise wait a slot. The operator may raise p for lightly used channels.
3. CONNECT retries and broadcast repeats use a random backoff of 1–5 s on top.

### 11.2 Inside a session

- The turn-holder (the station whose peer just finished a frame) replies after its turnaround without p-persistence, but still checks DCD for 50 ms first and waits if a third signal is present. A session therefore never talks over a transmission that was already in progress.
- Every 30 s of session time, the next turn-holder waits a 1.0 s yield window before transmitting. Other stations wanting the channel may start in that window; the session then defers as in 11.1.
- A single frame never exceeds the maximum on-air time (4.0 s default). A session may run as long as it has traffic; the yield windows and frame limit are what keep it a neighbor rather than an owner.

### 11.3 Legacy packet on the same channel

- The AFSK detector (7.2) asserts DCD for Bell 202 packet, so this mode defers to packet stations.
- The reference modem also demodulates 1200 AFSK, G3RUH 9600 and IL2P in parallel with this mode (13.7). Legacy frames reach the KISS host, and a station can answer a legacy station in its own mode, so a channel can migrate one station at a time.
- Packet stations' ability to defer to this mode depends on their carrier detect: TNCs and radios using squelch (COS) will see a carrier and wait; software modems using demodulator-based detect may not. That half is outside this specification.
- A 1200-baud HDLC courtesy preamble was considered and rejected (decision 20.8): a tone-based legacy DCD drops within its hold time once OFDM starts, so it would protect at most the first 300 ms of a frame. Instead, when legacy stations have been active on the channel within the last 10 minutes, the modem caps frames at 2 s and halves p. Activity means the AFSK detector (7.2) has asserted, or, in a modem with legacy demodulators (13.7), a legacy frame was decoded; every implementation has the AFSK detector, so every implementation applies the policy. The detector recognizes 1200-baud Bell 202 only; a G3RUH-aware trigger may come with the multi-mode receiver.

### 11.4 Recommendations for channel planning

- Coordinate a dedicated simplex frequency for session traffic where the band plan allows; use shared packet channels for broadcast frames and short sessions.
- Sessions through voice repeaters require repeater mode (11.5) on both stations, because both transmit through the repeater; without it the access delay and squelch tail break the turnaround budget. Broadcast frames at MCS 0–1 need only the access delay and the tone-rejecting preamble detector.

### 11.5 Repeater mode

An operator-selected mode for sessions through a voice repeater. Receivers need no configuration (the prefix length is detected blind), but both stations of a session must be in repeater mode because both transmit through the repeater; a mismatch fails the session. Expect 60–75% of simplex throughput at the same signal margins: two FM hops cost about 3 dB of audio SNR, and every turn pays the repeater's access delay.

| Repeater behavior | Design response |
| --- | --- |
| Access delay: receiver squelch and CTCSS decode plus controller delay lose the first 100–300 ms of every input | Repeater access delay (7.3, default 400 ms, measured by `calibrate`) after TXDelay: PTT, silence, then preamble |
| Hang time and courtesy tone after each input | Turn-holder may key during hang time; the preamble detector rejects steady tones (6.1); DCD hold covers the tail burst |
| Squelch-tail noise burst from the repeater receiver passes through unless the controller chops it | Arrives after the frame's last symbol, so it cannot damage the frame; TXTail unchanged |
| Two cascaded audio chains plus controller filters add time dispersion | Cyclic prefix 2.5 ms instead of 1.25 ms (5.2): 12.5 ms symbols, 80 symbols/s, raw rates 10% lower |
| Repeater carrier is up for the whole session, so quieting always reads busy | DCD uses the modulation detector of 7.2 plus the preamble and AFSK detectors; a keyed repeater with no modulation counts as clear for the turn-holder |
| Repeater time-out timers and other users | Session segments capped at 120 s, then a 10 s pause that lets the repeater drop; 2 s yield window (longer than typical hang time) every 30 s |
| DTMF decoders on the controller | Tones sit near DTMF pairs (750, 850, 950, 1350, 1650 Hz) but no pair dominates a 24-tone symbol; Stage 2 test item |

Neither station hears the other directly, only the repeater output, which changes nothing in the ping-pong. Width stays at the 24-tone base set. Worked example at MCS 3 with 4 s frames: 17 blocks per frame, 4.9 s cycle, about 3.5 kbps net against 4.8 kbps simplex.

The etiquette rules are part of the mode, not optional, and the operator's periodic identification setting still applies. Receivers detect the prefix length blind (6.1), so receiving needs no configuration; repeater mode selects only what a station transmits (the long prefix and the access delay). Both stations of a session transmit, so both must be in repeater mode; a mismatch fails the session.

## 12. Session layer: PDUs, content encoding and authentication

The session layer turns the ordered block stream into PDUs (messages), optionally compresses them against published dictionaries, and optionally signs them. It sits above the link layer so dictionaries and codecs can evolve without touching the air interface.

### 12.1 PDU framing

| Field | Size | Meaning |
| --- | --- | --- |
| Length | 2 bytes | PDU length after this field, 1–65,535 |
| Type | 1 byte | 0 AX25 (a raw AX.25 frame from KISS), 1 MESSAGE (opaque bytes for the native API), 2 FILE-CHUNK, 3 SIGNATURE, 4 CONTROL, 5 RATELESS (reserved for v2), 6 AX25-CONNECTED (proxied connected-mode payload, v1.x, 13.1), 7–255 reserved |
| Stream | 1 byte | Bits 7–6: priority, 0 highest. Bits 5–0: stream number, 0 = default. Streams are independent ordered channels within a session (12.4) |
| Encoding | 1 byte | 0 raw, 1 zstd without dictionary, 2 zstd with dictionary, 3 schema-encoded form (v2) |
| Dictionary ID | 4 bytes, only when encoding = 2 | First 4 bytes of the SHA-256 of the dictionary file |
| Body | variable |  |
| CRC-32 | 4 bytes | Over the decoded body; a dictionary mismatch fails here, loudly, rather than yielding plausible garbage |

PDUs are packed back-to-back into the block stream and may span blocks. In broadcast mode each frame's PDUs are complete within that frame and compressed independently, so a lost frame cannot corrupt the next.

### 12.2 Content encoding and dictionaries

- zstd is the only codec in v1: open, deterministic, decoders in every language, and dictionary training is built in. A session negotiates the intersection of both stations' dictionary ID lists in CONNECT/CONNECT-ACK; no common dictionary means encoding 0 or 1.
- Dictionaries are published files with a version, a hash, a training-corpus description and a license (CC0 only). A dictionary once published is never withdrawn, so archived traffic stays decodable. A dictionary contains literal fragments of its training data, so training material MUST be public domain, CC0, or covered by a rights-holder CC0 waiver; `REGISTRY.md` records each source and its license, and the dictionary pull request reviews them. Captured traffic is allowed only for beacons, bulletins and broadcast traffic meant for redistribution, never private messages.
- Governance (decision 20.9): dictionaries live in `dictionaries/` in the specification repository with a `REGISTRY.md` listing ID, version, SHA-256, corpus description, reproducible training command (zstd version, parameters, seed) and license. Releases are signed by at least two keyholders with published keys; new dictionaries arrive by pull request with held-out measurements; the license is CC0; files are never deleted.
- The reference set at v1: a general English-text dictionary, a forms dictionary trained on ICS/emergency-message templates, and an AX.25/APRS dictionary. Expected gains, to be measured on a real corpus before v0.2: 2.5–4× on short text, 5–20× on structured forms once schema encoding exists, none on already-compressed payloads. Winlink B2F traffic arrives compressed and should be sent raw.
- Dictionary fetch (v1.x): a station missing a dictionary its peer offers may request it with a CONTROL PDU (subtype 0, dictionary ID); the peer answers with an offer (subtype 1: ID, size, SHA-256) and sends the file as FILE-CHUNK PDUs on a low-priority stream, about a minute of air time for a 100 kB dictionary at 15 kbps. The session runs at encoding 0 or 1 until the file arrives and its hash verifies.
- Schema encoding (type 3) is reserved for v2: form fields sent as values against a published schema, then dictionary-compressed.

### 12.3 Authentication (optional)

- A SIGNATURE PDU carries an Ed25519 signature over the concatenation of source address, destination address, a 32-bit monotonic counter, and the preceding PDU's decoded body. The counter prevents replay.
- Public keys are distributed outside the protocol (a published key list per group, or exchanged in person). The modem exposes verify results to the host; it never drops unsigned or unverifiable traffic on its own.
- Nothing is encrypted. The signature is over readable content, which keeps it inside §97.113(a)(4).

### 12.4 Delivery semantics to the host

- Session PDUs are delivered in order within their stream, exactly once, or reported as undelivered at disconnect. Streams are independent: the sender interleaves them at block boundaries, highest priority first, so a short PDU on a high-priority stream waits at most one block behind a bulk transfer on another.
- FILE-CHUNK bodies begin with a 32-bit object ID and a 32-bit byte offset, so a transfer interrupted and resumed (9.2) or restarted later continues from the last delivered offset.
- Broadcast PDUs are delivered as received; duplicates from repeats are suppressed within a 60 s window by source and frame counter.

## 13. Host interface

The modem presents two interfaces at once: a KISS TNC for existing AX.25 software and a native API for applications that want sessions, link quality and compression. PTT is driven by the modem through CAT, a serial line, GPIO or a CM108-class sound interface; VOX is not supported for sessions.

### 13.1 KISS compatibility

- KISS over serial and over TCP (default port 8001), standard framing including TXDelay/persistence/slot-time commands, which map onto the parameters in Sections 7 and 11.
- Each AX.25 frame from the host becomes one AX25 PDU in a UI frame (broadcast mode) at the configured rung. Received UI frames carrying AX25 PDUs are returned to the host unchanged.
- Frames received in legacy modes by the multi-mode receiver (13.7) are delivered to the KISS host like any other. A frame addressed to a station last heard in a legacy mode is transmitted in that mode; the per-destination policy lives in the host configuration and can be overridden per callsign.
- AX.25 connected-mode proxy (v1.x, decision 20.10): the modem completes the AX.25 handshake with its host (SABM/UA), opens a native session to the far modem, carries I-frame payloads as AX25-CONNECTED PDUs (type 6) and acknowledges locally so the host's timers are satisfied; the far modem re-originates AX.25 to its host. End-to-end reliability comes from the native ARQ, disconnects propagate both ways, and a far station last heard in a legacy mode gets pass-through in that mode instead. This is what gives Winlink RMS Packet, BPQ32 and JNOS native speed unchanged.

### 13.2 Native API

A line-oriented JSON protocol over TCP (default port 8002) and a Unix socket. Commands: `connect` (with an optional resume of a recent session), `disconnect`, `send` (session or broadcast, with stream, priority and encoding preference), `beacon`, `probe` (link test, 9.6), `heard`, `status`, `config`, `calibrate`, `capture` (save the raw-audio buffer and decoder state for the last N seconds). Events: `rx` (PDU with source, stream, type, encoding, verify result), `session` (state changes, plus bytes delivered, rate, ETA and retransmission count), `link` (per-frame SNR, MCS in each direction, width and tone mask, block error rate, far-end distortion index), `heard` (every decoded header: callsign, SSID, time, SNR, width, frame type, mode), `channel` (DCD state, who holds the channel, time to the next yield window, why the modem is waiting), `dcd`, `error`. The schema is published with the reference implementation; a second implementation must speak the same schema to count toward v1.0. Every implementation accepts the full command set; `capture` and `calibrate` may instead return the schema's defined `unsupported` error (an implementation without diagnostic capture or host level control), and the API schema tests accept that answer. All other commands, including `heard`, are required.

### 13.3 PTT control

| Method | Notes |
| --- | --- |
| CAT via Hamlib/rigctld | Preferred where the radio supports it; adds its own latency, measured into TXDelay |
| Serial RTS or DTR | Classic sound-card interfaces |
| GPIO | Raspberry Pi and similar |
| CM108/CM119 HID GPIO | Digirig, DRA-xx and most USB sound interfaces |
| VOX | Broadcast mode only; the added delay breaks session turnaround |

### 13.4 Configuration

Callsign and SSID; channel width (sets the width cap) and repeater mode; audio devices and levels, or an SDR receive front end (13.7); PTT method; TXDelay, repeater access delay and TXTail; p-persistence and slot time; maximum frame time; broadcast rung and width; dictionary set and directory; signing key (optional); periodic ID (off, CW, or 1200-baud AX.25 beacon, with interval); yield-window interval; per-destination legacy mode policy; unattended operation (auto-accept, per-callsign allow and deny lists, scripting hooks on connect, PDU and disconnect); diagnostic capture (ring-buffer length, capture-on-failure); and, from v1.x, channel presets that bundle frequency, channel width, repeater mode and broadcast width and tune the radio when CAT is in use. A rig profile (13.6) supplies the radio-dependent defaults. A `calibrate` command plays the level-setting tone and, with a cooperating peer, measures round-trip turnaround to recommend a TXDelay (and the repeater access delay in repeater mode), then runs the stepped-tone drive calibration of 13.5.

### 13.5 Audio level control

The modem manages levels itself wherever the host allows: receive gain locally and automatically, transmit drive through a short calibration with a cooperating station, and an in-session trim driven by the far end's distortion report (B.2). Mis-set levels are the top real-world failure of sound-card modes.

**Receive gain (local, automatic).**

- The modem owns the input gain of the device it opened: ALSA or PipeWire mixer on Linux, CoreAudio volume on macOS, the endpoint volume API on Windows. USB Audio Class feature units expose these for most CM108/CM119 interfaces.
- Target: open-squelch noise near −20 dBFS RMS and signal peaks never above −3 dBFS. Adjust in 3 dB steps, between frames only; a gain change mid-frame invalidates the channel estimate.
- Where the interface has a physical pot (SignaLink) or the OS exposes no control, the modem falls back to a meter and instructions. OS audio "enhancements" (AGC, noise suppression, echo cancellation) corrupt the waveform; the implementation detects what it can and the setup checklist covers the rest.

**Transmit drive (stepped-tone calibration with a peer).**

1. Initiator sends CAL-REQUEST (frame type 9) with a control block (B.5) naming the tone (1000 Hz), step count (8), step length (400 ms with 100 ms gaps), start level and step size (3 dB).
2. Initiator transmits the tone bursts, raising its output mixer level each step across a 24 dB span.
3. Peer measures each burst's received amplitude and harmonic distortion (energy at 2 kHz and 3 kHz relative to 1 kHz) and returns a CAL-REPORT control block.
4. Initiator finds the knee: the first step where amplitude rises less than 0.5 dB per dB of drive, or distortion exceeds 5%. It sets its output so data-waveform peaks sit 3 dB (70%) below the knee and stores the result per radio.
5. Knee not found within the span: the modem tells the operator to raise the radio's mic or data-port gain and repeats. Peer reports its own ADC clipping during a step: that step is discarded and the sweep repeated from a lower start.

The exchange takes about 6 s of air time and needs no calibrated equipment at the far end, because it looks for the limiter's knee rather than an absolute deviation. Stations advertise support with capability bit 5 (B.3).

**In-session trim.** When the far end reports a distortion index of 2 or higher for two consecutive frames, the modem lowers its output 1 dB per frame until the index clears, then holds. Without host level control it shows a "reduce drive" warning instead. The index is logged per frame.

**Predistortion (v1.x; planned, not final).** Described here for design continuity; it will get its own ADR. With a 16-step sweep the calibration also yields the radio's input-to-output curve. The modem fits it and pre-expands peaks so that the limiter's compression is cancelled, which buys 1–2 dB of usable deviation on mic paths. The far end's distortion report guards against over-correction: any rise in the index backs the correction off.

### 13.6 Reference implementation requirements

These cost no spectrum and decide whether people get a link on the first evening. A second implementation need not provide them. The conformance suite's recorded captures come from the reference implementation's diagnostic capture; a new implementation only has to decode them (14.5).

| Requirement | Version | What it provides |
| --- | --- | --- |
| Guided setup and self-test | v1 | Device and PTT selection, PTT test, level-setting tone, `calibrate` with a peer, saved per-radio profile |
| Rig profile library | v1 | TXDelay, access delay, port type and quirks by radio model, shipped and community-maintained; measured values offered back, opt-in |
| Heard list | v1 | Every decoded header, in every mode, with time, SNR, width and frame type; `heard` command and export |
| Link dashboard with a "why is it slow" panel | v1 | Per-tone SNR, constellation, rung and width per direction, distortion index, block errors, net rate and ETA; symptoms mapped to causes (clipping, TXDelay, mask collapse, low deviation) |
| Transfer progress | v1 | Bytes, rate, ETA and retransmission count per session, in the API and UI |
| Etiquette status | v1 | DCD state, who holds the channel, time to the next yield window, why the modem is waiting |
| Unattended operation | v1 | Auto-accept with per-callsign allow and deny lists, scripting hooks, automatic periodic ID |
| Rolling raw-audio buffer and diagnostic capture | v1 | The last hours of received audio (58 MB per hour at 8 kHz, 350 MB at 48 kHz) and decoder state; saved opt-in on a failed frame or on request; frames that failed in real time get a second decode with heavier settings; captures become conformance test vectors |
| Link analytics and coverage maps | v1.x | Long-term per-tone SNR and heard logs plotted against positions: a group's self-measured coverage and best time of day |
| Channel presets with CAT tuning | v1.x | Presets bundle frequency, channel width, repeater mode and broadcast width; the modem tunes the radio when CAT is in use |
| Reference chat and file client | v1.x | Keyboard chat and file transfer with resume, exercising the native API; doubles as the interoperability harness |

### 13.7 Front ends: multi-mode receiver and SDR

- **Multi-mode receiver (v1.x).** The reference modem runs 1200 AFSK (Bell 202), G3RUH 9600 and IL2P demodulators in parallel with this mode on the same audio input, each costing a few percent of one core. Decoded legacy frames go to the KISS host and the heard list records each station's mode. The transmitter answers a station in the mode it was last heard in, unless the operator overrides the policy per callsign. Nothing on the air changes; a channel can migrate one station at a time.
- **SDR receive front end (v1.x).** An RTL-SDR-class receiver replaces or supplements the sound-card input: the modem channelizes a 2 MHz span into a dozen FM channels and demodulates each in software, which gives a flat 56-tone receive path from any transceiver, including a handheld transmitting on its mic path, and RF-level carrier-to-noise for DCD and adaptation. Transmit still goes through the transceiver, so the width stays per direction. A Pi 4 handles a dozen channels.
- **Cross-band full duplex (v2).** SDR receive on one band with transmit on the other, through a diplexer or separate antennas, removes turnaround overhead and doubles symmetric throughput; it needs a continuous-frame session mode (16.2).

## 14. Performance targets and test plan

The targets below are what v1.0 must demonstrate on real radios; the simulation stage exists to find out where the draft's nominal values are wrong before any radio is touched.

### 14.1 Targets

| Target | 24 tones | 56 tones |
| --- | --- | --- |
| Net payload throughput, one direction, 4 s frames, TXDelay 150 ms | ≥ 5.0 kbps at 25 dB audio SNR | ≥ 16 kbps at 30 dB audio SNR |
| Minimum usable link | ≥ 1.2 kbps net at 8 dB audio SNR (MCS 0) | same |
| Rate adaptation | Correct rung within 2 frames of a 6 dB SNR change; no oscillation over 60 s at constant SNR | same |
| Width negotiation | Usable width settled within one round trip; a masked tone restored within one re-probe after the impairment clears | same |
| Block error rate at each rung's switching point | ≤ 10% | same |
| Transmit drive calibration | Finds the limiter knee within ±1.5 dB on three radio makes; sets drive within 10% of the level-setting-tone target | same |
| Distortion report | Index rises to 3 or more within two frames of a 6 dB overdrive, and returns to 0 within two frames of its removal | same |
| Repeater mode | A 10-minute session sustained through a repeater with hang time and courtesy tone; net ≥ 60% of the simplex figure at the same SNR | same |
| Session resume | A transfer interrupted by 60 s of link loss completes with no delivered block re-sent | same |
| Link test | Both directions' expected net kbps reported within 3 s of air time and within 20% of what a following session achieves | same |
| Soft combining | Three identical repeats decode a bulletin at an SNR 4 dB below the single-copy threshold | same |
| Multi-mode receiver | Decodes 1200 AFSK and G3RUH captures with sensitivity at least equal to the reference software TNC on the same captures | same |
| False carrier-detect rate with squelch open, no signal | ≤ 1 per 10 minutes | same |
| Deference to AFSK packet | No transmission starts within 100 ms of a detected Bell 202 frame | same |
| Sample-clock tolerance | ±100 ppm between stations with no block-error penalty | same |
| Interoperability | Two independent implementations pass the conformance suite and exchange 1 MB error-free in each direction | same |

### 14.2 Stage 1: simulation

A Python/NumPy model of the full transmit and receive chain, run entirely offline, with a channel model made of these stages in order: FM transmit pre-emphasis (mic path) → mic-path band-pass and limiter → deviation clipping → additive white noise at a set audio SNR → FM receive noise tilt (flat port) → de-emphasis (mic path) → receive band-pass → sample-clock offset (resampling by 1 + ε) → DC offset and 60 Hz hum; a repeater-mode variant cascades two such chains with a controller band-pass and 2 ms of added dispersion. Outputs: block error rate versus SNR per rung, preamble detection rate versus SNR, PAPR distribution before and after clip-and-filter, and net throughput versus TXDelay and frame length. Pass criterion for moving on: the rung thresholds of Section 6.2 set from the measured values (done in ADR 0003; the draft's nominal values were within 3 dB at MCS 0–4 and are replaced). Stage 2 confirms them within 3 dB on real audio chains.

### 14.3 Stage 2: bench

Two radios of different makes into a dummy load through a step attenuator, with calibrated noise injected at the receiver. Sweep audio SNR from the FM threshold to full quieting; repeat with a handheld on its mic/speaker path and a mobile on its 9600 port. Measure: everything from Stage 1 on real audio chains, plus actual TXDelay per radio, DCD behavior against a 1200-baud packet station, and the real deviation produced by the level-setting procedure.

### 14.4 Stage 3: on air

A fixed 5–20 km path and a mobile path. Run the Stage 2 sweep where conditions allow, plus 24 hours of unattended sessions logging rung, block error rate and link losses. Share the channel with a live packet station for at least one session to validate Section 11.

### 14.5 Conformance suite

Published with the specification: preamble waveforms per profile; header, control and data block encodings as known-answer files (input bytes → coded bits → samples); recorded off-air captures (saved by the diagnostic capture of 13.6, in every mode the receiver supports) with expected decoded output; API schema tests; and a dictionary round-trip set. A new implementation is conformant when it decodes every capture and its own transmissions decode in the reference implementation.

## 15. Versioning, interoperability and governance

The protocol is versioned on the air, capabilities are negotiated per session, and every artifact a decoder needs (spec, codes, dictionaries, test vectors) is published under an open license so the mode outlives any one maintainer.

- **On-air version.** The 3-bit header version is bumped only for incompatible waveform or header changes. Everything else is negotiated through capability bits in CONNECT (16 bits in v1, 8 reserved) and through reserved rung and type IDs. Reserved fields are sent as zero and ignored on receipt.
- **Spec versioning.** Major.minor. Minor versions add rungs, PDU types, dictionaries or capabilities without breaking v1 decoders. v1.0 is declared when two independent implementations pass Section 14.5.
- **Licensing.** Specification and test vectors: CC BY 4.0. Reference implementation: Apache-2.0 (Section 2), chosen over MIT for its explicit patent grant from contributors. Dictionaries: CC0 only, with their training corpus described and every source public domain, CC0 or under a rights-holder CC0 waiver (12.2).
- **Publication.** A single public repository holds the spec, matrices, preamble sequences, dictionaries with hashes, test vectors and the conformance runner. Releases are tagged and archived outside the repository host (for example on Software Heritage and in an archive maintained by a club or group), so a lost account or an abandoned project does not take the decoder with it.
- **Records.** Because dictionaries are never withdrawn and the decoder is public, any logged transmission remains convertible to its original content, which is what §97.309(b) can require.
- **Change process.** Proposed changes are issues against the spec with a test-vector update. A change that alters on-air behavior needs an implementation in two code bases before merge.
- **Reserved identifiers.** These are held for the designs named and MUST NOT be used for anything else; senders send them as zero or not at all, and receivers ignore them until the design is specified.

| Identifier | Reserved for | Where |
| --- | --- | --- |
| Capability bit 8 (B.3) | Full-duplex session mode (v2) | 20.13 |
| Frame type 10 POLL (8.4) | Polled gateway mode (v2) | 20.14 |
| PDU type 5 RATELESS (12.1) | Rateless broadcast (v2) | 20.12 |
| Capability bit 3 (B.3) and content encoding 3 (12.2) | Schema encoding (planned) | 12.2 |
| Capability bit 4 (B.3) | Short session headers (v2); header flag bit 3 stays the live session-frame flag | 8.2, B.3 |
| Path control block type 6 in CONNECT (B.7) | Session relay (v2) | 20.11 |
| Capability bit 1 and ACK flag bit 1 | HARQ (planned, 9.4) | B.2, B.3 |

- **Header contingency.** The rate-1/3 header code (6.3, 20.4) is an incompatible on-air change; it must be settled by Stage 2, before v1.0. If Stage 2 triggers it, a superseding ADR adopts it.

## 16. Open questions and roadmap

All fourteen questions were decided on 2026-11-05; Section 20 records each decision with its reasoning. The ones that keep a measurement step say so, and the measurement can move a number within the stated range but does not reopen the decision.

### 16.1 Open questions

- [x] Name: Cairn, alternates Timberline and Talus, pending a collision check; KISS on 8001, native API on 8002 (20.1).
- [x] LDPC: 5G NR BG1 at Z = 48 for data, BG2 at Z = 26 for control blocks, BG2 at Z = 15 for headers; matrices published as files at v0.2 (20.2).
- [x] Preamble: S1 repeated plus S2; the repeat lag identifies the prefix; 99% detection at 6 dB and one false alarm per 10 minutes as the requirement; sequences chosen by Stage 1 search (20.3).
- [x] No BPSK rung; a rate-1/3 header code is the contingency (20.4).
- [x] 100 Hz tone spacing; the prefix is the dispersion knob; Stage 2 logs group delay (20.5).
- [x] Levels: 70% is the manual fallback; the calibrated target is 3 dB below the measured knee, moving to 1.5 dB if predistortion delivers (20.6).
- [x] Identification: ASCII ID control block in CONNECT, DISCONNECT, BEACON and every 10 minutes; ARRL confirmation as a residual (20.7).
- [x] Courtesy preamble rejected; 2 s frames and halved p when legacy stations are active (20.8).
- [x] Dictionaries: registry, two signers, pull requests with held-out numbers, CC0, never deleted; corpus and measurement plan (20.9).
- [x] AX.25 connected-mode proxy over native sessions, v1.x (20.10).
- [x] Digipeating for broadcast frames only, with a path control block; session relay as a host application; store-and-forward in the BBS (20.11).
- [x] Rateless broadcast designed around RaptorQ, LT as fallback (20.12).
- [x] Full-duplex session mode: v2 parameters recorded (20.13).
- [x] Polled gateway mode: v2 parameters recorded (20.14).

### 16.2 Roadmap

| Version | Contents | Exit criterion |
| --- | --- | --- |
| v0.1 (this draft) | Architecture and nominal parameters; review additions of Sections 17–19 folded in; decisions of Section 20 | Decisions in Section 2 confirmed by ADR 0002 |
| v0.2 | Stage 1 simulation; NR matrices, S1/S2 preambles and thresholds fixed; measured compression gains | Rung thresholds set from Stage 1 (6.2); test vectors published |
| v0.5 | Real-time reference implementation: 24-tone width, broadcast and sessions with resume, link test, ID control block, blind prefix detection (both prefix lengths received without configuration, 6.1), KISS, CSMA, receive auto-gain, heard list, guided setup, rig profiles, raw-audio buffer and diagnostic capture, unattended mode; bench results | Section 14.1 24-tone targets met on two radio makes |
| v1.0 | Full 56-tone width, drive calibration and distortion report, zstd dictionaries with governance, signatures, native API frozen; second implementation | Conformance suite passed by two code bases; 1 MB each way error-free on air |
| v1.x | Multi-mode receiver, SDR receive front end, iterative receiver, soft combining of repeats, stream multiplexing, dictionary fetch, per-link thresholds, predistortion, repeater mode, AX.25 connected-mode proxy, broadcast digipeating with path block, schema-encoded forms, channel presets, reference chat and file client, link analytics, periodic-ID options | Backward compatible with v1.0 |
| v2 | Hybrid ARQ with incremental redundancy; per-tone bit loading; short session headers; session relay application with via addressing; continuous-frame full-duplex sessions (20.13); rateless broadcast on RaptorQ (20.12); polled gateway mode (20.14); propagation prediction with cached terrain | Header version bump only if unavoidable |

## 17. Change record: review additions (accepted 2026-10-29)

Four additions came out of review and were accepted the same day. Their normative text now lives in the sections each item names; this section is kept as the rationale record. Section 6.1 also carries the related fix: the preamble detector rejects steady tones.

### 17.1 Host audio level control

**Verdict.** Possible on Linux, macOS, Windows and the Pi for most USB interfaces, and worth doing: mis-set levels are the top real-world failure. Receive gain can be fully automatic and local. Transmit drive needs a cooperating station because the modem cannot see deviation; a short stepped-tone exchange finds the radio's limiter knee without any calibrated equipment at the far end.

**Receive gain (local, automatic).**

- The modem owns the input gain of the device it opened: ALSA or PipeWire mixer on Linux, CoreAudio volume on macOS, the endpoint volume API on Windows. USB Audio Class feature units expose these for most CM108/CM119 interfaces.
- Target: open-squelch noise near −20 dBFS RMS and signal peaks never above −3 dBFS. Adjust in 3 dB steps, between frames only; a gain change mid-frame invalidates the channel estimate.
- Where the interface has a physical pot (SignaLink) or the OS exposes no control, the modem falls back to a meter and instructions. OS audio "enhancements" (AGC, noise suppression, echo cancellation) corrupt the waveform; the implementation detects what it can and the setup checklist covers the rest.

**Transmit drive (stepped-tone calibration with a peer).**

1. Initiator sends CAL-REQUEST (new frame type 9) with a control block naming the tone (1000 Hz), step count (8), step length (400 ms with 100 ms gaps), start level and step size (3 dB).
2. Initiator transmits the tone bursts, raising its output mixer level each step across a 24 dB span.
3. Peer measures each burst's received amplitude and harmonic distortion (energy at 2 kHz and 3 kHz relative to 1 kHz) and returns a CAL-REPORT control block.
4. Initiator finds the knee: the first step where amplitude rises less than 0.5 dB per dB of drive, or distortion exceeds 5%. It sets its output so data-waveform peaks sit 3 dB (70%) below the knee and stores the result per radio.
5. Knee not found within the span: the modem tells the operator to raise the radio's mic or data-port gain and repeats. Peer reports its own ADC clipping during a step: that step is discarded and the sweep repeated from a lower start.

The exchange takes about 6 s of air time. VARA FM has a comparable auto-tune that relies on a cooperating station; this version needs no calibration at the far end because it looks for the knee rather than an absolute deviation.

**Protocol additions.** Frame type 9 CAL-REQUEST; control block types 3 (calibration request) and 4 (calibration report: eight 8-bit amplitudes in 0.25 dB steps relative to full scale, eight 4-bit distortion values, eight clip flags). The `calibrate` command in 13.4 drives it.

**Spec impact.** Sections 7.1, 8.4, 13.4; new 13.5; Appendix B.5.

- [x] Accept 17.1

### 17.2 Repeater mode (optional, v1.x)

**Verdict.** Feasible as an operator-selected mode, set on both stations of a session. Expect 60–75% of simplex throughput at the same signal margins: two FM hops cost about 3 dB of audio SNR, and every turn pays the repeater's access delay. Sessions through a repeater are a courtesy problem before they are a technical one, so the mode carries etiquette rules.

| Repeater behavior | Design response |
| --- | --- |
| Access delay: receiver squelch and CTCSS decode plus controller delay lose the first 100–300 ms of every input | Repeater access delay parameter (default 400 ms, measured by `calibrate`) added after TXDelay: PTT, silence, then preamble |
| Hang time and courtesy tone after each input | Turn-holder may key during hang time; the preamble detector rejects steady tones (6.1 fix); DCD hold covers the tail burst |
| Squelch-tail noise burst from the repeater receiver passes through unless the controller chops it | Arrives after the frame's last symbol, so it cannot damage the frame; TXTail unchanged |
| Two cascaded audio chains plus controller filters add time dispersion | Cyclic prefix 2.5 ms instead of 1.25 ms (symbol period 12.5 ms, 80 symbols/s, raw rates 10% lower), set on each transmitting station |
| Repeater carrier is up for the whole session, so the quieting detector always reads busy | In repeater mode DCD uses a modulation detector (voice-band energy above the hang-time silence level) plus the preamble and AFSK detectors; quieting only reports whether the repeater is keyed |
| Repeater time-out timers and other users | Session segments capped at 120 s, then a 10 s pause that lets the repeater drop; 2 s yield window (longer than typical hang time) every 30 s |
| DTMF decoders on the controller | Tones sit near DTMF pairs (750, 850, 950, 1350, 1650 Hz) but no pair dominates a 24-tone symbol; Stage 2 test item |

Neither station hears the other directly, only the repeater output, which changes nothing in the ping-pong. Broadcast frames at MCS 0–1 need only the access delay and the tone-rejecting detector and may be used without the full mode. Worked example at MCS 3 with 4 s frames: 17 blocks per frame, 4.9 s cycle, about 3.5 kbps net against 4.8 kbps simplex.

**Protocol additions.** None on the air beyond the cyclic-prefix variant; the mode is configuration. Receivers detect either prefix length blind (6.1), so no receiver configuration is needed; a session needs repeater mode on both stations because both transmit. Responder mirroring through a reserved capability bit is a v1.x candidate.

**Spec impact.** 5.2 (prefix row), 7.2, 7.3, 11.4 reworded, new 11.5.

- [x] Accept 17.2

### 17.3 Negotiated width (replaces the two fixed profiles)

**Verdict.** Yes, and it simplifies the spec. The tone mask already adapts width within a profile; what it cannot do today is choose the profile. Making width a negotiated quantity, 24 to 56 tones per direction, removes NARROW and WIDE as separate profiles, gives intermediate radios intermediate widths, and lets a mismatched pair run each direction at its own best width.

- One profile. Preamble, header and control blocks always use the base 24 tones, so any station can decode any frame at any width. Cost: a wide frame's header takes 7 symbols instead of 3, about 45 ms, under 2% of a 4 s frame.
- Width probe: 2 symbols of known pilots on every tone up to the sender's cap, appended to CONNECT, CONNECT-ACK and SOUND, and to a DATA frame when the peer sets a re-probe flag. The receiver measures which tones arrive above threshold and returns the mask in its next control block (fields already in B.2 and B.3). After one round trip both directions know their usable width; Section 10.3 then keeps trimming and restoring.
- Caps are configuration, not negotiation. The operator sets the channel width and the cap follows for spectral safety: 30 tones (to 3,250 Hz) on a 12.5 kHz channel, 56 on 25 kHz. By Carson's rule 56 tones through a flat port would not fit a 12.5 kHz channel, so neither the probe nor data ever exceeds the cap, whatever the far end requests. The audio path type (mic or flat port) needs no configuration; the measurement finds it.
- Transmit power stays constant across the active tones: power from masked tones is reassigned, so a 24-tone link pays no penalty for having probed at 56.
- Broadcast frames do not negotiate; they use the configured broadcast width (default 24). The quieting-detector band follows the configured channel width.

**Protocol additions.** CONNECT control block: "Requested profile" (2 bits) becomes "Width cap" (6 bits, highest tone index), taking 4 of the reserved bits. ACK flags: re-probe request. Header flags: width probe present.

**Spec impact.** 2, 3.1, 4.1, 5.2, 5.4, 8.1, 8.2, 9.2, 10.3, Appendix A (rates shown at 24 and 56 tones), B.3.

- [x] Accept 17.3

### 17.4 Remote distortion report (4 bits in the ACK)

**Verdict.** Yes. The receiver can see transmit-side clipping and over-deviation; without a report the sender's only symptom is block errors, which the adaptation loop would chase with lower rungs instead of lower drive.

**How the receiver detects it.**

- Intermodulation energy on silent tones: power on masked tones, below 300 Hz and above the highest active tone, relative to active-tone power. A linear path shows only noise there; clipping raises it sharply.
- Amplitude-dependent error: at 16-QAM and above, outer constellation points carry more error than inner ones when the path compresses; a noise-limited path errs evenly.
- Over-deviation at the receiver's IF filter produces the same two signatures, so one report covers both causes.

| Field | Bits | Meaning |
| --- | --- | --- |
| Distortion index | 3 | 0 clean to 7 severe; intermod ratio quantized in 3 dB steps |
| Own ADC clipped | 1 | The reporting receiver's input clipped during the frame: its measurement is unreliable and its own RX gain is wrong, not the sender's drive |

**What the sender does with it.** Index 2 or higher for two consecutive frames: with host level control (17.1), lower output 1 dB per frame until the index clears, then hold; without it, show the operator a "reduce drive" warning. The adaptation loop ignores block errors from frames reported at index 3 or higher when deciding to step down, so distortion is fixed at the drive rather than hidden by a slower rung. The index is logged per frame.

**Spec impact.** 7.1, 10.1, 13.2 (the link event gains the index), B.2 (4 of the 12 reserved bits).

- [x] Accept 17.4

## 18. Change record: operator-experience additions (accepted 2026-10-29)

Thirteen conveniences, accepted on 2026-10-29 and folded into 8.4, 9.1, 9.2, 9.6, 12.1, 12.2, 12.4, 13.1–13.7, 14.1 and 16.2. The table is kept as the rationale record.

| # | Feature | What the operator gets | Protocol impact | Recommend |
| --- | --- | --- | --- | --- |
| 1 | Guided setup and self-test | Device and PTT selection, PTT test, level-setting tone, `calibrate` with a peer, saved per-radio profile | None | v1 |
| 2 | Rig profile library | Known TXDelay, access delay, port type and quirks by radio model, shipped and community-maintained; measured values offered back, opt-in | None | v1 |
| 3 | Link dashboard with a "why is it slow" panel | Per-tone SNR, constellation, rung and width per direction, distortion index, block errors, net rate and ETA; symptoms mapped to causes (clipping, TXDelay, mask collapse, low deviation) | None | v1 |
| 4 | Heard list | Every decoded header logged with time, SNR, width and frame type; `heard` API command and export | None | v1 |
| 5 | Link test with two-way report | `probe` returns both directions' SNR, usable width, suggested rung and expected net throughput in one short exchange | PROBE reply carries an ACK-type control block (B.2) and a width probe | v1 |
| 6 | Session resume | A session cut by link loss resumes within 10 minutes with its ID, sequence numbers and dictionaries; delivered blocks are not re-sent | CONNECT flag bit 1: resume; both ends keep session state for 10 minutes | v1 |
| 7 | Transfer progress | Bytes, rate, ETA and retransmission count per session in the API and UI | None | v1 |
| 8 | Unattended operation | Auto-accept with per-callsign allow and deny lists, scripting hooks, automatic periodic ID | None | v1 |
| 9 | Etiquette status | DCD state, who holds the channel, time to the next yield window, and why the modem is waiting | None | v1 |
| 10 | Diagnostic capture | Raw audio and decoder state saved on a failed frame or on request, opt-in; feeds bug reports and the conformance captures | None | v1 |
| 11 | Dictionary fetch over the air | A station missing a dictionary the peer offers requests it as a file; about a minute of air time, once | CONTROL PDU: dictionary request and offer | v1.x |
| 12 | Channel presets with CAT tuning | Presets bundle frequency, channel width, repeater mode and broadcast width; the modem tunes the radio when CAT is in use | None | v1.x |
| 13 | Reference chat and file client | Keyboard chat and file transfer with resume, exercising the native API; doubles as the interoperability harness | None (application layer) | v1.x |

**Link test (5).** PROBE names a station. The target replies with a SOUND-type frame carrying an ACK control block (its measurements of the PROBE: SNR, usable mask, suggested rung) and a width probe; the initiator reports the reverse direction from what it measured. Two transmissions, under 3 s of air time, and the result is shown as expected net kbps rather than raw figures.

**Session resume (6).** CONNECT with flag bit 1 and the old session ID. A responder still holding state answers CONNECT-ACK with its last acknowledged sequence in the base-sequence field, and transfer continues from there; otherwise it answers as a new session. State is dropped after 10 minutes or on DISCONNECT. Signed PDUs are unaffected because the replay counter continues.

- [x] Accept 1–4 and 7–10 (reference implementation, no air-interface change)
- [x] Accept 5 and 6 (small protocol additions)
- [x] Accept 11–13 for v1.x

## 19. Change record: compute, memory and storage additions (accepted 2026-10-29)

Fourteen candidates, accepted on 2026-10-29. The v1 and v1.x items are folded into 2, 3.1, 6.1, 6.3, 8.2, 8.4, 9.1, 10.1, 11.3, 11.5, 12.1, 12.4, 13.5–13.7, 14.1 and 16.2; the v2 items are scheduled in 16.2 with their design questions in 16.1. The table is kept as the rationale record.

| # | Feature | What it buys | Cost | Protocol impact | Recommend |
| --- | --- | --- | --- | --- | --- |
| 1 | Iterative receiver: decision-directed channel re-estimation and iterative demapping | 0.5–1.5 dB at 64- and 256-QAM, so one rung higher on many links | 2–3× decode compute, trivial on a Pi | None | v1.x (reference receiver) |
| 2 | Multi-hypothesis sync: both cyclic-prefix lengths and several sample-clock offsets tried in parallel | Receivers need no repeater-mode configuration; locks on weaker preambles | A few extra correlators | None | v0.5 (receive side; required of every receiver) |
| 3 | Soft combining of repeated broadcast frames | A bulletin repeated three times decodes about 4.8 dB below a single copy; repeats become useful instead of redundant | About 2 kB of stored soft decisions per failed block | Header flag: identical repeat of the same frame counter | v1.x |
| 4 | Rolling raw-audio buffer with offline re-decode | Frames that failed in real time get a second pass with heavier settings; captures become test vectors | 58 MB per hour at 8 kHz, 350 MB at 48 kHz; a day fits on any SD card | None | v1 |
| 5 | Per-link threshold calibration ("digital twin") | Rung thresholds computed from the measured channel instead of fixed values plus a blanket margin; recovers 1–2 dB on good radios | A one-second Monte Carlo at connect | None | v1.x |
| 6 | Audio-chain predistortion from the calibration sweep | Expands peaks before the radio's limiter compresses them: 1–2 dB more usable deviation on mic paths | A 16-step sweep and a small curve fit; B.5 as written allows at most 15 steps and CAL-REPORT carries 8, so the predistortion ADR must extend B.5 | None | v1.x experiment |
| 7 | Multi-mode receiver: 1200 AFSK, G3RUH 9600 and IL2P demodulators run alongside this mode | One TNC talks to every station on the channel; the heard list learns each station's mode and the transmitter answers in kind | A few percent of one core per demodulator | None on the air; a per-destination mode policy in the host | v1.x |
| 8 | SDR receive path (RTL-SDR class) | Monitors a dozen channels at once; a flat 56-tone receive path from any transceiver; RF-level carrier-to-noise for DCD and adaptation | Channelizer plus FM demod per channel, within a Pi 4 | None | v1.x |
| 9 | Cross-band full duplex (SDR receive on one band, transmit on the other) | Turnaround overhead disappears and symmetric throughput doubles; ACKs ride on the continuous reverse stream | Diplexer or separate antennas; a continuous-frame session mode | New session mode | v2 |
| 10 | Session multiplexing with priorities | A chat message gets through while a file transfer is running | A buffer per stream | One stream-ID byte in the PDU header | v1.x |
| 11 | Rateless (fountain-coded) broadcast for one-to-many distribution | Many stations with different links each decode a bulletin after hearing slightly more than its size, with no per-receiver ACKs; 40–60% less air time than repeating for the worst link | Decoding state about the object's size | RATELESS PDU type: object ID, size, seed, symbol index | v2; check LT and RaptorQ patent status first |
| 12 | Polled gateway mode (DAMA-style) | A gateway with many clients polls them in turn: no hidden-node collisions, no backoff waste | Registration state at the gateway | POLL frame type and registration | v2 |
| 13 | Link analytics and coverage maps | Long-term per-tone SNR and heard logs plotted against positions: a group's self-measured coverage and best time of day | Megabytes per month | None | v1.x |
| 14 | Propagation prediction with cached terrain (ITM/SPLAT-class) | "Which gateway can I reach from here" before keying up, validated against the modem's own measurements | Terrain tiles, hundreds of MB for a region | None | v2 |

**Soft combining (3).** A broadcast frame sent again with the same frame counter and header flag bit 5 set is bit-identical to the original. A receiver that failed on the original keeps its soft decisions, adds the repeat's, and decodes the sum; it discards stored decisions after 10 minutes or 64 frames.

**Multiplexing (10).** A stream byte after the PDU type carries a 2-bit priority and a 6-bit stream number: 64 concurrent streams per session at four priority levels. The sender interleaves streams at block boundaries, highest priority first, so a short PDU on a high-priority stream waits at most one block behind a bulk transfer.

**Considered and rejected.** Co-channel interference cancellation: FM capture means the stronger signal wins outright, so there is little to cancel. Longer LDPC blocks (8k–16k bits): under 0.5 dB gain and worse HARQ granularity. Machine-learned replacements for the detectors and equalizer: the handcrafted DSP is understood and testable; revisit only if Stage 2 data shows a specific failure. Overhearing caches and cooperative relay (a third station supplying blocks the destination missed) are real gains but belong to the network layer and the BBS, not the modem.

- [x] Accept 1, 2, 4, 5, 6, 7, 8 and 13 (no air-interface change)
- [x] Accept 3 and 10 (one header flag; one PDU byte)
- [x] Schedule 9, 11, 12 and 14 for v2 design

## 20. Decisions record (2026-11-05)

All fourteen open questions of 16.1 were decided on 2026-11-05. Each entry gives the decision, the reasoning, and where the normative text now lives. Entries marked "measure" keep a Stage 1 or Stage 2 measurement that can adjust a number within the stated range but does not reopen the decision.

### 20.1 Name and ports (Q1)

- Working name: **Cairn**. Short, CLI-safe (`cairn`, `cairn.conf`, `cairn.sock`), and apt: a trail marker built by everyone who passes, from stacked stones. Alternates held in reserve: Timberline, Talus. A collision check (GitHub, PyPI, crates.io, amateur mode lists, USPTO) is the first task in the repository and may force an alternate.
- Ports: KISS on TCP 8001, Dire Wolf's default, so existing clients connect unchanged; native API on TCP 8002 and the Unix socket `cairn.sock` in the runtime directory.

### 20.2 Error-correcting codes (Q2)

All three block codes come from the 5G NR LDPC base graphs (3GPP TS 38.212 §5.3.2), lifted and shortened:

| Block | Base graph | Lifting Z | Native k | Shortened to | Coded lengths used |
| --- | --- | --- | --- | --- | --- |
| Data | BG1 | 48 | 1,056 | 1,024 | 2,048 (1/2), 1,536 (2/3), 1,368 (3/4), 1,232 (5/6); rate-matching buffer 3,168 |
| Control | BG2 | 26 | 260 | 256 | 512 (1/2) |
| Header | BG2 | 15 | 150 | 144 | 288 (1/2); 432 (1/3) as contingency |

Reasoning: one decoder core for everything; rate compatibility is designed in, so incremental-redundancy HARQ sends the next unsent segment of the buffer; published performance curves; open implementations to cross-check against. Shortening fills the unused information bits with zeros known to both ends and never transmits them. Rejected: codec2's (2048, 1024), which has no designed puncturing and no rate compatibility; 802.11n, whose four codes have a different k per rate, breaking the fixed-size block. Deliverable at v0.2: the three lifted parity-check matrices and rate-matching tables as files, with known-answer encodings. Normative text: 6.3, A.3.

### 20.3 Preamble and detection (Q3) — measure

Symbols 1–2 are one constant-amplitude, low-PAPR sequence S1 on the base 24 tones, sent twice; symbols 3–4 are a distinct known sequence S2 (BPSK from a 24-bit PN segment). Detection is autocorrelation at the repeat lag, 11.25 ms for the short prefix and 12.5 ms for the long one, so the lag that peaks identifies the prefix; confirmation by cross-correlation with S1; fine timing and the channel estimate from S2. The normative requirement is performance, not a metric threshold: at least 99% detection at 6 dB audio SNR and at most one false alarm per 10 minutes on open-squelch noise. Measure: S1 is chosen by PAPR search in Stage 1 with Newman phases as the starting point; both sequences are published as test vectors at v0.2. Normative text: 6.1, 14.1.

### 20.4 No BPSK rung (Q4)

Closed. Above the FM threshold the audio SNR is already 12 dB or more against MCS 0's roughly 8 dB need, and level control (13.5) removes the under-deviation case that would have called for a slower rung. Contingency: if Stage 2 shows header losses above the threshold, the header code moves to rate 1/3 (432 coded bits, 10 symbols, +34 ms per frame). The contingency must be settled by Stage 2, before v1.0, because it is an incompatible on-air change (Section 15). Normative text: 6.2, 6.3.

### 20.5 Tone spacing (Q5)

100 Hz, closed. The cyclic prefix (1.25 or 2.5 ms, detected blind) is the dispersion knob, and the modem measures dispersion itself from the probe's per-tone phase. Stage 2 logs group-delay variation on three radio makes; the receiver sets the "use long prefix" ACK flag (bit 3, B.2) when the probe shows dispersion above about 1.25 ms, and the sender switches that direction only; repeater mode forces the long prefix regardless. Normative text: 5.2, 5.3.

### 20.6 Level targets (Q6) — measure

70% of maximum deviation is the manual fallback only; with calibration the target is 3 dB below the measured limiter knee, which adapts to each radio. Stage 2 measures knees on handhelds and mobiles and the gain from predistortion; if predistortion delivers, the calibrated target moves to 1.5 dB below the knee. The decision holds whatever the numbers are; only the margin moves, within 1.5 dB. Normative text: 7.1, 13.5.

### 20.7 Station identification (Q7)

An ID control block (type 5, B.6) carries the callsign in 7-bit ASCII, with the SSID and an optional grid locator, in CONNECT, DISCONNECT, every BEACON, and at least every 10 minutes within a session, piggybacked on the next frame (one control block, 135 ms). Reasoning: §97.119(b)(3) permits identification by a data emission using a specified digital code; ASCII is one under §97.309(a)(3), and §97.309(a)(4) allows any publicly documented technique to carry it. Fusion and D-STAR embed the callsign the same way. The packed header callsign remains for addressing; the ASCII block removes the argument. The optional CW or 1200-baud AX.25 ID stays available. Residual: a note to the ARRL regulatory desk for confirmation. Not legal advice. Normative text: 3.3, 8.4, 9.3, B.6.

### 20.8 Courtesy preamble rejected (Q8)

No HDLC courtesy preamble. A tone-based legacy DCD drops within its 100–300 ms hold once OFDM starts, so it would protect at most the first 300 ms of a 4 s frame; stations using COS defer already. Replacement policy: when legacy stations have been active within the last 10 minutes (the AFSK detector of 7.2 asserted, or a legacy frame decoded by a modem with legacy demodulators), cap frames at 2 s and halve p. Normative text: 11.3.

### 20.9 Dictionary governance (Q9) — measure

Dictionaries live in `dictionaries/` in the specification repository with `REGISTRY.md` listing ID, version, SHA-256, corpus description, reproducible training command (zstd version, parameters, seed) and license. Releases are signed by at least two keyholders (minisign or ssh-sig) with published keys, so no single maintainer can become the bottleneck. New dictionaries arrive by pull request with held-out measurements; the license is CC0; files are never deleted. First corpus: FEMA ICS form templates (public domain) and the project's own captured beacons, bulletins and broadcast traffic meant for redistribution, never private messages. ARRL bulletins and ARES material are excluded unless the rights holder grants permission (a CC0 waiver). Every source and its license goes in `REGISTRY.md` and is reviewed in the dictionary pull request. Measurement (Stage 1): 20% held out; ratios reported for 100–500 byte messages at 32, 64 and 112 kB dictionaries. Normative text: 12.2, 15.

### 20.10 AX.25 connected-mode proxy (Q10)

Yes, v1.x. The modem completes the AX.25 handshake with its host (SABM/UA), opens a native session to the far modem, carries I-frame payloads as AX25-CONNECTED PDUs (type 6) and acknowledges locally so the host's timers are satisfied; the far modem re-originates AX.25 to its host. End-to-end reliability comes from the native ARQ; disconnects propagate both ways; a far station last heard in a legacy mode gets pass-through in that mode instead. Reasoning: this is the satellite performance-enhancing-proxy pattern, the existing AX.25 library already holds the state machine, and it gives Winlink RMS Packet, BPQ32 and JNOS native speed with no changes. Normative text: 12.1, 13.1, 16.2.

### 20.11 Relay and digipeating (Q11)

Digipeating exists only for broadcast frames: a path control block (type 6, B.7) with up to two hops, WIDE-style aliases, and duplicate suppression by source and frame counter (v1.x). Session relay is a host application that runs two native sessions, addressed with a via entry in the path block of CONNECT (v2). Store-and-forward belongs to the BBS, not the modem. Normative text: 8.4, 9.1, B.7, 16.2.

### 20.12 Rateless-code patents (Q12)

The v2 rateless broadcast is designed around RaptorQ (RFC 6330). Qualcomm's disclosed patents carry dates from 2001 to 2007, so they expire between 2021 and 2027, and the IESG recorded that the disclosure does not preclude unencumbered implementation. Fallback, if a reading of the current IPR disclosure at design time says otherwise: plain LT codes with a robust soliton distribution. Not legal advice.

### 20.13 Full-duplex session mode, v2 parameters (Q13)

- Capability bit 8 in B.3; only between two stations on a cross-band pair (transmit on one band, receive on the other), each with an SDR or second receiver.
- Frames back-to-back with PTT held; every frame still carries its preamble and header, so a half-duplex monitor can decode it.
- Sliding window of 4 frames per direction; the ACK bitmap covers the window; an ACK at least once per frame or per 2 s.
- Flow control by credit in units of 8 blocks, carried in the ACK's reserved byte.
- Each band keeps its own DCD, CSMA and yield windows (11.2), and defers to legacy stations per band.

### 20.14 Polled gateway mode, v2 parameters (Q14)

- The gateway advertises polled mode in its BEACON: cycle length, poll-list size, and a contention window of three 150 ms slots for registration.
- A POLL frame (type 10) names one client; the client answers within its turnaround with DATA or ACK, subject to the per-frame limit of 7.3.
- Round-robin with aging: a client idle for three cycles is polled every fourth cycle until it sends.
- Clients revert to CSMA 60 s after the last POLL heard; the gateway still runs carrier sense before every POLL.
- Recommended only on a dedicated gateway channel.

## 21. Implementation plan

The order of work for the reference implementation, written so an engineer or an agent can start from the repository alone. Stage numbers match Section 14; every acceptance test below is a target from 14.1 or a number from Appendix A.

### 21.1 Repository layout

```
cairn/
  docs/SPEC.md         this document, exported; normative
  docs/                setup guide, rig profiles, API schema, implementation notes
  codes/               lifted NR parity-check matrices (BG1 Z=48, BG2 Z=26, BG2 Z=15), rate-matching tables, known-answer vectors
  preamble/            S1 and S2 as text and .wav test vectors
  dictionaries/        REGISTRY.md and the zstd dictionaries (20.9)
  vectors/             header, control and data block encodings; off-air captures with expected output
  sim/                 Stage 1 Python package: ldpc, ofdm, preamble, channel, link_sim, dict_eval, report
  modem/               real-time core (Rust): audio, dsp, codes, link, session, kiss, api, ptt, levels
  tools/               calibrate, capture, heard, conform, plots
```

### 21.2 Stage 1: simulation (Python, NumPy)

| Module | Scope | Acceptance |
| --- | --- | --- |
| `ldpc` | Load the three matrices; systematic encode with 38.212 rate matching; layered normalized min-sum decode, 30 iterations, early stop on CRC | Known-answer vectors round-trip; block error rate versus Eb/N0 within 0.3 dB of published NR curves at n = 2,048 |
| `ofdm` | Tone plan (5.2), symbol assembly with both prefixes, clip-and-filter PAPR (7.1), scrambler and interleaver (6.4), tone mask and power reassignment (5.4) | PAPR at or below each rung's clip level (7.1) on 10,000 random symbols; loopback through an ideal channel decodes every rung and every width error-free |
| `preamble` | S1 search (Newman phases, then random search for PAPR ≤ 3 dB), S2 generation, dual-lag detector with S1 cross-correlation and the sample-clock offset grid | ≥ 99% detection at 6 dB; ≤ 1 false alarm per 10 minutes of open-squelch noise; prefix identified correctly ≥ 99.9% |
| `channel` | The 14.2 chain: pre-emphasis, mic band-pass and limiter, deviation clipping, AWGN at a set audio SNR, receive noise tilt, de-emphasis, receive band-pass, sample-clock offset, DC and hum; the repeater cascade | Each stage unit-tested against its analytic expectation (limiter knee, noise-tilt slope, group delay) |
| `link_sim` | Header, control and data framing; per-rung block error rate versus SNR; net throughput versus TXDelay and frame length; width negotiation on a roll-off channel | Rung thresholds within 3 dB of the 6.2 table; the 7.4 worked example reproduced within 5% |
| `dict_eval` | Corpus loader, zstd dictionary training, held-out ratios (20.9) | Ratios reported for 100–500 byte messages at 32, 64 and 112 kB |

Deliverable: `sim/report.md` with the curves and the v0.2 parameter table; `codes/`, `preamble/` and `vectors/` populated. Nothing in Stage 2 starts before the `link_sim` acceptance passes.

### 21.3 Stage 2: real-time modem (Rust core, Python tooling)

Build order, each step with its own tests before the next:

1. Audio I/O at 48 kHz (cpal or PortAudio), ring buffers, no allocation in the audio callback.
2. PTT backends: serial RTS/DTR, CM108 HID, GPIO, Hamlib; TXDelay and TXTail timing verified with a scope or a second radio.
3. Receiver pipeline: continuous preamble search, demodulation, decoding, DCD per 7.2, heard list from every decoded header.
4. Transmitter: frame assembly from Appendix B, level-setting tone, PAPR control.
5. Link layer: broadcast first; then sessions with selective repeat; then resume, link test and the ID cadence.
6. KISS server on 8001; native API on 8002 with the JSON-lines schema in `docs/api.md`.
7. Level control (13.5): receive auto-gain, stepped-tone calibration, distortion report handling.
8. Diagnostic capture, raw-audio ring buffer, dashboard data.

Acceptance: the 14.1 24-tone targets on two radio makes, and every on-air behavior backed by a vector in `vectors/`. Rules for the core: every algorithm parameter comes from one `params.rs` that mirrors Appendix A; constants are generated from the specification tables where possible; a change that alters on-air behavior needs a test vector in the same commit.

### 21.4 Stage 3 and v1.0

On-air tests per 14.4; a second implementation (a Python port of the core is acceptable) for interoperability; the conformance runner `tools/conform` replays `vectors/` and compares; 1 MB each way error-free on air closes v1.0.

### 21.5 Working rules

- The specification is normative. Code that disagrees with it is either a bug or a specification change with a test vector; never a silent divergence.
- Log per-frame SNR, rung, width, distortion index and block error rate from day one; the dashboard and the analytics are built on that log, not retrofitted.
- Keep the v0.5 and v1.0 scope lines firm (16.2). The v1.x features are documented so they can be built later without redesign, not so they can be pulled forward.

## Appendix A: profile parameters and rate table

Derived values at the two width extremes, 24 and 56 tones (shown as 24 / 56), assuming 4.0 s frames (2.0 s for rungs 0–1 per Section 10.2), TXDelay 150 ms, and a peer ACK cycle of 0.46 s at either width. Net rates are one-direction payload after all overhead; compare with the raw rates in Section 6.2.

### A.1 Fixed per-frame costs

| Item | 24 tones | 56 tones |
| --- | --- | --- |
| Preamble | 4 symbols, 45 ms | 4 symbols, 45 ms |
| Header (288 coded bits at MCS 0, base tones) | 7 symbols, 79 ms | 7 symbols, 79 ms |
| Control block (512 coded bits at MCS 0, base tones) | 12 symbols, 135 ms | 12 symbols, 135 ms |
| Width probe (CONNECT, CONNECT-ACK, SOUND, re-probe) | 2 symbols, 22.5 ms | 2 symbols, 22.5 ms |
| ACK frame on air (preamble + header + 1 control block) | 259 ms | 259 ms |
| Block symbols available in a 4 s frame | 330 | 330 |

### A.2 Per-rung block geometry and net throughput

| MCS | Coded bits per symbol 24 / 56 | Symbols per data block 24 / 56 | Data blocks per frame 24 / 56 | Net kbps 24 / 56 |
| --- | --- | --- | --- | --- |
| 0 | 44 / 104 | 46.5 / 19.7 | 3 / 7 (2 s frame) | 1.2 / 2.9 |
| 1 | 44 / 104 | 31.1 / 13.2 | 4 / 11 (2 s frame) | 1.6 / 4.5 |
| 2 | 88 / 208 | 23.3 / 9.8 | 14 / 33 | 3.2 / 7.5 |
| 3 | 88 / 208 | 15.5 / 6.6 | 21 / 50 | 4.8 / 11.3 |
| 4 | 132 / 312 | 11.6 / 4.9 | 28 / 67 | 6.3 / 15.1 |
| 5 | 132 / 312 | 9.3 / 3.9 | 35 / 83 | 7.9 / 18.8 |
| 6 | 176 / 416 | 7.8 / 3.3 | 42 / 100 | 9.5 / 22.6 |

Each data block carries 126 payload bytes (1,008 bits). Net kbps = blocks × 1,008 ÷ cycle time: 4.46 s for 4 s frames and 2.46 s for the 2 s frames of rungs 0–1, at either width, since the header and control block use the base tones. Shorter frames reduce every net figure; Section 7.4 shows the 2 s case.

### A.3 Code lengths by rung

| Code rate | Coded block length (bits) | Used at MCS |
| --- | --- | --- |
| 1/2 | 2,048 | 0, 2 |
| 2/3 | 1,536 | 4 |
| 3/4 | 1,368 | 1, 3, 6 |
| 5/6 | 1,232 | 5 |
| 1/3 (HARQ only) | up to 3,168 | Reached by sending further buffer segments on retransmission (9.4) |

All four lengths are prefixes of the same 3,168-bit rate-matching buffer of the 5G NR BG1 code lifted at Z = 48 (k = 1,056, shortened to 1,024), so a retransmission can send the next unsent segment of the buffer (hybrid ARQ, 9.4) down to rate 1/3. The lifted parity-check matrices for all three codes and the rate-matching tables are published as files at v0.2 (decision 20.2).

## Appendix B: field layouts

Bit layouts for the two control blocks and the address format. The header layout is in Section 8.2. All fields are big-endian; reserved bits are zero. Every CRC-16 field is CRC-16/CCITT-FALSE (6.3) over the preceding bits of its block. Control types 2 and 7–15 are reserved: senders MUST NOT use them, and receivers MUST skip a control block of unknown type and process the rest of the frame (every control block is 256 bits, so the next block starts at a known position).

### B.1 Address format (40 bits)

| Code | Character |
| --- | --- |
| 0 | padding (trailing positions only) |
| 1–26 | A–Z |
| 27–36 | 0–9 |
| 37 | / |
| 38 | - |
| 39–63 | reserved |

Six characters at 6 bits each (first character in the most significant bits) followed by a 4-bit SSID. All 40 bits set = broadcast. A callsign longer than six characters sets the reserved code 63 in position one and carries a 30-bit hash in the remaining bits (first 30 bits of SHA-256 over the upper-case ASCII callsign text of B.6, with no SSID or padding; an all-ones result becomes all-ones minus one); the full text, at most 10 characters, travels in the ID control block (B.6) sent in CONNECT.

### B.2 ACK control block (256 bits)

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 0 = ACK |
| Flags | 4 | Bit 0: tone mask present. Bit 1: reserved for HARQ parity-only retransmission (planned, 9.4; send zero until specified). Bit 2: re-probe requested (send a width probe in your next frame). Bit 3: use long prefix (the receiver measured dispersion above about 1.25 ms from the probe's per-tone phase; the sender switches to the 2.5 ms prefix in this direction only, 5.2). |
| Acknowledged base sequence | 16 | Sequence number of the peer's last frame |
| Block bitmap | 128 | Bit i set = block base+i passed its CRC |
| Requested MCS | 4 | Rung the sender should use next |
| Measured SNR | 8 | 0.25 dB steps, 0–63.75 dB (saturating), channel SNR over active tones excluding the designed clipping, 7 dB-equivalent (7.1, ADR 0006) |
| Block errors in last frame | 8 | Count, for the peer's hysteresis logic |
| Tone mask | 56 | Usable tones 0–55; valid when flag bit 0 set |
| Distortion index | 3 | 0 clean to 7 severe: transmit-side clipping or over-deviation as seen at this receiver, beyond what clip-and-filter at the frame's rung produces by design (7.1), from intermodulation energy on silent tones and amplitude-dependent constellation error, in 3 dB steps |
| Own ADC clipped | 1 | This receiver's input clipped during the frame: its measurements are unreliable and its own receive gain is wrong, not the sender's drive |
| Reserved | 8 |  |
| CRC-16 | 16 |  |

### B.3 CONNECT / CONNECT-ACK control block (256 bits)

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 1 = connect parameters |
| Flags | 4 | Bit 0: a second control block with more dictionary IDs follows. Bit 1: resume the session named by the session ID (9.2). Bits 2–3 reserved. |
| Session ID | 16 | Chosen by initiator; echoed by responder; on resume, the ID of the interrupted session |
| Capabilities | 16 | Bit 0 widths above 24 tones, bit 1 reserved for HARQ (planned, 9.4), bit 2 signatures, bit 3 reserved for schema encoding (planned, 12.2), bit 4 reserved for short session headers (v2), bit 5 level calibration (accepts CAL-REQUEST), bit 6 stream multiplexing, bit 7 dictionary fetch, bits 8–15 reserved |
| Width cap | 6 | Highest tone index this station will transmit (23–55), set by its channel width |
| Requested MCS | 4 | Rung the sender wants to receive at |
| Max frame time | 6 | Units of 0.1 s, 5–40 (0.5–4.0 s). Each station advertises its configured maximum; frames in the session never exceed the lower of the two and the per-rung limit of 10.2 |
| Tone mask | 56 | As B.2: in CONNECT-ACK, usable tones for the initiator-to-responder direction; in CONNECT, all ones |
| Dictionary IDs | 128 | Up to four 32-bit IDs; zero = empty slot |
| CRC-16 | 16 |  |

### B.4 Data block (1,024 information bits)

126 payload bytes followed by a CRC-16/CCITT-FALSE (6.3) over those bytes. Payload bytes are a slice of the session's PDU stream; block boundaries carry no meaning of their own.

### B.5 Calibration control blocks (256 bits)

CAL-REQUEST (control type 3), carried in a CAL-REQUEST frame; the tone bursts of 13.5 follow the frame.

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 3 |
| Flags | 4 | Reserved |
| Tone frequency | 8 | Units of 10 Hz; default 100 (1000 Hz) |
| Step count | 4 | Default 8 |
| Step length | 8 | Units of 10 ms; default 40 (400 ms) |
| Gap length | 8 | Units of 10 ms; default 10 (100 ms) |
| Start level | 8 | dB below the modem's nominal output, 0.25 dB steps; default 96 (24 dB down) |
| Step size | 4 | dB per step; default 3 |
| Reserved | 192 |  |
| CRC-16 | 16 |  |

CAL-REPORT (control type 4), returned by the peer: in a session, in its next ACK or DATA frame (after the ACK control block); outside a session, in a UI frame addressed to the initiator.

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 4 |
| Flags | 4 | Bit 0: sweep valid (every step heard). Bits 1–3 reserved. |
| Received amplitude, steps 1–8 | 64 | 8 × 8 bits, dB below full scale in 0.25 dB steps |
| Harmonic distortion, steps 1–8 | 32 | 8 × 4 bits, percent: energy at 2× and 3× the tone frequency relative to the tone |
| Receiver clipped, steps 1–8 | 8 | 1 bit per step |
| Reserved | 128 |  |
| CRC-16 | 16 |  |

### B.6 ID control block (256 bits)

Control type 5. Sent in CONNECT, DISCONNECT, every BEACON, and at least every 10 minutes within a session (decision 20.7).

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 5 |
| Flags | 4 | Bit 0: grid locator present. Bits 1–3 reserved. |
| Callsign | 80 | Ten 8-bit characters, 7-bit ASCII with the high bit zero, upper case, space-padded; includes any indicator the operator must send (for example a /7 or a self-assigned suffix) |
| SSID | 4 | As in the header address |
| Grid locator | 48 | Six 8-bit ASCII characters (Maidenhead), valid when flag bit 0 set |
| Reserved | 100 |  |
| CRC-16 | 16 |  |

### B.7 Path control block (256 bits, v1.x)

Control type 6. Carried by BEACON and UI frames that request digipeating (9.1), and by CONNECT for session relay in v2 (20.11).

| Field | Bits | Meaning |
| --- | --- | --- |
| Control type | 4 | 6 |
| Flags | 4 | Bit 0: entry 1 has been used. Bit 1: entry 2 has been used. Bit 2: via (session relay, v2). Bit 3 reserved. |
| Hops remaining | 2 | Decremented by each relay; a frame with 0 is not relayed |
| Entry count | 2 | 0–2 |
| Entry 1 | 40 | Relay callsign and SSID in the B.1 format, or a WIDE-style alias (for example WIDE1 with SSID 1) |
| Entry 2 | 40 | As entry 1 |
| Reserved | 148 |  |
| CRC-16 | 16 |  |

A relay whose callsign or alias matches the first unused entry writes its own callsign into that entry, sets the used flag, decrements hops remaining, and retransmits the frame once after the channel-access rules of 11.1. Duplicates are suppressed by source and frame counter for 60 s.

## Appendix C: references and prior art

Linked entries were consulted for this draft; the rest are named for follow-up and should be opened before v0.2.

**Existing modes**

- VARA FM (EA5HVK): closed OFDM modem, the throughput benchmark. [Overview and speed figures](https://hamradio.my/?p=11653); [SignaLink notes on WIDE versus NARROW paths](https://tigertronics.com/sl_suprt.htm); [Winlink note on VARA FM and packet sharing a channel](https://winlink.org/sites/default/files/RMSE_FORMS/rms_packet_dual_mode_single_radio_setup_0_0.pdf).
- codec2 / FreeDV data modes: open OFDM and FSK modems with LDPC, raw-data API, and the 2400B mode designed to pass through commodity FM radios. [README\_data](https://github.com/drowe67/codec2/blob/main/README_data.md).
- FreeDATA: open ARQ platform on codec2 data modes, mainly HF; reference for the ARQ layer. [Project wiki](https://github.com/DJ2LS/FreeDATA/wiki).
- IL2P (Improved Layer 2 Protocol, Nino Carrillo): Reed–Solomon FEC and header compression over existing FM modems, as implemented in NinoTNC and Dire Wolf.
- M17 Project specification: open 4FSK voice and packet mode for direct FM modulation.
- Dire Wolf (WB2OSZ): software TNC; its DCD and PSK modems are useful comparison points.
- AX.25 Link Access Protocol v2.2 (TAPR) and the KISS TNC protocol (Chepponis and Karn).

**Techniques**

- 3GPP TS 38.212 §5.3.2: 5G NR LDPC base graphs, lifting sizes and rate matching; the selected code family (decision 20.2).
- 3GPP CQI/MCS tables and HARQ procedures as the reference design for link adaptation.
- Schmidl and Cox, "Robust frequency and timing synchronization for OFDM" (repeated-preamble detection).
- Zstandard (RFC 8878) and its dictionary-training tool; the Compression Dictionary Transport work in the IETF for shared-dictionary negotiation.
- RFC 6330 (RaptorQ) and Qualcomm's IPR disclosures for it: [disclosure #1553](https://datatracker.ietf.org/ipr/1553/), [IESG document history noting unencumbered implementation](https://datatracker.ietf.org/doc/draft-ietf-rmt-bb-fec-raptorq/history/) (decision 20.12).
- "Language Modeling Is Compression" (DeepMind, 2023) for the v2+ model-based compression stretch goal.

**Regulatory**

- 47 CFR §97.307(f) as currently published: [govinfo](https://www.govinfo.gov/content/pkg/CFR-2024-title47-vol5/pdf/CFR-2024-title47-vol5-sec97-307.pdf).
- 47 CFR §97.119, station identification: [Cornell LII](https://www.law.cornell.edu/cfr/text/47/97.119); the §97.309 specified-code reasoning as applied to embedded callsigns: [KB9MWR](https://www.qsl.net/k/kb9mwr/projects/wireless/pingid.html) (decision 20.7).
- FCC 23-93 Report and Order and Further Notice (November 2023): HF symbol-rate limits replaced by a 2.8 kHz bandwidth limit; VHF/UHF removal proposed. [FCC document](https://docs.fcc.gov/public/attachments/DOC-397992A1.pdf); [ARRL summary of its comments on the VHF/UHF proposal](https://www.arrl.org/news/view/arrl-responds-to-fcc-proposals).
- 47 CFR §97.309(b), §97.113(a)(4), §97.221, and petition RM-11831 (the Winlink-era argument over compressed traffic as effective encryption).
