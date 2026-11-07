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
| `ldpc_eval` | Code performance against the normal-approximation bound and a sum-product reference |
| `mapping` | Scrambler, interleaver, Gray QAM mapper and max-log demapper (**provisional**, see below) |
| `ofdm` | Half-bin OFDM modulator/demodulator, clip-and-filter PAPR control |
| `preamble` | S1/S2 (chosen by the Stage 1 search) and the SPEC 6.1 dual-lag detector |
| `preamble_eval` | Detection, false-alarm and prefix-identification measurements |
| `frame` | Frame assembly (preamble, header, width probe, data blocks) and the receiver |
| `channel` | AWGN and the SPEC 14.2 FM audio chain (`mic`, `flat12`, `flat25`, `repeater`) |
| `link_sim` | Block error rate against SNR, rung thresholds, and the SPEC 7.4 time budget |
| `experiments` | Runs the threshold sweeps in parallel, writes `results/*.json` |
| `report` | Builds `report.md` from the results |

```sh
uv run --package cairn-sim python -m cairn_sim.experiments awgn      # also: channels, clip
uv run --package cairn-sim python -m cairn_sim.report
uv run --package cairn-sim python -m cairn_sim.vectors               # regenerate codes/ and vectors/
```

## Provisional choices

SPEC §6.4 defers the scrambler, interleaver and constellation labeling to an
ADR due before v0.2. Until then the simulation uses the placeholder designs
documented at the top of `mapping.py`. They meet the normative properties and
are meant to be replaced by whatever the ADR specifies.

S1 and S2 (§20.3) are the Stage 1 search results; the top of `preamble.py`
says exactly how they were chosen, for publication as test vectors.

## Audio SNR

Signal power over noise power in the occupied band (300 Hz to the top active
tone + 50 Hz), at the receiver's audio output. With tones filling the band,
it equals the per-tone SNR.
