#!/usr/bin/env bash
# File-based end to end for the Rust modem, from the repository root:
#   bash modem/tools/e2e_wav.sh [OUTDIR]
# For each configuration: cairn-modem tx --wav, then
#   1. cairn-modem listen --wav on the clean WAV (every frame, every block),
#   2. the simulator's FmChannel (mic profile, 25 dB, +40 ppm) and listen again,
#   3. cairn-bench rx (the simulator's receiver) on the clean WAV.
set -euo pipefail
OUT=${1:-modem/target/e2e}
mkdir -p "$OUT"
cargo build --release --manifest-path modem/Cargo.toml -p cairn-modem -q
BIN=modem/target/release/cairn-modem

# run NAME "TX-ONLY-ARGS" FRAME-ARGS...
run() {
    name=$1
    txonly=$2
    shift 2
    echo "== $name: $* $txonly"
    # shellcheck disable=SC2086
    "$BIN" tx --wav "$OUT/$name.wav" --count 3 --gap 0.7 $txonly "$@" > /dev/null
    "$BIN" listen --wav "$OUT/$name.wav" --stats 0 --expect 3 "$@" | tail -1
    uv run --package cairn-sim python modem/tools/fm_channel_wav.py "$OUT/$name.wav" "$OUT/$name-fm.wav" \
        --profile mic --snr 25 --ppm 40 > /dev/null
    "$BIN" listen --wav "$OUT/$name-fm.wav" --stats 0 --expect 3 "$@" | tail -1
    uv run --package cairn-sim cairn-bench rx "$OUT/$name.wav" | grep -E "headers decoded"
}

run mcs0 "" --mcs 0
run mcs2-tone "--tone 1" --mcs 2
run mcs4 "" --mcs 4 --blocks 10
run mcs6-long "" --mcs 6 --prefix long --blocks 8
run w40-mask "" --mcs 2 --width 40 --mask 8-11,30-33 --blocks 12
echo "all configurations decoded"
