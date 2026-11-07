# Cairn modem (Stage 2, Rust)

The real-time modem of SPEC §21.3. This first step is the physical layer
only, ported from the Stage 1 simulator (`sim/cairn_sim/`) and checked bit
for bit against the published vectors: samples in, samples out, at 48 kHz.
Audio I/O, PTT, the link layer and the host interfaces come next.

```sh
cd modem
cargo test                                   # vectors, fixtures, loopback, spec tables
cargo clippy --all-targets -- -D warnings
cargo run --release --example bench          # single-core timing of 4 s frames
```

## Layout

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
