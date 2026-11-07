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
| `rx` | Receiver, mirroring `frame.receive` with its defaults (header-noise method "measured", decision-directed noise, burst-aware LLRs) |
| `dsp` | Savitzky-Golay, convolution, interpolation, unwrap, median, Kaiser FIR with NumPy/SciPy semantics |
| `channel`, `wav` | AWGN test channel; 16-bit WAV reading and writing |

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
  both prefixes, with and without masks.

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
