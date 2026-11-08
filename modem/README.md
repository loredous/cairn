# Cairn modem (Stage 2, Rust)

The real-time modem of SPEC §21.3. Done so far: the physical layer, ported
from the Stage 1 simulator (`sim/cairn_sim/`) and checked bit for bit
against the published vectors; the real-time layer around it (steps 1-4):
audio I/O at 48 kHz, PTT backends with measured TXDelay and TXTail, a
transmitter that sends frames with real headers and control blocks, and a
continuous receiver that decodes each frame header first within the
100 ms turnaround of SPEC 7.3; carrier detect (SPEC 7.2); and steps 5-6:
the link layer (`link/`, sessions, ARQ, adaptation, resume, link test,
broadcast) wired to the PHY in one runtime with a KISS server and a
control interface (`cairn-modem run`). Two complete modems hold sessions
over a simulated FM channel (`cli/tests/two_station.rs`), and the first
on-air two-radio session ran on 2026-11-07 (24 tones, a few kB each way);
from it came wider session widths with tone masks (`run --cap`), received
files on disk (`run --rx-dir`) and carrier-detect hysteresis
([docs/bench-testing.md](../docs/bench-testing.md#two-station-session-with-cairn-modem-run)).
The native JSON API of SPEC 13.2 (step 6) runs in `run` on TCP port 8002
and a Unix socket, with the heard list and, from step 8, the raw-audio ring
buffer and diagnostic capture; its schema is
[docs/api.md](../docs/api.md) ([docs/api.schema.json](../docs/api.schema.json)).
After a capture lost 26.6 ms inside a frame on a loaded laptop in that
session, the audio path asks for real-time priority, captures through ALSA
with a 200 ms buffer, fills any capture gap with exactly as many zeros
(later frames keep their timing; the frame across it is flagged), and
`run --rx-agc` sets the receive gain between frames (SPEC 13.5); see
[real-time audio](#real-time-audio-priority-buffers-gaps-and-receive-gain).
The operator's side of v0.5 (SPEC 13.4, 13.6, 16.2) is in: a config file
with rig profiles, a guided `setup` with transmit steps that each need a
typed `yes`, a non-transmitting `selftest`, and `run --unattended` with
allow and deny lists, session and daily keyed-time limits, periodic ID in
broadcasts, a PTT watchdog, a rotating log and hooks; see
[operator experience](#operator-experience-config-rig-profiles-setup-self-test-unattended)
and [docs/operator-guide.md](../docs/operator-guide.md).

```sh
cd modem
cargo test --release                         # vectors, fixtures, loopback, spec tables, mock audio/PTT, link, two-station sims
cargo clippy --all-targets -- -D warnings
cargo run --release --example bench          # single-core timing of 4 s frames
cargo build --release -p cairn-modem         # target/release/cairn-modem
```

Building needs the ALSA headers (`libasound2-dev` on Debian/Ubuntu,
`alsa-lib` on Arch) and `pkg-config`. The `alsa` feature (on by default)
adds native ALSA capture and the receive-gain mixer control;
`--no-default-features` builds without them (capture through cpal, no
`--rx-agc`).


## Crates

| Crate | Directory | Contents |
| --- | --- | --- |
| `cairn-phy` | `phy/` | Waveform, codes, bit mapping, preamble detector, receiver (below) |
| `cairn-audio` | `audio/` | Streams at 48 kHz chosen by name (capture on native ALSA with its own period and buffer, or cpal; playback on cpal), lock-free rings, real-time thread priority (`rt`: `SCHED_FIFO` or rtkit), xrun, gap and ring high-water counters, capture gaps filled with zeros at their place, a mock capture device, timing of every output sample, input level meter, the capture-gain mixer control (`mixer`) |
| `cairn-ptt` | `ptt/` | PTT trait and backends: serial RTS/DTR, CM108 HID, Hamlib `rigctld`, VOX/none; a drop guard and a shared handle for signal handlers |
| `cairn-link` | `link/` | The link layer, sans-IO (SPEC 8-12, ADRs 0008-0013); see [link/README.md](link/README.md) |
| `cairn-api` | `api/` | The native API's transport side (SPEC 13.2): protocol constants and message builders, the TCP and Unix-socket server with per-client subscriptions, the heard list, the raw-audio ring buffer and capture files, a JSON Schema validator for tests |
| `cairn-modem` | `cli/` | Transmit sequencing, the listener, test frames and the level-setting tone; the modem runtime (link layer + PHY + carrier detect), KISS, the native API on the runtime (`hostapi`), the control interface, the two-station simulator (library), and the `cairn-modem` binary |

## The `cairn-modem` command

```sh
M=target/release/cairn-modem
$M setup                                     # guided setup: writes ~/.config/cairn/cairn.toml (asks before keying)
$M selftest                                  # PASS/WARN/FAIL checks, nothing transmitted (--transmit: one ID frame)
$M rigs                                      # rig profiles; --rig NAME or station.rig uses one
$M devices                                   # every ALSA device; 48k = can run at 48 kHz; PTT candidates
$M levels -d All-In-One-Cable --seconds 5    # input RMS (about the mean, and with DC), peak, DC, CLIP/HOT, xruns
$M ptt-test --ptt serial:All-In-One-Cable --seconds 1      # refuses more than 5 s without --force
$M tone -d All-In-One-Cable --ptt serial:All-In-One-Cable --seconds 3   # SPEC 7.1 level-setting tone
$M tx -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 --mcs 2 --blocks 3
$M listen -d All-In-One-Cable                               # header first; Ctrl-C to stop
$M run -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1   # the modem (below)
```

- **Config file and rig profiles**: every command reads
  `~/.config/cairn/cairn.toml` (`--config PATH`, `CAIRN_CONFIG`,
  `--no-config`) and the rig profile it names (`--rig NAME`); flags and
  their environment variables override both. See
  [operator experience](#operator-experience-config-rig-profiles-setup-self-test-unattended).
- **Devices** are chosen by a substring of the ALSA name or its
  description, ignoring case and punctuation (`All-In-One-Cable` matches
  `plughw:CARD=3,DEV=0`, whose description is "All-In-One-Cable, USB
  Audio"), never by index. Among several matches the `plughw:` device
  wins, then `hw:`. `CAIRN_DEVICE` sets the default.
- **PTT** (`--ptt`, or `CAIRN_PTT`): `none` (default), `vox`,
  `serial:PATH` (a path, or a substring of a `/dev/serial/by-id/` name),
  `cm108:/dev/hidrawN` (`--ptt-gpio`, default 3; `cm108:auto` takes the
  only C-Media-class HID node), `rigctld[:HOST[:PORT]]`.
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
  from the frame: control and data present from the counts, first
  fragment (flag bit 6) on every BEACON and UI frame (each a complete
  message, ADR 0009), session for `--type data` (which also carries an
  ACK control block first, SPEC 8.4, cumulative with a mask, ADR 0008 and
  ADR 0011, demonstration values), and the probe whenever the data uses a
  tone above 23 (ADR 0007), to the frame's width (ADR 0011
  `width-probe-extent`; `--cap` overrides it for tests). Then `--mcs`,
  `--width`, `--prefix short|long`, `--mask 8-11,30` and `--mask-id N`
  (session frames only: the mask and the ID the header names; a broadcast
  is unmasked), `--blocks N`
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
  `--mask` under `--mask-id` (a frame naming another Mask ID is reported
  undecoded, ADR 0011) and `--peer-cap` (the extent of the peer's probe;
  default the width, as ADR 0011 requires), handed to the receiver as the
  link state for each decoded header (`cairn_phy::LinkSource`); a frame
  outside a session takes its width from its own probe (ADR 0007), or is
  24 tones without one. It prints one
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
- **Audio threads and buffers** (every command that opens a sound card):
  `--period` (frames, default 480 = 10 ms: how often the audio threads
  wake), `--buffer-ms` (capture buffer, default 200: headroom, no added
  latency), `--audio-backend auto|alsa|cpal` (default auto: native ALSA,
  cpal if that fails to open), `--no-rt`, `--rt-prio` (default 60 for the
  audio threads; processing threads 20 below). The input line says what
  was granted: `input plughw:... (S16 x1 @ 48000 Hz, period 480 (10.0 ms),
  buffer 9600 (200.0 ms), alsa); capture thread SCHED_RR 20 via rtkit`.
  How to allow real-time priority: [docs/bench-testing.md](../docs/bench-testing.md#real-time-priority-capture-gaps-and-receive-gain).

### How it works

- **Audio** (`cairn-audio`): the audio threads only convert samples
  (i16, S32 or f32, any channel count; input takes the first channel) and
  move them through preallocated `rtrb` rings, plus atomics: no
  allocation, locking or I/O. Counted: ALSA xruns, input ring overflows and
  the ring's high-water mark, gaps in the capture timestamps (samples lost
  upstream whether or not ALSA said so), output underruns during a
  transmission. Capture runs on native ALSA (the `alsa` feature) with the
  period and the buffer set separately, playback on cpal; see
  [real-time audio](#real-time-audio-priority-buffers-gaps-and-receive-gain). The output stream runs
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

## The modem runtime (`cairn-modem run`)

SPEC 21.3 steps 5-6: the link layer (`cairn-link`) wired to the PHY with
carrier detect, a KISS server and a control interface. The procedure for a
two-station session on the air is in
[docs/bench-testing.md](../docs/bench-testing.md#two-station-session-with-cairn-modem-run).

```sh
M=target/release/cairn-modem
$M run -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 --grid FN31
#   --kiss 127.0.0.1:8001 (default; "off")    --control 127.0.0.1:8003 (the stdin protocol on TCP)
#   --api 127.0.0.1:8002 (default; "off")     --api-socket PATH (the native API on a Unix socket too)
#   --ring-seconds 120 (raw-audio ring; 0 off)  --capture-dir captures  --capture-on-failure  --heard-file PATH
#   --txdelay/--txtail (ms)  --persistence 0.25  --slot-ms 100  --max-frame 4.0  --broadcast-mcs 0
#   --cap 32 (width cap in tones, 24-56)  --no-mask  --rx-dir DIR (received files and messages)
#   --quieting 3600-4150 (Hz; default just above the cap)  --dcd-legacy  --refuse  --status 30
#   -v (every header and DCD change)  --seconds N  --tx-budget S (never key more than S s in total)
#   --rx-agc [--rx-agc-control NAME] (receive gain between frames)  --period/--buffer-ms/--no-rt/--rt-prio
#   --unattended [--allow CALL-*,...] [--deny ...]  --id-broadcasts  --log-file PATH  --no-watchdog
```

- **Width** (`--cap N`, 24-56 tones, default 24): the B.3 width cap is
  N - 1, and above 24 the station advertises capability bit 0. In a
  session each station sends at its own cap + 1 when its peer advertised
  the bit, else at 24 tones (SPEC 5.4, ADR 0011 `session-width`): with
  `--cap 32` at both ends, 32 tones each way. CONNECT carries the probe to
  the sender's cap, CONNECT-ACK and every session frame whose data uses a
  tone above 23 to the session width (ADR 0007, ADR 0011). When a session
  opens both ends log `session width with PEER: tx 32 tones (own cap 31,
  peer wide), rx 32 tones (peer cap 31, own wide); tx mask ID 0: ...`.
  Broadcasts stay at 24 tones.
- **Tone masks** (on unless `--no-mask`): the receiver measures every tone
  of every decoded frame from the peer (the ADR 0006 channel SNR fitted per
  tone on the rebuilt symbols; tones above 23 only from a frame's probe)
  and runs SPEC 10.3 through the link layer's mask tracker: a tone more
  than 6 dB below the active-tone average for two consecutive frames is
  dropped, a dropped tone comes back when a probe shows it within 3 dB of
  the average (the reading `READING_MASK_RESTORE_DB`), every change takes
  the next mask ID, and every ACK block carries the mask with its ID; the
  sender applies the latest mask it decoded and names its ID in the
  header (ADR 0011). CONNECT-ACK reports the mask the CONNECT's probe
  showed, so a session starts masked. A receiver with a tone dropped asks
  for a probe after 10 s without one (ADR 0013). Changes print as
  `rx mask report ID 1 to PEER: tones 0-2 masked, 29 of 32 active` and
  `tx mask ID 1 from PEER: ...`, rung changes as `rungs with PEER: ...`;
  `status` shows each direction's rung, width and mask.
- **Received files** (`--rx-dir DIR`): FILE-CHUNK PDUs are written into
  `DIR/CALL_objN.bin.part` at their offset as they arrive (CALL the
  sender, N its object ID), and the file is renamed to `CALL_objN.bin` once
  every byte up to its end has arrived, with `rx file from CALL object N
  complete: BYTES bytes -> PATH sha256 HEX`. Text messages (session
  MESSAGE PDUs, and UI broadcasts) become `CALL_msgK.txt` and
  `CALL_uiK.txt`. Names never overwrite (`-2`, `-3`, ... are appended).
  FILE-CHUNK carries no object length, so this modem marks the end itself:
  every chunk but the last carries 2048 bytes, and a file whose length is
  a multiple of 2048 ends with an empty chunk at its length.

Control commands (stdin, or the `--control` port; one line each, every
line out carries the seconds since start):

```text
connect CALL[-SSID] [new]   open a session; resumes retained state unless "new"
send TEXT | send @PATH      a MESSAGE PDU, or a file as FILE-CHUNK PDUs (object ID, offset, data)
disconnect [now]            graceful by default (after everything queued is acknowledged)
probe CALL [beacon]         link test (ADR 0012), or a BEACON request
beacon [TEXT]  |  ui TEXT   broadcasts
heard                       the heard list (SPEC 13.6)
capture [SECONDS] [LABEL]   save the raw-audio ring and decoder state (below)
status  |  quit
```

Host events print as they happen: connecting, connected (initiator or
responder, resumed), connect failed (refused, no answer after three
retries), refused an incoming CONNECT, link lost (state kept 10 minutes),
disconnected, rx (text, or file chunks with object and offset), delivered
(when every PDU of a send is acknowledged), undelivered, link-test result
(both directions' SNR, rung and expected kbps), beacons heard, warnings
(receive clipping, the peer's distortion report), and each transmission
with the measured TXDelay and TXTail.

### The native API (SPEC 13.2)

JSON lines on TCP (`--api`, default `127.0.0.1:8002`) and a Unix socket
(`--api-socket PATH`), any number of clients; the schema, with every
command, event, field, unit and error code and an example of each, is
[docs/api.md](../docs/api.md), and [docs/api.schema.json](../docs/api.schema.json)
is the same as a JSON Schema. Each connection starts with a `hello` event
naming the schema version (0.1.0), the commands, the unsupported ones and
the events.

```sh
python3 tools/api_client.py                                       # print every event
python3 tools/api_client.py --connect N0CALL-2 --file report.txt --disconnect
printf '%s\n' '{"id":1,"cmd":"status"}' | nc -q1 127.0.0.1 8002     # by hand
```

- **Commands**: `connect` (resumes retained state unless `"resume":false`),
  `disconnect`, `send` (session or broadcast; `text`, base64 `data` or a
  `file` path as FILE-CHUNK PDUs; `stream`, `priority`, `encoding`,
  `pdu_type`), `beacon`, `probe` (link test or BEACON request), `heard`,
  `status`, `config` (read, and set the parameters that are safe live:
  persistence, slot time, maximum frame time, broadcast rung, width cap
  outside a session, TXDelay, TXTail, accepting CONNECTs; all or nothing),
  `calibrate`, `capture`, and `subscribe` (per-client event filter).
- **Events**: `rx` (every delivered PDU: source, mode, stream, priority,
  type, encoding, CRC, signature result, the body in base64 and as text,
  where `--rx-dir` saved it), `session` (state changes; progress: bytes
  queued, acknowledged and delivered, rate over 30 s, ETA, blocks sent and
  resent; each send's delivery or non-delivery), `link` (per frame from the
  peer: SNR, clock offset, rung and width and mask each way, block errors
  and the block error rate over 16 frames, the peer's report from its ACK
  block; link-test results), `heard` (every decoded header), `channel`
  (carrier detect with and without hold, who holds the channel, whose turn
  it is, what the modem wants to send and why it waits: backoff, carrier,
  the turn-holder check, the yield window, p-persistence; time to the next
  yield window; legacy activity), `dcd` (each raw detector edge), `tx`
  (each keying with the measured TXDelay and TXTail; broadcasts sent),
  `error` (asynchronous errors and the clipping and distortion warnings).
- **Unsupported here**: `calibrate` (no host level control yet, SPEC 21.3
  step 7) and `send` with `"encoding":"zstd"` or `"zstd_dict"` (no codec or
  dictionaries yet, SPEC 12.2) answer the schema's `unsupported` error;
  `auto` sends raw. Signatures are not implemented, so every `rx` says
  `"verify":"unsigned"`. `capture` answers `unsupported` with
  `--ring-seconds 0`.
- **Heard list**: every decoded header updates its source's entry (first
  and last heard, count, last and best SNR, width, rung, frame types and
  their counts, modes); `heard` lists it most recent first, `heard` events
  push each update, and `--heard-file` keeps it across restarts (written
  every 30 s when changed and at exit).
- **Raw-audio ring and capture** (SPEC 13.6, 21.3 step 8): the last
  `--ring-seconds` (default 120 s, 11.5 MB as 16-bit samples) of input are
  kept in a buffer allocated at start; the event loop copies each 10 ms
  chunk in (the sound card's callback is untouched, and nothing is
  allocated per chunk). `capture` (API or the control line) writes the last
  N seconds to `--capture-dir` as `cairn-YYYYMMDDTHHMMSSZ[-label].wav`
  (16-bit mono 48 kHz, the samples exactly as captured) and a `.json`
  sidecar: every decoded header in the window (time, source, type, rung,
  width, blocks, SNR, clock offset, decode time), header failures, the
  carrier-detect timeline, own transmissions, the `status` and `config`
  results. `--capture-on-failure` captures 2 s after a frame from the
  session peer loses blocks or a header fails during a session, at most
  once a minute. `listen --wav` and `cairn-bench rx` replay the WAV.
- Port 8002 has no authentication: it can key the radio and read any file
  the modem can (`send` `file`). Keep it on loopback or a Unix socket.

KISS (SPEC 13.1, ADR 0010 `kiss-ui-frames`): each data frame from a host
is one AX25 PDU in a UI frame to the broadcast address (several per frame
when they fit, fragmented when one does not); every AX25 PDU received in a
UI frame goes to every connected host unchanged. TXDELAY, persistence and
slot time map onto the modem (p = (P + 1)/256, 10 ms units).

### How it works

- **One core, two clocks** (`runtime::Modem`): a sans-IO core owns the
  `Station`. Audio comes in 10 ms chunks stamped with the time of their
  first sample; every call carries `now`. Live (`driver`), `now` is the
  monotonic clock and capture times come from the earliest arrival of
  each chunk over the last 5 s; in the simulator both are the medium's
  sample clock.
- **Receive.** Each chunk goes to carrier detect and to the streaming
  preamble detector. A detection is decoded header first: the header asks
  the station how to decode the rest (`Station::rx_link`: a session frame
  from the peer at the session width with the mask its Mask ID names; a
  mask ID it does not hold leaves every block failed, ADR 0011; CONNECT,
  CONNECT-ACK and frames outside a session take their width from their
  probe). The frame goes to the station with the ADR 0006 channel SNR,
  per-tone SNR, the probe flag, input clipping during the frame and the
  time of its last symbol. A preamble whose header fails is
  `Event::HeaderFailed`.
- **Carrier detect.** Raw edges of the OR of quieting, the preamble
  correlator, the Bell 202 detector and COS go to the station without hold
  (it applies the hold, ADR 0010); a Bell 202 detection is also legacy
  activity (2 s frames and half p for 10 minutes, SPEC 11.3). The
  preamble correlator is up from the detector's early confirmation
  (`Detector::take_early`, the gated plateau and S1 correlation over
  threshold) to the end of the frame its header describes.
- **Transmit.** `Action::Transmit` is converted (`PhyCodec`), built with
  `Transmitter::build_frame` and scaled to -3 dBFS peak; a transmit thread
  keys, plays TXDelay later, releases TXTail after the last sample has
  played (`txseq::transmit`) and reports the release time, which is the
  station's `TxDone`. From keying to release plus 5 ms the receiver ignores
  its input (half duplex): the detector sees silence and carrier detect
  neither asserts nor learns.
- **Frame model** (`phymodel::PhyFrameModel`): on-air time from
  `cairn_phy::FrameConfig` geometry (TXDelay + samples + TXTail); it gives
  Appendix A.2's blocks per frame on every rung at 24-56 tones. (With
  three control blocks the PHY packs them one symbol shorter than A.1's
  3 x 12.)

### Two-station simulation (`cli/tests/two_station.rs`)

`twosim::Sim` runs complete `Modem`s (real PHY TX and RX, the real link
layer, real carrier detect) on one simulated audio channel, on a virtual
clock: about 2.5 s of wall time per minute of two-station session (one
core per test). The medium sums each receiver's audible transmitters (resampled for
their sample-clock offset), adds that receiver's noise and models FM
quieting: open-squelch noise as loud in the band as the reference signal
until a carrier it hears is keyed (from PTT on, so during TXDelay too),
then noise at that link's SNR (the 7 dB-equivalent in-band SNR of SPEC 7.1).
The noise is white, band-limited 150 Hz-4 kHz. Outages, SNR steps and
foreign transmitters (a Bell 202 station) can be scheduled. A link can take
an audio response (`Sim::set_shape`, a linear-phase FIR applied with its
delay removed) and a receiver coloured noise (`SimStation::noise_shape`);
`Sim::radio_like_link` gives both from the radios of the first on-air
session (`twosim::radio_like`: the noise 25 dB stronger at 300-350 Hz, 17
at 450, 11 at 550, 4 at 650 Hz, then tilting from +1.5 dB at 1.1 kHz to
-2.5 dB at 3.5 kHz; the signal flat from 650 Hz to 3.9 kHz and -10 dB at
4.4 kHz; per tone the link SNR less 28, 18 and 11 dB on tones 0-2; heard
12 dB below the transmitted level), and `SimStation::radio_tail` keeps a
station's carrier up after PTT release while its own receiver hears
silence. Otherwise AWGN: the FM mic, flat and repeater models of
`sim/cairn_sim/channel.py` are not ported yet (the PHY's fixtures cover
them frame by frame). Results, 24
tones, TXDelay 150 ms, TXTail 10 ms, decoded frames reaching the station
20 ms after their completing chunk:

| Scenario | Result |
| --- | --- |
| Connect, 50 kB each way at once, disconnect; 25 dB, clocks +60/-40 ppm | Every byte delivered once and in order both ways in 105 s; 3.86 + 4.16 = 8.0 kbps together (both at MCS 5; A.2 one way 7.61); no delivered block resent; both ends `Disconnected`, nothing undelivered |
| 50 kB one way (15 kB at 8 dB) | 8 dB: MCS 0, 1.11 kbps (A.2 1.21, 92%); 14 dB: MCS 2, 2.81 (3.02, 93%); 20 dB: MCS 3, 4.27 (4.55, 94%); 25 dB: MCS 5, 6.87 (7.61, 90%); 32 dB: MCS 6, 8.51 (9.11, 93%); no block lost. The shortfall is start-up (CONNECT, the first frames) and the last short frame |
| SNR 25 -> 19 -> 25 dB during a transfer | MCS 5, then MCS 3 from the 2nd frame after the drop, then MCS 4 again on the 4th frame after the rise (one step, ADR 0013; the run's lowest measured SNR held it one rung below MCS 5, which the test allows) |
| A Bell 202 station (300 ms of flags, 60 bytes, 734 ms) keys mid-session at 20 dB | Both stations' AFSK detectors assert; no transmission starts while it is keyed or within 100 ms after (the next one 156 ms after its release); frames capped at 2 s afterwards (SPEC 11.3); the transfer completes |
| 120 s outage mid-transfer, A reconnects 1 s after its link loss | Link lost at 80 s (A) and 115 s (B), resumed at 137/141 s, transfer complete with no delivered block resent and nothing undelivered |
| Link test, then a session | 0.86 s of air; 14 dB: forward SNR 14.2 dB, MCS 2, 3.02 kbps predicted, session 2.82 (-6%); 25 dB: 25.2 dB, MCS 5, 7.59 predicted, session 6.56 (-14%: four more frames at MCS 4 before the climb) |
| Two AX.25 frames from KISS (one of 920 bytes, fragmented) | Both back at B's KISS side unchanged, in order, from UI frames to the broadcast address |
| BEACON with a status text | Heard with its ID block; text delivered |
| Idle, open squelch, 2 x 10 min x 3 seeds (`--ignored`) | No carrier-detect rise from any detector, no detection |
| Radio-like channel (below), link SNR 24 dB, `--cap 32` both, 60 kB one way | Masked: tones 0-2 dropped from the CONNECT-ACK on (every DATA frame), MCS 2, then MCS 4 from the 4th frame, 6.24 kbps net (60 kB in 77 s), no block lost. Unmasked (`--no-mask`): the noisy low tones pull the measured SNR down and the session stalls at MCS 2, 3.88 kbps (124 s). At 28 dB: MCS 5 masked, 7.35 kbps, against MCS 3 unmasked, 5.91 |
| 32 tones at 26 dB, tones 0-2 fade by 25 dB at 25 s and recover at 70 s | Masked from the 3rd DATA frame after the fade (mask ID 1), restored on the 2nd after the recovery (ID 2); the rung holds MCS 4 throughout and climbs to 5 after |
| Both radios keep their carrier 0.65 s after PTT release (as on the air) | Each receiver hears its own tail as silence and waits for the noise (back 635-645 ms after release): quieting rises 7 and 8 for 7 and 8 peer transmissions (legacy detector: 15 and 16); the peer waits the carrier out and replies 814-824 ms after the last sample |
| `--rx-dir` with three files (9000, 4096 and 1 bytes) and a message | Each file byte-identical on disk with its SHA-256 logged; the message in `N0AAA_msg1.txt` |

**Turnaround** (peer's last audio sample to own PTT, every turn-holder
reply): p50 73.8 ms, max 77.5 ms over 23 replies with the 20 ms decode
model; with decoded frames reaching the station after the measured decode
time (`DecodeTime::Measured`, 4 block threads; decode p50 8.6 ms, max
9.3 ms) p50 73.8, max 81.2 ms. The time is TXTail (10 ms), the quieting
detector's release (about 12 ms after the carrier drops), the 10 ms chunk
and the 50 ms raw-DCD window of ADR 0010 `turn-holder-reply-check`; the
decode runs in parallel with the window. Live, the event loop adds its
2 ms poll and the decode's wake-up (the loop runs under the utilisation
clamp, see "Full speed on wake-up").

### Carrier detect (`cairn_phy::dcd`, SPEC 7.2)

| Detector | How | Measured |
| --- | --- | --- |
| Quieting | 8th-order Butterworth band power above the highest tone (2800-3200 Hz at 24 tones, 3600-4150 at 32, 6000-7000 at 56; from `--cap`, or `--quieting`) per 10 ms; the open-squelch floor learnt from the 200 ms mean (rises with 0.4 s, falls with 2 s, frozen while asserted, re-learnt after 60 s); asserts when the 50 ms mean is 6 dB below the floor, releases when the 20 ms mean is back above the lower of floor -3 dB and the midpoint between floor and carrier level; unarmed while the floor is below -60 dB (squelch closed, or no radio). Hysteresis: after a release it decides on hops after it only (50 ms at least) and for 250 ms needs a 3 dB deeper drop; once up it stays up 30 ms at least. After own PTT it neither learns nor asserts until the open-squelch noise is back (at most 1.5 s) | Up 33-58 ms after a carrier keys; down 5-15 ms after the noise returns (20 dB quieting), 7-31 ms (10 dB) |
| Preamble correlator | The streaming detector's early confirmation (`Detector::take_early`: plateau and S1 correlation over threshold), held to the end of the frame its header describes | Fires 48-54 ms after a frame starts (final detection 88-94 ms), down to 2 dB SNR |
| Bell 202 | Sliding one-bit DFTs at 1200 and 2200 Hz: tone dominance, tone purity within a bit, transitions on the 1200-baud grid (rising and falling separately, for twist), over 60 ms | Up 50-80 ms after the burst starts at 10-30 dB with up to 6 dB twist (up to 130 ms at 10 dB with 9 dB twist); down 6-31 ms after it ends |
| COS | `Modem::set_cos` (no sound-interface GPIO reader yet) | |

**On the air (2026-11-07).** The first two-radio session counted 71 and 72
quieting rises in 3.5 minutes (`run -v` logs). Two causes: the radios (or
their AIOC cables) keep the carrier 0.64-0.73 s after PTT release, so each
station's receiver heard silence after every own transmission and quieting
asserted on it (17 and 16 rises), and the noise came back a few dB low
after every carrier with the 50 ms assert window still holding the quieted
hops, so it flapped every 10-30 ms around every transmission. With the
hysteresis and the recovery after own PTT above, on the off-air captures
(`cap_AtoB.wav`, `cap_BtoA.wav`: idle open-squelch noise, four carriers
each with a frame) `cargo run --release --example dcd_wav -- CAP.wav`
counts 5 and 4 rises (none idle, one per carrier, one dropout inside a
24-tone frame) against 9 and 8 for `--legacy` (two per carrier: the flap at
each carrier's end); with the 32-tone band (`--width 32`, 3600-4150 Hz)
exactly one per carrier, also on the 32-tone frames. Latency is unchanged
(`tests/dcd.rs`). In the simulator with a 0.65 s radio tail the own-tail
rises are gone (table above); the receiver's recovery time after each own
transmission is in `status` ("receiver back after own PTT").

False asserts (the §14.1 target is at most one per 10 minutes): none from
any detector on 80 minutes of synthetic open-squelch noise (white and
f²-tilted, 300-3000 and 200-4000 Hz), and none in the runtime on 60
station-minutes of the simulator's noise. No Bell 202 detection on 20
minutes of voice-like audio, Cairn frames at every rung, 1000/1750 Hz
tones, DTMF, CW or steady 1200/2200 Hz tones. Limits: a sudden 3 dB or
larger drop in the noise level reads as a carrier about one time in four
(slow drift does not); bright speech gives 100-180 ms quieting dropouts
during a voice carrier; a squelch that closes reads as a carrier until the
60 s re-learn. A Cairn frame's own spectrum leaks into the quieting band
about 14 dB below its mean (19 dB just at its edge, which the band now
clears by 100 Hz), so quieting holds through the peer's frame only when
the open-squelch noise is well above that; the
preamble correlator covers the frame in any case. `phy/tests/dcd.rs` has
the tests (13 in the default run, three 10-minute runs `--ignored`).

### Testbed notes for the runtime (AIOC, no radio attached)

Before the interfaces went to live radios: `run` started on the AIOC
(i16 mono, 48 kHz, 25 ms output latency) and idled with the quieting
detector unarmed (no open-squelch noise at -71 dBFS) and no carrier-detect
rise; `connect N0NONE-2` to a fictitious callsign sent CONNECT (576 ms
keyed, TXDelay 150.0 ms, TXTail 10.2 ms at the DAC) four times (the
attempt and three retries, ADR 0010 `connect-retry-schedule`), 2.3 s keyed
in all, then reported "no answer after three retries" and stayed unkeyed;
a KISS client on port 8001 sent an AX.25 frame and the modem sent it as a
UI frame (813 ms keyed). The planned 60 s idle check with PTT wired was not
run (the cable was moved to live radios).

### Real-time audio: priority, buffers, gaps and receive gain

- **Priority** (`cairn_audio::rt`). The capture and playback threads ask
  for `SCHED_FIFO` (`--rt-prio`, default 60), the processing threads (the
  event loop that detects, decodes and runs the link layer; `listen`'s
  processing and decoder threads; the transmit thread) for `SCHED_RR` 20
  below: directly when `RLIMIT_RTPRIO` allows it (`@audio - rtprio 95`),
  else through rtkit on the system bus (via `busctl`; audio threads get up
  to rtkit's 20, processing threads nice -10, so rtkit's `RLIMIT_RTTIME`
  watchdog never meets the decoder), else a lower nice value if allowed,
  else one warning line and normal priority. Nothing of it runs in an
  audio callback: a cpal callback publishes its thread id once and the
  opener promotes it. The decoder's utilisation clamp (`boost`) stays.
- **Buffers.** cpal sizes the ALSA buffer at two periods, so a capture
  thread that woke more than about 10 ms late lost audio. Capture now
  opens the PCM itself (`audio/src/alsa_in.rs`: blocking `snd_pcm_wait`
  and `snd_pcm_readi` of one period into a preallocated buffer, capture
  time from the status timestamp less the frames still waiting, EPIPE
  recovered with prepare and start): 10 ms periods in a 200 ms buffer by
  default (`--period`, `--buffer-ms`; the line at start shows what ALSA
  granted). The buffer adds no latency: each period is taken as it
  completes. Playback stays on cpal (two periods; its thread is real-time).
  The capture ring between the audio thread and the processing thread
  holds 8 s, and its high-water mark is in `status` (line and API): the
  processing thread never blocks the audio thread, it can only fall behind,
  and the mark says by how much. Control clients that stop reading are
  dropped after 200 ms instead of stalling the event loop.
- **Gaps** (`cairn_audio::AudioGap`). When the capture timestamps jump
  (an xrun, a buffer lost) or the ring was full, the audio thread puts
  exactly the missing number of zero samples in the stream and reports the
  interval, so stream index *i* is always the *i*-th sample period: later
  frames decode at their true times. The listener keeps the intervals; a
  frame across one is flagged (`FrameReport::gap`, `FrameInfo::audio_gap`,
  `Output::HeaderFailed::audio_gap`; `AUDIO GAP 26.6 ms at +1.234 s` on its
  line, printed by `run` also without `-v`; `audio_gap` in the API's `link`
  and `rx` events, `link` `header_failed`). A frame whose header was lost
  is a header failure toward the link layer like any other; nothing
  changes on the air.
- **Receive AGC** (`run --rx-agc`, `agc::RxAgc`, SPEC 7.1 and 13.5). The
  runtime measures each received frame's level and 0.5 s windows of idle
  input; the controller steps the capture gain by 3 dB toward open-squelch
  noise at -20 dBFS RMS (dead band +-3 dB; without a noise reference,
  squelch closed, the data's RMS toward -18 +- 4 dBFS), never letting frame
  peaks pass -3 dBFS or the data fall under -30 dBFS, stepping down at once
  (2 s) on clipping or hot peaks. Only while the receiver is idle (no
  carrier, nothing detected, queued or on the air, not keyed) and not
  within 300 ms of a frame's end; at most one step per 10 s; two windows
  in a row must agree; measurements from before a step and the second
  after it are discarded. Carrier detect is told (`Dcd::level_step`: the
  quieting floor moves with the gain and 100 ms around the step are
  ignored; untold, 6 dB steps gave 29-33 false quieting asserts in 2
  minutes of noise, told none). The control is a `MixerControl`: ALSA's
  capture volume of the input's card ('AIOC Audio In' on the AIOC, or
  `--rx-agc-control`), in dB where the card gives a dB scale; there is no
  way to reach a playback control through it. Changes print as `rx gain
  +0.0 -> -3.0 dB: open-squelch noise -13.1 dBFS, above -17`; the gain is
  in `status` and the API's `status` (`audio.rx_gain`) and `config`
  (`rx_agc`, `rx_gain_db`).
- **Under load** (`cargo run --release -p cairn-modem --example rx_load --
  --burn N --matrix`): the full receiver on audio played at real-time pace
  through the mock capture device, whose audio thread wakes once a period
  like ALSA's and overruns when it wakes later than its buffer allows,
  while N threads burn CPU at normal priority.

  60 s of synthetic UI broadcasts (MCS 0-4, widths 24 and 32, 25 dB SNR)
  per configuration; "old" is the capture path before this change (no
  real-time priority, two 10 ms periods of buffer); "rt" is real-time
  priority, "200 ms" the native ALSA buffer. On a KVM guest with 16 vCPUs
  (the build host), already at load 16-30 from other jobs, plus 16
  burners; priority through `RLIMIT_RTPRIO` 95 (audio `SCHED_FIFO` 60,
  processing `SCHED_RR` 40), two runs:

  | Configuration | xruns (ms lost) | Frames with a gap | Ring high-water | Decode latency p50/p99 | Turnaround p50/p99 |
  | --- | --- | --- | --- | --- | --- |
  | old (20 ms, no rt) | 0 (0) / 1 (20) | 0 / 1 | 20 / 30 ms | 25/43 / 25/38 ms | 33/54 / 30/47 ms |
  | 200 ms, no rt | 0 / 0 | 0 / 0 | 190 / 20 ms | 23/71 / 25/34 ms | 27/80 / 31/43 ms |
  | rt, 20 ms | 11 (680) / 6 (400) | 7 / 6 | 230 / 170 ms | 13/19 / 14/17 ms | 19/87 / 19/28 ms |
  | rt and 200 ms (new default) | 0 / 0 | 0 / 0 | 60 / 140 ms | 14/18 / 13/40 ms | 20/27 / 19/88 ms |

  The guest loses its vCPUs to the host now and then (steal time) for
  tens of milliseconds whatever the thread's priority: real-time priority
  halves the decode latency but cannot prevent those stalls, and with a
  20 ms buffer every one was an overrun (about 65 ms lost each); the
  200 ms buffer absorbed all of them. On this laptop (8 cores, bare metal,
  rtkit: audio `SCHED_RR` 20, processing nice -10) neither 16 spinning
  threads nor a `cargo build -j16` loop of the workspace caused a single
  overrun in any configuration (turnaround p99 20-29 ms). With 16 threads
  each also mapping, touching and unmapping 256 MB in a loop (`--mem 256`,
  the page-fault and TLB-shootdown load of parallel compiles; 30 s runs),
  still no overrun, but the receiver itself slowed: turnaround p50/p99
  113/432 ms on the old path, 88/250 ms with the buffer alone, 86/144 ms
  with priority alone and 78/147 ms with both, so real-time priority is
  what keeps the 100 ms budget of SPEC 7.3 in sight under memory pressure.

## Operator experience: config, rig profiles, setup, self-test, unattended

SPEC 13.4 and 13.6 (v0.5 in 16.2). The procedures are in
[docs/operator-guide.md](../docs/operator-guide.md); this is how it is
built.

**Config file** (`cli/src/config.rs`). TOML, sections `[station]`
(callsign, grid, rig), `[audio]` (device, period, buffer_ms, backend, rt,
rt_prio, rx_agc, rx_agc_control, output_control, output_db), `[ptt]`
(method as `--ptt`, line, invert, gpio), `[timing]` (txdelay_ms,
txtail_ms), `[link]` (cap, no_mask, persistence, slot_ms, max_frame,
broadcast_mcs, quieting, refuse, id_broadcasts), `[host]` (kiss, api,
api_socket, control, rx_dir, capture_dir, heard_file, ring_seconds,
capture_on_failure, log_file, log_max_kb, log_keep) and `[unattended]`
(below). Unknown keys are errors. The layers merge key by key: built-in
defaults < rig profile < config file < flags and their environment
variables (clap's value source decides whether a flag was given).

**Rig profiles** (`docs/rigs/*.toml`, compiled in; `~/.config/cairn/rigs/`
adds or replaces by name). A profile is a `[profile]` table (name,
description, `status = "measured"` with `measured_on`, or `"unverified"`;
interface, `usb_ids`, `device_match`, `serial_match`, squelch,
carrier_tail_ms, path, passband_hz, weak_tones, notes) plus the same
`[audio]`, `[ptt]`, `[timing]` and `[link]` sections as the config.
Shipped: `aioc-ht` (the field-test values: `front:CARD=AllInOneCable,DEV=0`,
serial PTT with DTR high and RTS low, TXDelay 300 ms, TXTail 20 ms, cap 32,
'AIOC Audio Out Volume' about -9.5 dB, carrier tail 0.7 s, passband
350 Hz-4.25 kHz, tones 0-2 weak; measured on one pair of handhelds),
`digirig` (serial RTS), `signalink` (VOX: refuses sessions) and
`cm108-gpio` (`cm108:auto`, GPIO 3), the last three unverified defaults.
`cli/src/detect.rs` finds PTT candidates from sysfs and
`/dev/serial/by-id` without opening anything, and names known interfaces
by USB ID.

**Setup and self-test** (`cli/src/setup.rs`, `cli/src/selftest.rs`) run
over two traits (`cli/src/hw.rs`): `Terminal` (say, ask) and `Hardware`
(list devices, open input and output, measure the input, read the PTT
line idle, real-time check, capture control, port check, write a file;
and, taking a `Confirmed`, key the PTT, transmit, transmit and listen).
`Confirmed` has a private field; `confirm_keying` is the only constructor,
after stating what goes out, for how long and the identification it
carries, and only on a typed `yes`. The binary's `realhw.rs` is the real
implementation; the tests drive the wizard end to end on a scripted
terminal and a recording mock (all transmissions confirmed, all declined,
`y` instead of `yes`, end of input, bad answers, a new profile, an
existing config), and the self-test on good and broken stations, with
and without `--transmit`. The setup writes only what differs from the
chosen profile (`FileConfig::minus`). The self-test's reference recording
(`fixtures/rx/hdr_mic_w27_ui_broadcast_snr14.wav`, compiled in) goes
through a mock capture device into a `Modem`, the live receive path.

**Unattended operation** (`cli/src/unattended.rs`, wired in `driver.rs`):
patterns expand to addresses for the link layer's new allow list
(`cairn_link::Config::allow`) and its deny list; a `SessionClock`
disconnects a session past `max_session_min` (default 30; a resume keeps
the clock); a `TxLedger` counts keyed time per UTC day (default 60 min,
persisted in `budget_file`): under the reserve (30 s, or a quarter of a
small budget) the station refuses sessions and closes the open one, at
zero it keys nothing more that day. `id_broadcasts` (the link's
`Config::id_broadcasts`, on with `--unattended`) puts the ID block in a UI
frame when 9 minutes have passed since the station's last one; a repeat
keeps its original's blocks. The watchdog (every `run`, `--no-watchdog`
to drop it outside unattended mode) wraps the PTT (`WatchedPtt`); its
thread releases the line itself and stops the modem when a transmission
stays keyed past its keyed time plus `watchdog_margin_s` (2 s), or no audio
arrives for `audio_stall_s` (5 s; a hung event loop stops the audio
clock too). Hooks run the configured program with its configured
arguments, never through a shell; the event's data go in `CAIRN_EVENT`,
`CAIRN_TIME`, `CAIRN_STATION`, `CAIRN_PEER`, `CAIRN_SESSION_ID`,
`CAIRN_RESUMED`, `CAIRN_INITIATOR`, `CAIRN_REASON`, `CAIRN_FILE`,
`CAIRN_OBJECT`, `CAIRN_BYTES`, `CAIRN_SHA256`, `CAIRN_BROADCAST`
(printable ASCII only) and a received message's text on stdin; at most four
run at once, each killed after `timeout_s` (60 s). `--log-file` (or
`host.log_file`) copies every output line, UTC-stamped, to a file rotated
by size.

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
clip-and-filter), demodulates it with its own window, and fits a gain per
tone over the symbols after the preamble whose content is known (probe, a
decoded header, control and data symbols whose blocks all decoded), with
the pilots' phase track refined against the rebuilt tones and a gain of
their own for the symbols before the eighth, where the radios' level may
still be settling (ADR 0006 interpretation `level-settling-at-frame-start`);
the same fit as the simulator's `frame.channel_snr`, fallback rules
included. `tests/loopback.rs` checks it within 1 dB from 12 to 45 dB at
MCS 0, 4 and 6 (measured within 0.3 dB).

The sample-clock drift comes from the continuous pilots' phase slopes, with
a variance from their residuals; it is combined with a prior
(`ClockPrior`: 0 +- 50 ppm by default, or what the link layer knows of the
peer, through `RxOptions::clock_prior` or `LinkSource::clock_prior`) by
their variances and bounded at +-150 ppm, so a header-only frame (where
two pilots over seven symbols measure it to +-300 ppm) keeps the prior and
a 4 s frame measures it to under 1 ppm. `RxResult::clock_ppm` gives the
estimate and its standard deviation.

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
- `audio/src/capture.rs`, `audio/src/mock.rs`: a timestamp jump becomes
  exactly the missing zeros with later samples at their true index; a full
  ring (the reader stalled) keeps alignment too, the gaps reported before
  their zeros; buffers dropped from the mock device come back as zeros at
  their place; the paced mock keeps real time. `alsa_in.rs` opens ALSA's
  software `null` PCM (`--ignored`: needs an ALSA configuration; no sound
  card). `rt.rs`: priorities, the report text, `--no-rt`.
- `cli/tests/gaps.rs`: five UI broadcasts through the mock capture device
  with 26.6 ms dropped inside frame 1's data, 50 ms between frames 2 and 3,
  and frame 4's whole header dropped: frames 0-3 decode at their sent
  sample positions (within 4 samples; without the zeros the later ones
  would be 1277 and 3677 samples early) and on the modem's clock, only
  frame 1 is flagged (offset and length exact), frame 4 is one header
  failure, flagged; through `listen::run` and through the `Modem` runtime.
- `cli/src/agc.rs`: the AGC controller on synthetic level sequences: the
  bench's -12 dBFS noise steps down into the band and stays; quiet noise
  steps up only as far as the peaks allow; clipping steps down within 2 s,
  twice; nothing while busy or within 300 ms of a frame; stable or wobbling
  input never moves; one odd window is not enough; squelch closed uses the
  data; down steps keep the data above -30 dBFS; nothing at the end of the
  range; measurements from before a step are discarded.
  `phy/tests/dcd.rs` `quieting_follows_agc_gain_steps`: 3 and 6 dB steps
  every 5 s for 2 minutes of open-squelch noise, applied 30 ms after the
  detector is told, give no quieting assert.
- `cli/tests/api.rs`: two complete modems in the two-station simulator,
  each behind an API server, driven only by clients on real sockets (TCP
  for one, a Unix socket for the other): connect, a 9000-byte file and a
  message, `rx`, `session` (states, progress, deliveries), `link`, `heard`,
  `channel`, `dcd` and `tx` events, `heard`, `status`, a capture at the
  receiver, disconnect; a second client subscribed to `heard` gets nothing
  else. Every line the modems send (about 200: hello, responses, events) is
  validated against `docs/api.schema.json`. Also: `calibrate` and zstd
  answer `unsupported`, every malformed input (bad JSON, a non-object, a
  missing `cmd`, a bad `id`, non-UTF-8, a line over 1 MiB, unknown
  commands, out-of-range and read-only `config` values, an atomic `config`
  failure) gets its error and the connection keeps working; `capture`
  without a ring is `unsupported`; a capture's WAV equals, sample for
  sample, the audio pushed into the modem.
- `api/`: the server's framing (CRLF, blank, over-long and non-UTF-8
  lines), hello-before-events, subscriptions, several clients, TCP and Unix
  sockets; the heard list, its file; the ring buffer; base64 and RFC 3339
  times; the schema validator against `docs/api.schema.json`.
- `tests/test_api_schema.py` (repository root, `uv run pytest`): the schema
  is valid draft 2020-12, every example in `docs/api.md` validates (and
  there is one for every command and event), invalid messages fail, and
  `tools/api_client.py` sends only valid requests to a stand-in modem.

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
  protocol's channel SNR (ADR 0006, mask boost counted), the same fit in
  both, agrees within 0.01 dB (measured: under 0.001 dB). Six `hdr_*` fixtures (mic and repeater profiles, AWGN at 30
  tones with a probe, mask and two control blocks, a header-only PROBE,
  and two wide UI broadcasts, 33 tones in AWGN and 27 through the mic
  path) carry real headers and control blocks and are decoded header first
  by both receivers: same configuration from the header (and the same
  width from the probe for the broadcasts), same control blocks, per-tone
  SNR within 1e-13 dB. Rust clip-and-filter matches the simulator to 3e-15.
- `tests/loopback.rs`: Rust TX to Rust RX, every rung at widths 24-56,
  both prefixes, with and without masks; header-only frames; the channel
  SNR; short frames (header only, a control block, one data block) with the
  received level settling over the first symbols as over the air: clock
  estimate near its prior, channel SNR within 1 dB.
- `tests/offair.rs`: real frames from the first two-radio session
  (`fixtures/offair/`, cut by `tools/cut_offair_fixtures.py`; see its
  README): every frame type decodes in both directions (but a one-block
  frame whose capture lost 26.6 ms to an input xrun); short frames' clock
  offset consistent with the direction's 4 s frame (within 5 ppm given it
  as the peer's prior); their SNR within 1.5 dB of the 4 s frame, never
  more than 1 dB above it.
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
