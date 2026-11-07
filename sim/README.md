# cairn-sim: Stage 1 simulation

The offline Python/NumPy model of the Cairn transmit chain, FM radio channel
and receiver described in `docs/SPEC.md` §14.2 and §21.2. Its job is to find
where the specification's nominal numbers are wrong before any radio is
involved; v0.2 is reached when the measured rung thresholds are within 3 dB
of the §6.2 table and the test vectors are published.

```sh
uv sync --all-packages
uv run --package cairn-sim pytest sim/tests -m "not slow"
```

## Modules

| Module | Contents |
| --- | --- |
| `params` | Every protocol number, mirrored from the specification |
| `crc` | CRC-16/CCITT-FALSE, bit-serial and batched |
| `ldpc` | NR BG1/BG2 lifted codes from `codes/`, encoder, rate matching, layered normalized min-sum decoder |
| `mapping` | Scrambler, interleaver, Gray QAM mapper and max-log demapper (**provisional**, see below) |
| `ofdm` | Half-bin OFDM modulator/demodulator, clip-and-filter PAPR control |
| `preamble` | S1/S2 sequences (**provisional** until the Stage 1 search) |
| `frame` | Frame assembly and the thin-slice receiver |
| `channel` | AWGN and the FM audio-chain model |

## Provisional choices

SPEC §6.4 defers the scrambler, interleaver and constellation labeling to an
ADR due before v0.2, and §20.3 has S1 chosen by a Stage 1 search. Until then
the simulation uses the placeholder designs documented at the top of
`mapping.py` and `preamble.py`. They meet the normative properties and are
meant to be replaced by whatever the ADR specifies.

## Audio SNR

Signal power over noise power in the occupied band (300 Hz to the top active
tone + 50 Hz), at the receiver's audio output. With tones filling the band,
it equals the per-tone SNR.
