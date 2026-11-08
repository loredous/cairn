#!/usr/bin/env bash
# Receive turnaround (SPEC 7.3: at most 100 ms from a frame's last sample to
# its decoded result), from the repository root:
#   bash modem/tools/turnaround.sh [OUTDIR] [FRAMES]
# For 24 and 56 tones at MCS 0, 2, 4 and 6 (SPEC 10.2 frame lengths, real
# headers): cairn-modem tx --wav, then cairn-modem listen --wav --realtime,
# which plays the file at real-time pace in 10 ms chunks through the same
# code as the sound card and reports p50/p99 of the turnaround; clean, and
# through the simulator's FmChannel (mic at 24 tones, flat25 over the width
# at 56) 3 dB above the rung's threshold, where decoding is slowest.
set -euo pipefail
OUT=${1:-modem/target/turnaround}
N=${2:-10}
mkdir -p "$OUT"
cargo build --release --manifest-path modem/Cargo.toml -p cairn-modem -q
BIN=modem/target/release/cairn-modem
declare -A THR=([0]=3.5 [2]=8.5 [4]=17.5 [6]=25.5)

for width in 24 56; do
    for mcs in 0 2 4 6; do
        name="w${width}-mcs${mcs}"
        "$BIN" tx --wav "$OUT/$name.wav" --callsign N0CALL-1 --width "$width" --mcs "$mcs" \
            --count "$N" --gap 0.5 > /dev/null
        if [ "$width" = 24 ]; then profile=mic; else profile=flat25; fi
        snr=$(python3 -c "print(${THR[$mcs]} + 3)")
        uv run --package cairn-sim python modem/tools/fm_channel_wav.py "$OUT/$name.wav" "$OUT/$name-fm.wav" \
            --profile "$profile" --snr "$snr" --width "$width" --ppm 20 > /dev/null
        for v in clean fm; do
            f="$OUT/$name.wav"
            [ "$v" = fm ] && f="$OUT/$name-fm.wav"
            echo "== $name $v ($profile $snr dB for fm)"
            "$BIN" listen --wav "$f" --realtime --stats 0 | tail -2
        done
    done
done
