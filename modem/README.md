# Cairn modem (Stage 2, Rust)

The real-time modem of SPEC §21.3. Done so far: the physical layer, ported
from the Stage 1 simulator (`sim/cairn_sim/`) and checked bit for bit
against the published vectors, and the real-time layer around it (steps
1-4 without the link layer): audio I/O at 48 kHz, PTT backends with
measured TXDelay and TXTail, a continuous receiver and a transmitter, in
the `cairn-modem` command. The link layer and the host interfaces come
next.

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
$M tx -d All-In-One-Cable --ptt serial:All-In-One-Cable --mcs 2 --blocks 3
$M listen -d All-In-One-Cable --mcs 2 --blocks 3            # Ctrl-C to stop
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
- **`tx`** builds frames with `cairn-phy`: `--mcs`, `--width`, `--prefix
  short|long`, `--mask 8-11,30` (tones silenced in data symbols),
  `--blocks N` or `--frame-s S` (default SPEC 10.2: 2 s at MCS 0-1, 4 s
  above, block count as `cairn-bench tx` computes it), `--count`, `--gap`
  (unkeyed seconds between transmissions), `--tone S` (the level-setting
  tone first), `--seed`. Each frame is its own keyed transmission, peak at
  -3 dBFS (`TxFrame::output`). Header and payload bits are the
  simulator's random ones for the seed (`frame.build(cfg,
  default_rng(seed * 1000 + i))`, NumPy's generator reimplemented bit for
  bit); real header fields wait for the link layer.
- **`--wav out.wav`** (tx and tone) writes the signal instead of playing
  it, with a `cairn-bench/1` manifest next to it (`out.json`), so
  `cairn-bench rx out.wav` scores it exactly as its own recordings,
  header and block bits included.
- **`listen`** prints one line per frame (time, prefix, MCS and width,
  header OK, blocks OK/total, the ADR 0006 channel SNR on the 7 dB scale,
  sample-clock offset in ppm as `cairn-bench rx` reports it, detector rho,
  decode time) and every `--stats` seconds the input level, capture
  counters, queue depth and totals. `--wav in.wav` runs the same code on a
  file as fast as it goes; `--record cap.wav` saves what the sound card
  captured; `--expect N` fails unless exactly N frames decode completely
  (for scripts). Until the header carries the frame's fields, the frame
  configuration comes from the same `--mcs/--width/--blocks/--mask` flags
  as `tx` (the prefix from the detector unless `--prefix` forces it).
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
- **Listen** (`listen::run`): the processing thread takes 50 ms chunks
  (sound card or file, the only difference), runs the streaming detector
  across chunk boundaries, keeps enough history for every pending frame,
  and once a frame's last sample has arrived hands it to a decoder thread
  (never the audio callback). Where the configuration comes from is the
  `ConfigSource` trait: `FixedConfig` (command line) today; a header-first
  source returns the configuration the header names, after a lookahead of
  preamble plus header.
- On the AIOC at idle, `listen` uses about 9% of one core (the detector
  rescans its 12k-sample window every chunk); a 4 s MCS 2 frame decodes in
  about 20 ms clean and 60-90 ms through the FM channel model, so a result
  follows the frame's last sample by at most one chunk (50 ms), 25 ms of
  margin and the decode. The 100 ms receive turnaround of SPEC 7.3 will
  want smaller chunks or an incremental detector.

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
| `detector` | SPEC 6.1 dual-lag detector with S1 cross-correlation and the ±100 ppm grid, one-shot and streaming |
| `frame` | Frame geometry: segments, tones, masks, gains (ADR 0004, 0005) |
| `tx` | Frame builder, mirroring `frame.build` |
| `rx` | Receiver, mirroring `frame.receive` with its defaults (header-noise method "measured", decision-directed noise, burst-aware LLRs), plus the ADR 0006 channel SNR from rebuilt symbols |
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

Like the simulator, the receiver takes the frame configuration (width,
rung, block count, prefix, mask) from the caller; decoding the header
fields to get it is link-layer work still to do.

## Verification

Real-time layer:

- `cli/src/txseq.rs`: a virtual-clock output device and a logging PTT
  check the order key, TXDelay, audio, drain, TXTail, unkey (first sample
  within a sample of TXDelay, release TXTail after the last sample's DAC
  time, every sample played while keyed), underrun counting, and release
  on an audio error, a panic and a stop signal.
- `cli/src/listen.rs`: three frames straddling chunk boundaries at
  different phases, the last ending 100 samples before the input, all
  decode; ten seconds of noise give no detection.
- `cli/src/nprng.rs`: the NumPy generator against values recorded from
  NumPy 2.5; `cairn-bench rx` scores `tx --wav` output with every header
  and block bit matching.
- `ptt/`: guard release on drop, error and panic; the rigctld protocol
  against a fake server; serial line bits; CM108 reports.
- `modem/tools/e2e_wav.sh`: MCS 0, 2 (with the tone), 4, 6 with the long
  prefix, and 40 tones with a mask: every frame and block decodes from
  the clean WAV and through FmChannel (mic, 25 dB, +40 ppm, reported as
  +39 to +40 ppm), and in `cairn-bench rx`.

Physical layer (paths under `phy/`):

- `tests/ldpc_vectors.rs`: every `vectors/ldpc_*.json` vector (CRC, full
  codeword, every rate-matched length), the rate-matching tables, the
  lifted files against `codes/bg*.csv`, and the alists.
- `tests/bitmap_vectors.rs`: every stage of all 13 `vectors/bitmap/`
  frames: info, rate-matched, scrambler, scrambled and interleaved bits,
  padding, I/Q levels, segment geometry and gains, the tone grid (within
  its 9 printed decimals) and the three sample streams (within 6).
- `tests/rx_fixtures.rs`: fixtures from `tools/gen_rx_fixtures.py` (clean,
  AWGN, mic, repeater with long prefix, masked, ±100 ppm, noise burst) as
  16-bit WAV. The Rust detector finds the simulator's starts, prefixes and
  clock-grid points; the receiver's per-tone SNR agrees to 1e-12 dB and it
  decodes the same blocks in the same number of LDPC iterations. Rust
  clip-and-filter matches the simulator to 3e-15.
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
