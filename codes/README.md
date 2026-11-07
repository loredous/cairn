# LDPC base graphs

`bg1.csv` and `bg2.csv` are the 5G NR LDPC base graphs from 3GPP TS 38.212
V19.4.0, Tables 5.3.2-2 (BG1: 46 rows, 68 columns, 316 entries) and 5.3.2-3
(BG2: 42 rows, 52 columns, 197 entries). Each row gives a base-graph entry's
row and column index and its shift coefficient V for lifting-size set
indices 0-7. They were extracted from the specification document and checked
entry by entry against an independent transcription.

Cairn lifts them at Z = 48 (BG1, set index 1, data blocks), Z = 26 (BG2, set
index 6, control blocks) and Z = 15 (BG2, set index 7, headers); the shift is
V mod Z (SPEC 6.3, 20.2). The lifted matrices, rate-matching tables and
known-answer vectors that v0.2 publishes are generated from these files by
`sim/`.

## Published files (generated)

Regenerate with `uv run --package cairn-sim python -m cairn_sim.vectors`; the
output is deterministic and a test checks the committed files are current.
`<stem>` is `data_bg1_z48`, `control_bg2_z26` or `header_bg2_z15`.

- `<stem>_lifted.txt`: the lifted code in compact form. `#` lines are
  comments; every other line is `block_row block_col shift`. Block (i, j) is
  the Z x Z identity matrix with the one of row z in column
  `j*Z + (z + shift) mod Z`, and shift = V mod Z.
- `<stem>.alist`: the full parity-check matrix H in MacKay's alist format
  (1-based indices; N = base columns x Z, including the 2Z punctured and the
  filler columns; M = base rows x Z). Columns 0..K'-1 are information bits,
  K'..K-1 are shortened filler zeros, K..N-1 are parity.
- `<stem>_ratematch.txt`: for each on-air length E, a `length E` line then the
  E 0-based codeword bit indices in transmission order, 32 per line. This is
  38.212 5.4.2 rv0: the first 2Z bits are punctured, fillers are skipped, and
  the bits form a circular buffer; every length is a prefix of the buffer,
  and a retransmission continues at buffer index E.

Known-answer encodings are in `vectors/`.
