#!/usr/bin/env bash
# File-based end to end for the Rust modem, from the repository root:
#   bash modem/tools/e2e_wav.sh [OUTDIR]
# For each configuration: cairn-modem tx --wav (real headers, callsign
# N0CALL), then
#   1. cairn-modem listen --wav on the clean WAV, header first (every frame,
#      every block, every control block; broadcasts take their width from
#      the probe, session frames from --width/--mask),
#   2. the simulator's FmChannel (mic profile, 25 dB, +40 ppm; flat25 at
#      30 dB over the frame's width for the wide ones) and listen again,
#   3. cairn-bench rx (the simulator's receiver) on the clean WAV.
# Then the old random-header frames with listen --fixed.
set -euo pipefail
OUT=${1:-modem/target/e2e}
mkdir -p "$OUT"
cargo build --release --manifest-path modem/Cargo.toml -p cairn-modem -q
BIN=modem/target/release/cairn-modem

# run NAME PROFILE[:SNR[:WIDTH]] "TX-ONLY-ARGS" "LISTEN-ONLY-ARGS" FRAME-ARGS...
# (the FmChannel SNR is measured over WIDTH tones, default 25 dB over 24)
run() {
    name=$1
    IFS=: read -r profile snr swidth <<< "$2"
    snr=${snr:-25}
    swidth=${swidth:-24}
    txonly=$3
    rxonly=$4
    shift 4
    echo "== $name: $* $txonly | listen $rxonly"
    # shellcheck disable=SC2086
    "$BIN" tx --wav "$OUT/$name.wav" --count 3 --gap 0.7 $txonly "$@" > /dev/null
    # shellcheck disable=SC2086
    "$BIN" listen --wav "$OUT/$name.wav" --stats 0 --expect 3 $rxonly "$@" | tail -1
    uv run --package cairn-sim python modem/tools/fm_channel_wav.py "$OUT/$name.wav" "$OUT/$name-fm.wav" \
        --profile "$profile" --snr "$snr" --width "$swidth" --ppm 40 > /dev/null
    # shellcheck disable=SC2086
    "$BIN" listen --wav "$OUT/$name-fm.wav" --stats 0 --expect 3 $rxonly "$@" | tail -1
    uv run --package cairn-sim cairn-bench rx "$OUT/$name.wav" | grep -E "headers decoded"
}

C="--callsign N0CALL-1"
run ui-mcs0 mic "$C --id --grid FN31" "" --mcs 0
run ui-mcs2-tone mic "$C --tone 1 --message HELLO" "--text" --mcs 2
run beacon-ui mic "$C --type beacon" "" --mcs 0
run ui-w40-mcs4 flat25:30:40 "$C --id" "" --mcs 4 --width 40 --blocks 10
run ui-w56-mcs5-long flat25:30:56 "$C" "" --mcs 5 --width 56 --prefix long --blocks 8
run data-w40-mask flat25:30:40 "$C --type data --to N1CALL" "" --mcs 2 --width 40 --mask 8-11,30-33 --blocks 12
run random-mcs2 mic "--random-header" "--fixed" --mcs 2 --blocks 4
echo "all configurations decoded"
