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
