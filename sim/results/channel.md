## Channel model (SPEC 14.2)

`channel.FmChannel` implements the §14.2 chain; every stage's parameters and
the reasoning behind them are in the `channel.py` docstring. Profiles used
in the threshold runs:

| Profile | Path | Width | Main impairments |
| --- | --- | --- | --- |
| `mic` | Mic in / speaker out (or 1200-baud port) | 24 | 750 µs pre/de-emphasis; 300 Hz (order 2) and 3000 Hz (order 6) band-pass at each end; soft limiter; drive 3 dB below the knee |
| `flat12` | 9600 port, 12.5 kHz channel | 30 | Discriminator noise rising 6 dB/octave; IF-limited top at 3300 Hz |
| `flat25` | 9600 port, 25 kHz channel | 56 | As `flat12`, top at 5900 Hz |
| `repeater` | Two mic chains, controller band-pass, 2 ms all-pass dispersion | 24, long prefix | Each hop contributes half the noise (§11.5's "about 3 dB") |

All profiles add DC (1% of the knee) and 60 Hz hum (−30 dB); runs use no
sample-clock offset (±100 ppm is covered separately: MCS 5 over 4 s frames
decodes with 0–1 errors in 140 blocks at ±100 ppm through `mic` at 30 dB).

What the chain does to the waveform, noise-free:

- **Mic path.** The 300 Hz high-pass's tail outlasts the 1.25 ms prefix, so
  the lowest tones see inter-symbol interference: about 15–20 dB SINR on
  tones 0–1 and 20+ dB from tone 2 up, against about 22 dB for the clipping
  floor elsewhere. It only matters at MCS 5–6.
- **Repeater.** Total group-delay spread is about 5 ms against the 2.5 ms
  long prefix. Tones 0–2 (350–550 Hz) fall to about −10 dB SINR and carry
  essentially nothing; tones 3–5 are degraded. The receiver's per-tone noise
  estimate gives those tones near-zero LLRs, so MCS 0–4 still decode, but
  MCS 5–6 do not.
