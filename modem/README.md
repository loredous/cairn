# Cairn modem (Stage 2, Rust)

The real-time modem of SPEC §21.3. Done so far: the physical layer, ported
from the Stage 1 simulator (`sim/cairn_sim/`) and checked bit for bit
against the published vectors, and the real-time layer around it (steps
1-4 without the link layer): audio I/O at 48 kHz, PTT backends with
measured TXDelay and TXTail, a transmitter that sends frames with real
headers and control blocks, and a continuous receiver that decodes each
frame header first within the 100 ms turnaround of SPEC 7.3, in the
`cairn-modem` command. That is enough for a one-way broadcast test between
two radios (below). The link layer and the host interfaces come next.

```sh
cd modem
cargo test                                   # vectors, fixtures, loopback, spec tables, mock audio/PTT
cargo clippy --all-targets -- -D warnings
cargo run --release --example bench          # single-core timing of 4 s frames
cargo build --release -p cairn-modem         # target/release/cairn-modem
```

Building needs the ALSA headers (`libasound2-dev` on Debian/Ubuntu,
`alsa-lib` on Arch) and `pkg-config`.

## Crates

| Crate | Directory | Contents |
| --- | --- | --- |
| `cairn-phy` | `phy/` | Waveform, codes, bit mapping, preamble detector, receiver (below) |
| `cairn-audio` | `audio/` | cpal streams at 48 kHz chosen by name, lock-free rings, xrun counters, timing of every output sample, input level meter |
| `cairn-ptt` | `ptt/` | PTT trait and backends: serial RTS/DTR, CM108 HID, Hamlib `rigctld`, VOX/none; a drop guard and a shared handle for signal handlers |
| `cairn-modem` | `cli/` | Transmit sequencing, the listener, test frames and the level-setting tone (library), and the `cairn-modem` binary |

## The `cairn-modem` command

```sh
M=target/release/cairn-modem
$M devices                                   # every ALSA device; 48k = can run at 48 kHz
$M levels -d All-In-One-Cable --seconds 5    # input RMS (about the mean, and with DC), peak, DC, CLIP/HOT, xruns
$M ptt-test --ptt serial:All-In-One-Cable --seconds 1      # refuses more than 5 s without --force
$M tone -d All-In-One-Cable --ptt serial:All-In-One-Cable --seconds 3   # SPEC 7.1 level-setting tone
$M tx -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 --mcs 2 --blocks 3
$M listen -d All-In-One-Cable                               # header first; Ctrl-C to stop
```

- **Devices** are chosen by a substring of the ALSA name or its
  description, ignoring case and punctuation (`All-In-One-Cable` matches
  `plughw:CARD=3,DEV=0`, whose description is "All-In-One-Cable, USB
  Audio"), never by index. Among several matches the `plughw:` device
  wins, then `hw:`. `CAIRN_DEVICE` sets the default.
- **PTT** (`--ptt`, or `CAIRN_PTT`): `none` (default), `vox`,
  `serial:PATH` (a path, or a substring of a `/dev/serial/by-id/` name),
  `cm108:/dev/hidrawN` (`--ptt-gpio`, default 3), `rigctld[:HOST[:PORT]]`.
  Serial keys with `--ptt-line dtr` (default) or `rts`, active high unless
  `--ptt-invert`; the other line is held low. That is the AIOC's wiring
  (DTR high and RTS low keys it). GPIO is not implemented yet.
- **Timing**: `--txdelay` (ms, default 150) from keying to the first
  sample at the DAC, `--txtail` (ms, default 10) from the last sample at
  the DAC to release (SPEC 7.3). Each transmission prints both as
  measured.
- **`tx`** builds frames from SPEC 8.2 header fields with `cairn-phy`:
  `--callsign CALL[-SSID]` (the source, `CAIRN_CALLSIGN`; required),
  `--to CALL[-SSID]` (default broadcast), `--type ui|beacon|data|probe`
  (default UI, the broadcast data frame of SPEC 8.4), `--seq N` (a
  broadcast's frame counter, +1 per frame; a session frame's first block
  sequence, + its block count per frame), `--id` and `--grid` (the B.6 ID
  control block; a BEACON always carries it), `--message TEXT` (the
  payload, zero-padded; default the seed's random payloads). Flags follow
  from the frame: control and data present from the counts, session for
  `--type data` (which also carries an ACK control block first, SPEC 8.4,
  with demonstration values), and the probe whenever the data uses a tone
  above 23 (ADR 0007), to the width for a broadcast and to `--cap` for a
  session frame. Then `--mcs`, `--width`, `--prefix short|long`, `--mask
  8-11,30` (session frames only: a broadcast is unmasked), `--blocks N`
  or `--frame-s S` (default SPEC 10.2: 2 s at MCS 0-1, 4 s above, the
  block count as `cairn-bench tx` computes it, less the control blocks and
  the probe), `--count`, `--gap` (unkeyed seconds between transmissions),
  `--tone S` (the level-setting tone first), `--seed`. Each frame is its
  own keyed transmission, peak at -3 dBFS (`TxFrame::output`).
  `--random-header` sends the old frames instead: header and payload bits
  are the simulator's random ones for the seed (`frame.build(cfg,
  default_rng(seed * 1000 + i))`, NumPy's generator reimplemented bit for
  bit), for `listen --fixed` and `cairn-bench`.
- **`--wav out.wav`** (tx and tone) writes the signal instead of playing
  it, with a `cairn-bench/1` manifest next to it (`out.json`), so
  `cairn-bench rx out.wav` scores it exactly as its own recordings,
  header, control and block bits included (the manifest carries a real
  header's bits).
- **`listen`** decodes every frame header first: the header once the
  preamble and header are in, then the frame its fields describe. Session
  frames (flag bit 3) use the link state on the command line, `--width`,
  `--mask` and `--peer-cap` (the peer's cap in tones, the extent of its
  probe; default the width); a frame outside a session takes its width
  from its own probe (ADR 0007), or is 24 tones without one. It prints one
  line per frame: time (UTC and stream seconds), source > destination
  (`*` for broadcast), frame type and flags, sequence, MCS and width
  ("from probe" for a broadcast), prefix, data and control blocks OK/total,
  the ADR 0006 channel SNR on the 7 dB scale, sample-clock offset in ppm as
  `cairn-bench rx` reports it, detector rho, decode time; then one line per
  control block (`ctrl 1: ID N0CALL-1 grid FN31`, `ctrl 1: ACK base ...`)
  and, with `--text`, the payload. Every `--stats` seconds: the input
  level, capture counters, queue depth and totals. `--wav in.wav` runs the
  same code on a file as fast as it goes (`--realtime`: at real-time pace,
  reporting the receive turnaround); `--record cap.wav` saves what the
  sound card captured; `--expect N` fails unless exactly N frames decode
  completely (for scripts); `--threads` (default 4) decodes data blocks in
  parallel. `--fixed` takes the whole configuration from
  `--mcs/--width/--blocks/--mask` instead (the prefix from the detector
  unless `--prefix` forces it), for `--random-header` frames.
- Every keyed transmission over 5 s needs `--force` (and 300 s is the
  hard limit).

### How it works

- **Audio** (`cairn-audio`): the callbacks only convert samples
  (i16 or f32, any channel count; input takes the first channel) and move
  them through preallocated `rtrb` rings, plus atomics: no allocation,
  locking or I/O. Counted: ALSA xruns, input ring overflows, gaps in the
  capture timestamps (samples lost upstream whether or not ALSA said so),
  output underruns during a transmission. The output stream runs
  continuously (silence when idle) and reports each callback's DAC time
  (`OutputCallbackInfo` playback timestamp), so the time any queued sample
  plays is known. Default period 480 frames (10 ms); output latency on the
  AIOC is 25 ms.
- **Transmit** (`txseq::transmit`): key; wait for the next output callback
  (at most one period) to learn when a sample queued now will play; queue
  exactly enough silence to put the first audio sample at key + TXDelay;
  feed the audio; wait until the callback has taken the last sample, then
  until its DAC time plus TXTail; unkey. A `PttGuard` releases on return,
  error and panic; Ctrl-C, SIGTERM and SIGHUP set a stop flag and release
  the PTT at once from the signal thread (a second signal, or 3 s without
  a clean exit, exits the process after releasing again); a panic hook
  releases too.
- **Serial PTT** opens the port with `open(2)` and touches only the
  modem-control lines, both at once with one `TIOCMSET` (no termios
  changes, so no baud-rate side effects). Linux raises DTR and RTS
  together inside `open(2)`; the very next call drops both, and DTR+RTS
  high does not key the AIOC. HUPCL is made sure of for active-high lines,
  so the kernel drops DTR if the process dies without cleanup.
- **Listen** (`listen::run`): the processing thread takes 10 ms chunks
  (the sound card's period; sound card or file, the only difference) and
  feeds the incremental detector (`detector::Detector`: the front-end FIR,
  the autocorrelation plateau and the S1 correlations computed only for the
  new samples, a position decided once its +-3 symbol peak window is
  complete, so a detection is final 85 ms after the frame's first sample
  at any chunk size, with the same starts as the one-shot scan). Each
  detection is resolved by a `ConfigSource` as soon as enough of the frame
  is in: `HeaderFirst` decodes the header after preamble plus header
  (11 symbols) and, for a wide broadcast, measures the probe's extent once
  the data symbols every width has are in (`rx::extent_samples`), which is
  never later than the frame's end; `FixedConfig` takes the command line.
  The frame goes to the decoder thread the moment its last sample (plus
  1e-4 of its length for a clock offset) has arrived, and the decoder
  spreads the data blocks over `--threads` threads.
- **Full speed on wake-up** (`boost`): between frames the decoder sleeps
  and an idle core drops to its lowest clock (600 MHz here under
  schedutil), possibly an efficiency core; a frame that decodes in 8 ms on
  a busy performance core then took 30-50 ms. The decoder thread asks the
  kernel for a utilisation clamp (`sched_setattr`, uclamp min 1024, no
  privileges needed), so it wakes on a fast core at full clock; its block
  threads inherit it. `--no-boost` turns it off.
- **Receive turnaround** (SPEC 7.3: at most 100 ms from a frame's last
  sample to its decoded result), measured with `modem/tools/turnaround.sh`:
  10 frames per row with real headers and SPEC 10.2 lengths (2 s at
  MCS 0, 4 s above), `listen --wav --realtime` (the file played at
  real-time pace in 10 ms chunks through the same code as the sound card),
  on this 8-core Apple M2 (Asahi Linux), 4 decoder threads. "FM" is the
  simulator's FmChannel 3 dB above the rung's threshold (mic path at 24
  tones, flat25 over the width at 56), +20 ppm. The time includes the
  chunk wait (up to 10 ms), the clock-offset margin, the decode with the
  ADR 0006 rebuild, and the worker's wake-up.

  | Width | MCS | Blocks | Clean p50 / p99 (ms) | FM p50 / p99 (ms) | Without the clamp, clean / FM p50 (ms) |
  | --- | --- | --- | --- | --- | --- |
  | 24 | 0 | 3 | 10.1 / 15.4 | 8.7 / 14.9 | 34.1 / 33.4 |
  | 24 | 2 | 14 | 16.4 / 18.9 | 16.9 / 20.1 | 50.8 / 58.1 |
  | 24 | 4 | 28 | 16.9 / 19.7 | 17.5 / 20.5 | 58.5 / 51.9 |
  | 24 | 6 | 42 | 17.3 / 20.0 | 18.0 / 21.1 | 52.8 / 52.8 |
  | 56 | 0 | 7 | 15.5 / 24.2 | 15.1 / 20.3 | 36.2 / 45.0 |
  | 56 | 2 | 33 | 23.7 / 26.8 | 25.8 / 28.3 | 68.0 / 67.7 |
  | 56 | 4 | 66 | 23.4 / 31.6 | 25.6 / 29.7 | 64.3 / 69.3 |
  | 56 | 6 | 99 | 27.0 / 30.1 | 22.9 / 27.9 | 69.0 / 73.2 |

  The worst of all 160 frames was 31.6 ms with the clamp (85 ms without).
  Every frame was detected and its header decoded; every block decoded
  except one of 420 at 24 tones MCS 6 through FmChannel and, at 56 tones
  MCS 6, 9 of 990 clean and 3 of 990 through FmChannel
  (that rung shows a block error floor near 1% at 56 tones even noise-free,
  in both receivers; its threshold was measured at 24 tones). Decode
  alone (`cargo run --release --example rx_cost`, warm core) is 4-10 ms at
  24 tones and 4-16 ms at 56, about two thirds of it the ADR 0006 rebuild;
  4 block threads change little 3 dB above threshold, where the LDPC
  decoder stops after a few iterations, and help near threshold.

- **Idle cost.** The incremental detector costs 1.9% of one core at 10 ms
  chunks (`cargo run --release --example detector_cost`; the rescanning
  one cost 5.1% at that chunk size, 1.1% at 50 ms). Live on the AIOC at
  idle the whole `listen` process used 7-13% of one core in 120-300 s
  runs (CPU time at the idle cores' low clock, which inflates it), with no
  detection; 0 xruns on a quiet machine, one 25 ms capture gap in 300 s
  while four compile jobs and the test suites ran alongside.

### Two-radio broadcast test

One station transmits, the other listens; nothing is acknowledged. The
full procedure, with level setting and what to vary, is in
[docs/bench-testing.md](../docs/bench-testing.md#two-radio-broadcast-test-with-cairn-modem).

```sh
M=target/release/cairn-modem
# Levels: A sends the tone (drive to 70% deviation); B sets its input so it peaks near -10 dBFS.
$M tone -d All-In-One-Cable --ptt serial:All-In-One-Cable --seconds 3            # station A
$M levels -d All-In-One-Cable --seconds 10                                       # station B
# B listens: header first, broadcast widths from the probe; --record keeps the audio.
$M listen -d All-In-One-Cable --text --record rx.wav                             # station B
# A sends UI broadcasts with its callsign in every header and the ID block (B.6).
$M tx -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 --id --grid FN31 \
    --mcs 0 --count 5 --gap 2                                                    # station A
$M tx ... --callsign N0CALL-1 --mcs 2 --count 5 --message "test de N0CALL"       # station A
```

Station identification is the source callsign in every header (SPEC 8.3)
and, with `--id` or `--type beacon`, the ID control block; add a CW or
voice ID between runs where your rules ask for one. B prints each frame
with its source, type, counter, rung, width, blocks, control blocks
(`ctrl 1: ID N0CALL-1 grid FN31`), SNR and clock offset; `listen --wav
rx.wav --realtime` replays the recording through the same code and
reports the receive turnaround.

### Testbed notes (AIOC, no radio attached)

- `devices` lists it as `plughw:CARD=3,DEV=0` / `hw:CARD=3,DEV=0`
  ("All-In-One-Cable, USB Audio"), native i16 mono at 48 kHz both ways.
- Idle input: RMS about the mean -71.5 dBFS, DC -0.0047 (-46.6 dBFS,
  which dominates the -46.6 dBFS RMS with DC), peak -44 dBFS; no xruns or
  gaps.
- Output-to-input crosstalk inside the cable is about 70 dB down: the
  detector still finds the preambles of a transmission (rho 0.85), but the
  frames do not decode (SNR about -3 dB). A real loop needs a radio or a
  cable.
- Measured on the DAC timestamps: TXDelay 150.0 ms for 150 ms set, TXTail
  10.2-10.4 ms for 10 ms set (sleep granularity); not yet checked with a
  scope or a second radio (SPEC 21.3 step 2).

### Hardware and cross-checks

```sh
# Opt-in hardware tests (ignored by cargo test and in CI); keys the PTT for under a second
CAIRN_TEST_DEVICE=All-In-One-Cable CAIRN_TEST_PTT=serial:All-In-One-Cable \
    cargo test --release -p cairn-modem --test hardware -- --ignored --test-threads 1
# File-based end to end, from the repository root: tx --wav -> listen --wav; through the
# simulator's FmChannel (modem/tools/fm_channel_wav.py) -> listen; cairn-bench rx on the WAV
bash modem/tools/e2e_wav.sh
```

## Physical layer

One library crate, `phy/` (`cairn-phy`):


| Module | Contents |
| --- | --- |
| `params` | Every protocol number (SPEC 5-8, Appendix A, ADR 0003 clip table); `tests/spec_tables.rs` parses the SPEC tables and checks them |
| `crc` | CRC-16/CCITT-FALSE |
| `ldpc` | The three NR codes from `codes/*_lifted.txt`: systematic encoder, 38.212 rate matching, the Stage 1 layered normalized min-sum decoder (f32, alpha 0.75, 30 iterations, early stop on CRC and syndrome) |
| `gold` | TS 38.211 Gold sequence; scrambler, S2, pilot and probe sequences |
| `interleave` | ADR 0005 linear block interleaver |
| `mapping` | 38.211 Gray labeling, max-log demapper |
| `ofdm` | Half-bin OFDM synthesis (negated-tail prefix) and demodulation, clip-and-filter, PAPR, peak scaling |
| `preamble` | S1, S2 and the preamble grid |
| `detector` | SPEC 6.1 dual-lag detector with S1 cross-correlation and the ±100 ppm grid: one-shot (`scan`, `detect`), incremental streaming (`Detector`) and the older rescanning streamer (`RescanDetector`) |
| `fields` | SPEC 8.2 header, B.1 addresses (with the long-callsign SHA-256 hash) and every Appendix B control block: pack, unpack, CRC and field checks |
| `frame` | Frame geometry: segments (header, probe, control, data), tones, masks, gains (ADR 0004, 0005); the ADR 0007 probe rule and the probe's extent (`cap`); `LinkState` and `FrameConfig::from_header` |
| `tx` | Frame builder, mirroring `frame.build`; `build_frame` from a header and control blocks (sets the probe flag when the data needs it) |
| `rx` | Receiver, mirroring `frame.receive` with its defaults (header-noise method "measured", decision-directed noise, burst-aware LLRs), plus the ADR 0006 channel SNR from rebuilt symbols; header-first decoding (`receive_header_first`, `detect_and_receive`, and its halves `receive_header` and `receive_frame`), the broadcast width from the probe (`probe_extent`), data blocks on several threads (`RxOptions::threads`) |
| `dsp` | Savitzky-Golay, convolution, interpolation, unwrap, median, Kaiser FIR with NumPy/SciPy semantics |
| `channel`, `wav` | AWGN test channel; 16-bit WAV reading and writing |

The reported SNR (`RxResult::snr_7db_equivalent`, SPEC 7.1 and ADR 0006)
is the channel's, without clip-and-filter's designed distortion: the
receiver rebuilds the frame from the decoded bits (encode, map,
clip-and-filter), demodulates it with its own window, and fits each tone
over the symbols whose content is known (preamble, probe, a decoded header,
data symbols whose blocks all decoded), with a residual phase ramp where
the pilot track leaves one. `tests/loopback.rs` checks it within 1 dB from
12 to 45 dB at MCS 0, 4 and 6 (measured within 0.2 dB).

The receiver decodes header first (`Receiver::receive_header_first`, or
`detect_and_receive` with the detector in front): it decodes the preamble
and header on the base tones, unpacks the SPEC 8.2 fields, and takes the
rung, the control and data block counts and the probe flag from them. A
session frame (flag bit 3) needs what the header does not carry, the link
width, tone mask and the peer's cap (`LinkState`; `LinkState::from_cap`
for a session whose width is the cap). A frame outside a session takes
its width from its own probe (`probe_extent`, ADR 0007) or is 24 tones
without one. The prefix comes from the detector. Control blocks are
decoded before the data and parsed (`RxResult::controls`); a header that
fails its CRC or has malformed fields stops the decode, with the reason in
`RxResult::header_error`. `Receiver::receive` with a `FrameConfig` still
decodes a frame of known geometry.

ADR 0007 in both implementations (`frame.py`, `frame.rs`):

- **Every wide frame probes.** A frame carries the probe exactly when its
  data symbols use a tone above 23 (`needs_probe`: a data tone or an upper
  pilot; a 56-tone link masked to the base tones, or a frame of header and
  control blocks only, needs none). Forcing the probe off then is refused
  (`ConfigError::NeedsProbe`), and `build_frame` sets header flag bit 4
  itself. Without a probe the frame's grid is the base tones.
- **The probe reaches the sender's cap.** `FrameConfig::cap` (tones, at
  least the width; the B.3 width cap plus one) is the probe's extent and
  the grid's width; `LinkState::cap` carries the peer's for session frames.
  Every symbol keeps the width's per-tone scale, so a probe wider than the
  data has more power than a data symbol (see the notes below).
- **Broadcast width from the probe** (`probe_extent`). Each tone above 23
  is tested: its two probe symbols divided by the known values both measure
  the channel when the tone carries the probe, so Re{z0 conj z1} estimates
  its power (clip-and-filter regrowth above the cap is not the probe
  sequence and averages zero); the power of the data symbols that every
  width has (the frame's at 56 tones, so they are always present) is added,
  less the noise measured on the same tone in the silent preamble, header
  and control symbols. A tone counts when that clears 5 standard deviations
  of its noise-only value and -18 dB against the base tones (regrowth is
  about -30 dB, the mic path's leakage -22 to -25 dB); the width is the
  change point of the run (count of tones that do minus those that do
  not, from tone 24 up; ties to the higher).
  If no data block decodes at that width, the neighbouring widths (+1, -1,
  +2, -2, +3, -3) are tried and the first at which a block decodes is kept,
  so a weak edge tone on a rolled-off path costs a retry, not the frame.
- **Detection margin** (`uv run --package cairn-sim python -m
  cairn_sim.extent_eval --trials 200 --decode`, UI broadcasts at MCS 0
  with 3 blocks). AWGN, widths 25-56: no width error in 200 frames per
  point from 2 dB (widths 32-56) or 3.5 dB (all widths); at 1 dB 0.5-4%,
  at 0 dB 8-24%, while the data's block error rate at the true width is
  still 44-96% at 1 dB and reaches 1% near 3 dB. So the extent alone is
  reliable about 1-1.5 dB below the data's 1% point; with the decode retry,
  header-first decoding loses no frame to the width at or above 3.5 dB
  (its losses there are header failures: 2-7 of 150 frames at 3.5 dB,
  none to the width). Noise-free, the edge decisions clear their
  thresholds by 12-18 dB inside and 13-15 dB outside. Through the mic
  path (FmChannel, whose 3 kHz filters put tone 29 about 10 dB down):
  errors persist at 25-26 tones up to 10 dB (2-7%, the edge tone against
  limiter and filter leakage) and at 29-30 tones up to 3.5-6 dB (the
  rolled-off edge tones), but with the retry header-first block loss
  equals the true-width loss from 6 dB up (0-1%).
- **SNR with a mask** counts the mask's power boost as signal (ADR 0006
  interpretation); the two receivers agree within 0.07 dB on every
  fixture (`tests/rx_fixtures.rs`).

## Verification

Real-time layer:

- `cli/src/txseq.rs`: a virtual-clock output device and a logging PTT
  check the order key, TXDelay, audio, drain, TXTail, unkey (first sample
  within a sample of TXDelay, release TXTail after the last sample's DAC
  time, every sample played while keyed), underrun counting, and release
  on an audio error, a panic and a stop signal.
- `cli/src/listen.rs`: three fixed-configuration frames straddling chunk
  boundaries at different phases, the last ending 100 samples before the
  input, all decode; frames with real headers (broadcasts at 24 and 40
  tones, a BEACON with the ID block, a masked session DATA frame at 30
  tones) decode header first with the header, geometry, blocks and
  control blocks of the transmitter; ten seconds of noise give no
  detection.
- `cli/src/headers.rs`: callsign and frame-type parsing, message payloads,
  a wide broadcast's probe flag, control-block summaries.
- `cli/src/nprng.rs`: the NumPy generator against values recorded from
  NumPy 2.5; `cairn-bench rx` scores `tx --wav` output with every header
  and block bit matching.
- `ptt/`: guard release on drop, error and panic; the rigctld protocol
  against a fake server; serial line bits; CM108 reports.
- `modem/tools/e2e_wav.sh`: real-header UI broadcasts at MCS 0 (with the
  ID block), 2 (with the tone and a message), a BEACON, 40 tones at MCS 4
  and 56 tones at MCS 5 with the long prefix (widths from the probe), a
  session DATA frame at 40 tones with a mask, and the random-header frames
  with `listen --fixed`: every frame, block and control block decodes
  header first from the clean WAV and through FmChannel (mic at 25 dB, or
  flat25 at 30 dB over the width, +40 ppm), and in `cairn-bench rx` with
  header bits matching.
- `modem/tools/turnaround.sh`: the receive turnaround at real-time pace
  (below).

Physical layer (paths under `phy/`):

- `tests/ldpc_vectors.rs`: every `vectors/ldpc_*.json` vector (CRC, full
  codeword, every rate-matched length), the rate-matching tables, the
  lifted files against `codes/bg*.csv`, and the alists.
- `tests/field_vectors.rs`: every `vectors/header/` and `vectors/control/`
  case: fields pack to the published bits and unpack back; the headers and
  blocks a receiver must refuse are refused, a reserved control type is
  skipped; SHA-256 known answers.
- `tests/header_first.rs`: every header and control field over its range,
  addresses, single-bit CRC errors, flag and version checks; header-first
  decode through AWGN; a header-only frame (clipped at 7 dB); control
  blocks with a probe, a mask and the long prefix at 30 tones; a failed
  header, a malformed one, a reserved rung and one failed control block.
- `tests/bitmap_vectors.rs`: every stage of all 13 `vectors/bitmap/`
  frames: info, rate-matched, scrambler, scrambled and interleaved bits,
  padding, I/Q levels, segment geometry and gains, the tone grid (within
  its 9 printed decimals) and the three sample streams (within 6).
- `tests/bitmap_vectors.rs` also checks the 7 `vectors/frame/` frames
  (real headers and control blocks: control segment geometry, positions,
  stages, grid, samples), builds them from their fields, and decodes the
  published samples header first.
- `tests/adr0007.rs`: a wide frame without the probe refused; the probe
  to a cap above the width (24/30, 30/56, 40/48) with the data within the
  width, decoded with the cap from the link state; wide broadcasts (25,
  30, 40, 56 tones) decoded with no link state, width from the probe;
  probe extent at 25-56 tones from 3.5 to 15 dB and noise-free (clip
  regrowth above the cap ignored); session frames at several widths and
  masks; block results independent of the decoder threads.
- `tests/rx_fixtures.rs`: the incremental detector gives the one-shot
  detections (start, prefix, grid point, rho within 1e-6) at chunk sizes
  from one sample to a megasample, each final within its stated latency.
- `tests/rx_fixtures.rs`: fixtures from `tools/gen_rx_fixtures.py` (clean,
  AWGN, mic, repeater with long prefix, masked, ±100 ppm, noise burst) as
  16-bit WAV. The Rust detector finds the simulator's starts, prefixes and
  clock-grid points; the receiver's per-tone SNR agrees to 1e-12 dB and it
  decodes the same blocks in the same number of LDPC iterations; the
  protocol's channel SNR (ADR 0006, mask boost counted) agrees within
  0.15 dB (measured: 0.07 dB at most, 0.006 dB on the masked 30-tone
  frame). Six `hdr_*` fixtures (mic and repeater profiles, AWGN at 30
  tones with a probe, mask and two control blocks, a header-only PROBE,
  and two wide UI broadcasts, 33 tones in AWGN and 27 through the mic
  path) carry real headers and control blocks and are decoded header first
  by both receivers: same configuration from the header (and the same
  width from the probe for the broadcasts), same control blocks, per-tone
  SNR within 1e-13 dB. Rust clip-and-filter matches the simulator to 3e-15.
- `tests/loopback.rs`: Rust TX to Rust RX, every rung at widths 24-56,
  both prefixes, with and without masks; header-only frames; the channel SNR.
- Block-by-block agreement near the MCS 2 threshold (3,200 blocks at 7.0,
  7.5, 8.0 and 8.25 dB, 24 tones): no block, header or LDPC iteration count
  differs between the Rust and Python receivers.
- BLER, MCS 2 at 24 tones in AWGN, detector in the loop:

  | SNR (dB) | Python (6,400 blocks) | Rust (12,800 blocks) |
  | --- | --- | --- |
  | 7.0 | 65.8% | 65.1% |
  | 7.5 | 17.5% | 17.0% |
  | 8.0 | 1.23% | 1.63% |
  | 8.5 | 0 | 0.055% |
  | 9.0 | 0 | 0 |

  1% thresholds 8.02 dB (Python) and 8.07 dB (Rust).

Regenerate the fixtures (deterministic) with

```sh
uv run --package cairn-sim python modem/tools/gen_rx_fixtures.py
```

Cross-checks that are too slow for `cargo test`:

```sh
# Block-by-block agreement on a sweep near the MCS 2 threshold
uv run --package cairn-sim python modem/tools/gen_rx_fixtures.py --sweep 50 --out /tmp/sweep
cargo run --release --example rx_agreement -- /tmp/sweep
# BLER against SNR, same procedure as link_sim.bler_point
uv run --package cairn-sim python modem/tools/bler_python.py 2 8.0 --blocks 6400
cargo run --release --example bler -- 2 7.5,8.0,8.5 --blocks 12800
```

## Performance

`cargo run --release --example bench`, one thread, fastest of 7, on a
performance core of the Apple M2 (Asahi Linux) while other jobs loaded the
machine. A 4 s frame (SPEC Appendix A geometry, short prefix); RX includes
the channel-SNR rebuild; "-3 dB" is 3 dB below the rung's threshold, where
most blocks run all 30 LDPC iterations (the decoder's worst case).

| Width | MCS | Blocks | Frame (s) | TX (ms) | Detect (ms) | RX +1 dB (ms) | RX -3 dB (ms) | RX clean (ms) | (Detect + worst RX) / frame |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 24 | 0 | 7 | 3.79 | 5.1 | 9.5 | 7.6 | 12.9 | 7.1 | 0.006 |
| 24 | 1 | 10 | 3.62 | 4.9 | 9.5 | 7.0 | 11.1 | 6.8 | 0.006 |
| 24 | 2 | 14 | 3.79 | 5.2 | 10.1 | 9.3 | 21.7 | 8.8 | 0.008 |
| 24 | 3 | 21 | 3.80 | 6.3 | 9.5 | 8.6 | 20.3 | 9.1 | 0.008 |
| 24 | 4 | 28 | 3.79 | 6.4 | 12.3 | 11.7 | 29.5 | 9.7 | 0.011 |
| 24 | 5 | 35 | 3.80 | 6.6 | 11.4 | 12.0 | 25.8 | 10.6 | 0.010 |
| 24 | 6 | 42 | 3.80 | 9.2 | 11.6 | 13.9 | 36.3 | 12.7 | 0.013 |
| 56 | 0 | 16 | 3.70 | 7.0 | 9.6 | 9.5 | 21.2 | 8.3 | 0.008 |
| 56 | 1 | 25 | 3.85 | 5.8 | 10.7 | 11.4 | 25.4 | 11.3 | 0.009 |
| 56 | 2 | 33 | 3.80 | 7.6 | 12.9 | 17.4 | 51.4 | 13.5 | 0.017 |
| 56 | 3 | 50 | 3.85 | 8.2 | 12.9 | 16.7 | 45.1 | 13.7 | 0.015 |
| 56 | 4 | 67 | 3.86 | 9.3 | 13.9 | 21.8 | 67.7 | 16.5 | 0.021 |
| 56 | 5 | 83 | 3.84 | 8.8 | 13.9 | 20.3 | 57.9 | 16.4 | 0.019 |
| 56 | 6 | 100 | 3.85 | 9.2 | 12.3 | 24.6 | 84.6 | 19.7 | 0.025 |

On an efficiency core, under the same load, the worst case was 11% of the
frame (56 tones, MCS 6: 359 ms to receive, 60 ms to detect).
