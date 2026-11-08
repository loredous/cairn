# cairn-sim: Stage 1 simulation

The offline Python/NumPy model of the Cairn transmit chain, FM radio channel
and receiver described in `docs/SPEC.md` §14.2 and §21.2. Its job is to find
where the specification's nominal numbers are wrong before any radio is
involved; v0.2 is reached when the measured rung thresholds are within 3 dB
of the §6.2 table and the test vectors are published.

```sh
uv sync --all-packages
uv run --package cairn-sim pytest sim/tests          # add -m slow for the long statistical runs
```

## Modules

| Module | Contents |
| --- | --- |
| `params` | Every protocol number, mirrored from the specification |
| `crc` | CRC-16/CCITT-FALSE, bit-serial and batched |
| `ldpc` | NR BG1/BG2 lifted codes from `codes/`, encoder, rate matching, layered normalized min-sum decoder |
| `vectors` | Generates the published matrices, rate-matching tables and known-answer vectors (`codes/`, `vectors/`) |
| `bitmap_vectors` | Generates the ADR 0005 bit-mapping vectors (`vectors/bitmap/`): every stage from payload to samples |
| `fields` | Header (SPEC 8.2), address (B.1) and every Appendix B control block: pack, unpack, CRC and field checks |
| `frame_vectors` | Generates the header, control block and whole-frame vectors (`vectors/header/`, `control/`, `frame/`) |
| `ldpc_eval` | Code performance against the normal-approximation bound and a sum-product reference |
| `mapping` | Scrambler, interleaver, Gray QAM mapper and max-log demapper (ADR 0005) |
| `ofdm` | Half-bin OFDM modulator/demodulator, clip-and-filter PAPR control |
| `preamble` | S1/S2 (chosen by the Stage 1 search) and the SPEC 6.1 dual-lag detector |
| `preamble_eval` | Detection, false-alarm and prefix-identification measurements |
| `frame` | Frame assembly (preamble, header, width probe, control and data blocks, SPEC 5.4 tone masks) and the receiver, header first when not told the configuration |
| `channel` | AWGN and the SPEC 14.2 FM audio chain (`mic`, `flat12`, `flat25`, `repeater`) |
| `link_sim` | Block error rate against SNR, rung thresholds, and the SPEC 7.4 time budget |
| `interleaver_eval` | SPEC 6.4 interleaver checks: bit layout under tone masks, dead-tone and burst BLER |
| `experiments` | Runs the threshold sweeps in parallel, writes `results/*.json` |
| `report` | Builds `report.md` from the results |
| `bench` | The `cairn-bench` CLI: test WAV out (`tx`), offline decode of a radio recording (`rx`), optional `play`/`record`/`levels`/`ptt-test` |

```sh
uv run --package cairn-sim python -m cairn_sim.experiments awgn      # also: channels, clip, interleaver, header
uv run --package cairn-sim python -m cairn_sim.report
uv run --package cairn-sim python -m cairn_sim.vectors               # regenerate codes/ and vectors/
uv run --package cairn-sim python -m cairn_sim.bitmap_vectors        # regenerate vectors/bitmap/
uv run --package cairn-sim python -m cairn_sim.frame_vectors         # regenerate vectors/header/, control/, frame/
```

## Bench kit

`cairn-bench` generates a WAV of test frames, decodes a recording of it offline, and
(with the optional `audio` extra) plays and records through a sound interface with
serial PTT. It lets you test the waveform through two real radios without a real-time
modem; see [`docs/bench-testing.md`](../docs/bench-testing.md).

```sh
uv run --package cairn-sim cairn-bench tx --plan quick --callsign N0CALL -o quick.wav
uv run --package cairn-sim cairn-bench rx recording.wav --manifest quick.json
```

## Bit mapping

ADR 0005 made the simulation's scrambler, interleaver, constellation
labeling, bit-to-tone order, pilot, probe and preamble sequences and
negated-tail prefix normative (SPEC §5.2, §6.1, §6.4). The published vectors
in `vectors/bitmap/` come from `bitmap_vectors`, and a test checks that
they regenerate byte for byte, so a change to any of these is a change to
the protocol and needs a new ADR.

S1 and S2 (§20.3) are the Stage 1 search results; the top of `preamble.py`
says exactly how they were chosen.

## Clip level and tone masks

Frames are clipped at their data rung's level, `params.clip_db(mcs)`: 7 dB
above RMS at MCS 0–3, 8, 9 and 10 dB at MCS 4, 5 and 6. The whole frame,
preamble to data, uses that level. Results stay on the average-referenced
SNR axis below; add `params.clip_offset_db(mcs)` (`clip_db - 7`) to quote a
threshold on the 7 dB-clip scale a peak-limited radio sees.

`FrameConfig.mask` is the SPEC 5.4 set of usable tones. It applies to data
symbols only; see the top of `frame.py` for exactly what it changes.

## Audio SNR

Signal power over noise power in the occupied band (300 Hz to the top active
tone + 50 Hz), at the receiver's audio output. With tones filling the band,
it equals the per-tone SNR.

The SNR a receiver reports (`frame.channel_snr`, `RxResult.detail["snr_db"]`,
ADR 0006) is a different figure: the channel SNR over the active tones on the
7 dB-equivalent scale, measured against the frame rebuilt from the decoded bits
so the transmitter's designed clipping is not counted. In white noise it equals
the audio SNR above (plus `clip_offset_db`). With too few known symbols it
falls back to an estimate that counts the clipping and reads low
(`detail["snr_fallback"]`).
