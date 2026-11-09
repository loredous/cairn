# Bench testing the Cairn waveform through real radios

This is a how-to for testing the Cairn modem waveform through two real FM radios
without a real-time modem. You generate a WAV of test frames, play it through
radio A, record what radio B hears, and decode the recording afterwards with the
Stage 1 receiver (`sim/cairn_sim`). It is the cheapest way to start on the Stage 2
bench plan of [SPEC §14.3](SPEC.md): real audio chains, real deviation, real
radios, no modem firmware.

The tool is `cairn-bench`, part of the simulation package:

```sh
uv sync --package cairn-sim                  # core: tx and rx
uv sync --package cairn-sim --extra audio    # adds play, record, levels, ptt-test (sounddevice, pyserial)
uv run --package cairn-sim cairn-bench --help
```

The `audio` extra is optional. Without it, play and record with any audio tool
(Audacity, `aplay`/`arecord`, `ffmpeg`) and key the radio with VOX; `tx` and `rx`
need nothing else.

## What you need

- Two FM radios, or one radio and a receiver, on the same channel. Use simplex.
  The transmitter should have low power or, better, no antenna at all (see below).
- A sound interface for each radio, or one interface per radio on one computer:
  a Digirig, an AIOC (All-In-One-Cable), a SignaLink-style box, or a CM108 USB
  sound-card cable. Any of them works for a bench test. Check that the interface
  gives you a *flat* audio path (see "Mic path or data port").
- A computer with the tool installed. One computer with two interfaces is
  simplest: it plays out of one and records from the other, and the recording and
  playback clocks are then different sound cards, which is a realistic test of the
  receiver's clock tracking.

### Wiring for the bench

Never connect one radio's antenna output straight to another radio's input. The
usual arrangements are:

1. **Dummy load plus attenuator.** Radio A transmits into a dummy load through a
   tee (or a directional coupler) and a step attenuator; radio B's antenna port is
   fed from the attenuator. A transmitter at 5 W (37 dBm) needs about 70 to 90 dB of
   total attenuation to bring radio B to a few microvolts. With a step attenuator you
   can sweep the received signal across the FM threshold, which is what Stage 2 asks
   for. Keep the attenuator and the coax rated for the power you use.
2. **Low power, short distance.** Lowest power setting, antennas a few metres apart
   (or in different rooms). No equipment needed, but the signal is not
   controllable, and you are on the air (see "Station identification").
3. **Both radios in a shielded setup.** A shielded box or tent works, with a small
   antenna near the receiver. Be careful about leakage.

## Mic path or data port

The waveform expects a *flat* audio path (SPEC §3.1). Radios give you three ways in:

- **Data/9600 port (flat).** No pre-emphasis in transmit, no de-emphasis in receive. This is the
  intended path where the radio has one, and the `flat12` and `flat25` channel profiles
  in the simulation model it. On receive the discriminator noise rises with frequency
  (the upper tones see lower SNR); the per-tone SNR plot shows it.
- **Mic and speaker path.** Pre-emphasis in transmit and de-emphasis in receive, a mic
  band-pass and a limiter. It works, and the simulation's `mic` profile models it, but
  the tone levels are no longer flat and the limiter is closer. Turn off any mic
  compressor, voice scrambler, VOX gain or "voice enhancement" on the radio.
- **1200 baud packet port.** Usually mic-like (with emphasis) at a fixed level.

Record which path you used next to every result. Results from the two are not comparable.

## Setting levels

Levels are the main way a bench test goes wrong. Set them in this order, with the
tone that `tx` puts at the start of the WAV (the **level-setting tone**, SPEC §7.1: 1000 Hz at the same
peak level as the data, 5 s by default, `--tone-s` to change).

### Transmit drive

SPEC §7.1: drive the radio so the tone produces **70% of maximum deviation**: 3.5 kHz on a
25 kHz channel and 1.75 kHz on a 12.5 kHz channel. The 30% headroom is for the
peaks that remain above the clip point.

1. Set the computer's playback volume (and the interface's output control, if it has one) to about half.
2. Put the radio in transmit into the dummy load. Play the WAV (or `play`; the tone comes first).
3. Read the deviation with a service monitor or deviation meter, and set the drive to
   3.5 kHz (or 1.75 kHz). If you have no deviation meter, use a second receiver with a
   discriminator output or an SDR with FM deviation display, or use the decode results as a
   check: raise the drive until the report's distortion index rises above 1 or the
   constellation compresses, then back off by 3 dB. That gets you close, no more.
4. Leave the radio's own mic gain, deviation limit and (on mic paths) any compressor alone from now on.

The tone is also the best check on the *file*: its peak is -3 dBFS (`--peak-dbfs`
changes it). A tone that plays quietly means the playback volume is low, not that
the file is.

### Receive level

The receiver accepts input from -30 to -3 dBFS (SPEC §7.1). Aim for the **tone peak
near -10 dBFS** in the recording, and never at full scale. Check with `levels`
(or any recorder meter) while radio A sends the tone, and set radio B's volume
(or the interface's input control) to suit. `rx` reports the level-tone peak and
flags recordings that reach full scale: a clipped receive path is the most common
cause of frames stuck at low rungs.

`cairn-bench levels --device NAME --seconds 2` prints RMS, peak and DC of the input.
With nothing connected, a clean interface reads very low (an AIOC reads about -46 dBFS RMS,
mostly a small DC offset). If your idle noise floor is much higher than that, the receive
level is too high or the interface is noisy.

## Keying: PTT or VOX

- **PTT**: `play` keys the radio through a serial control line (DTR or RTS on a USB serial
  port that the interface provides: Digirig, AIOC) for the whole WAV, with a `--txdelay` lead
  before the audio and `--txtail` after it. The AIOC default is DTR asserted and RTS
  deasserted, which is the default here. Use `--ptt-line rts` or `--ptt-invert`
  for other wiring. `ptt-test` keys for half a second to check the wiring. The tool forces
  both lines low before it opens the port and always releases PTT on exit, on an error, and
  on Ctrl-C, but check once with a dummy load connected.
- **VOX**: a sound-card interface with VOX, or the radio's own VOX. Radio VOX has an attack
  time that can eat the first 100 ms or more of audio, and a hang time. The WAV starts with
  1 s of silence and the level tone for exactly this reason: they open the VOX before the
  first frame. Between frames the default gap is 0.5 s; keep it *shorter than the hang time*
  so the radio stays keyed (increase the hang time, or reduce `--gap`). If VOX drops
  between frames, the next preamble is clipped and `rx` reports that frame as missed.
- **Time-out timer.** Most radios cut transmit after 1 to 3 minutes. `tx` warns when the
  WAV is longer than 2 minutes. Either split a long plan into several files (different
  `--seed`, so their payloads differ) or raise the radio's timer.

A keyed carrier with no audio is fine: a PTT radio sends the carrier during the gaps.

## Station identification

If any of this goes out on the air, you are a licensed station transmitting data and have to
identify (in the US, Part 97 requires your callsign at the end of the communication and at least
every 10 minutes during it). Pass your callsign:

```sh
cairn-bench tx --callsign N0CALL ...
```

That sends a CW ID (about 20 WPM, 800 Hz, at the same peak level as the data) at the start, at
least every 9 minutes within the file (the file's first and last signals are IDs), and at the end.
Never use `--callsign` for someone else's. Other administrations have their own rules,
so check yours. On a dummy load into an attenuator nothing is radiated and no ID is needed,
but there is no harm. The detector ignores the CW tone and the level tone (a steady tone is
never a preamble, SPEC §6.1).

## Step by step

```sh
# 1. Make the test WAV (and ladder24.json, the manifest, next to it).
uv run --package cairn-sim cairn-bench tx --plan ladder24 --callsign N0CALL -o ladder24.wav

# 2. Set levels with the tone (play it alone first if you like; see above).
uv run --package cairn-sim --extra audio cairn-bench levels --device "Digirig" --seconds 2

# 3. Record on radio B's interface, then play on radio A's. Two terminals, record first:
uv run --package cairn-sim --extra audio cairn-bench record --device "Digirig" --seconds 60 -o rec.wav
uv run --package cairn-sim --extra audio cairn-bench play ladder24.wav --device "AllInOneCable" \
    --ptt-port /dev/serial/by-id/usb-AIOC_All-In-One-Cable_*-if04

# 4. Decode and report.
uv run --package cairn-sim cairn-bench rx rec.wav --manifest ladder24.json --json report.json --plots plots/
```

Devices are chosen by a name substring (`cairn-bench devices` lists them), not by number. `rx`
reads any WAV: mono or stereo, 44.1 or 48 kHz or other rates (it resamples). For stereo it
uses the louder channel (`--channel left|right|mix` to override). Record for the whole
playback plus a few seconds each side; recording extra is harmless, and `rx` scans the
entire recording for preambles, so you can start recording early and stop late.

### Plans

| `--plan` | Frames |
| --- | --- |
| `quick` | MCS 0, 2, 4 at 24 tones (about 12 s) |
| `ladder24` | every rung, MCS 0 to 6, at 24 tones |
| `width` | MCS 2 at 24, 32, 40, 48 and 56 tones |
| `prefix` | MCS 0 and 2 at 24 tones, short then long prefix |
| `mask` | MCS 2 at 40 tones, then with tones 8-11 and 30-33 silenced |
| `custom` | the cross product of `--rungs 0-3,6`, `--widths 24,40` and `--prefix` short, long or both |

Common options: `--repeat N` (repeat the whole plan), `--frame-s` or `--blocks` (frame length;
the default is SPEC §10.2's, 2 s at MCS 0 and 1, 4 s above), `--mask 8-11,30` (silenced tones,
custom plan), `--gap`, `--lead`, `--tone-s`, `--seed`, `--peak-dbfs`. Payloads are generated from
a seed per frame and recorded in the manifest, so `rx` can score every bit. Keep the manifest
that belongs to the WAV you played.

Repeat the plan several times (`--repeat 5`) to get a block error rate rather than a pass or
fail. One frame at a rung tells you little near its threshold.

## Reading the report

```
 #  MCS  tones  prefix   time(s)  hdr  blocks  SNR7(dB)   clock(ppm)  EVM(dB)  dist  pick  note
 2    2     24  short     15.61   ok   14/14       25.3           0    -21.3     0     4
```

| Column | Meaning |
| --- | --- |
| time | Where the preamble was found in the recording, seconds. Unmatched frames are listed as **MISSED**, with the time they were expected at. A detection that matches no frame is counted as an *extra detection* in the summary. |
| prefix | The prefix the detector found. A note appears if it differs from what was sent. |
| hdr | The header decoded, and (in the JSON) matched what was sent. |
| blocks | Blocks that passed their CRC *and* matched the transmitted payload, out of the blocks sent. |
| SNR7 | The **channel SNR** over the active tones on the **7 dB-equivalent scale** (SPEC §7.1, ADR 0003, ADR 0006): signal over everything the channel and radios add, but not the distortion clip-and-filter produces by design, plus the frame's clip level minus 7 dB. It is directly comparable with the thresholds of SPEC §6.2, whatever rung the frame used, and a clean link reads high (60 dB or more is a quantisation-limited file; the field saturates at 63.75 dB). The receiver rebuilds the symbols it knows (preamble, width probe, the header and every data symbol whose blocks passed their CRC) from the decoded bits and measures the error against them. A **\*** after the figure means the **fallback** was used: too few symbols were known (the header and the blocks failed, a weak frame), so the figure counts the modem's own clipping as noise and reads low, never high. Treat a starred figure as "at most about this"; it is also in the JSON as `snr_fallback`, with `snr_known_symbols` out of `snr_total_symbols`. |
| clock | Sound-card clock offset in ppm, from the pilots of that frame. Positive means the recording is shorter than the transmission (the recording clock is faster). The summary also gives a figure from the frame timing across the whole recording, which is much more accurate; if it exceeds 25 ppm the recording is resampled by it before decoding. |
| EVM | Error vector magnitude of the data constellation, in dB below the constellation power. |
| dist | Distortion index, 0 (clean) to 7 (severe), in 3 dB steps. It follows the idea of SPEC Appendix B.2 and is **indicative**: energy on silent tones (above the band, and on masked tones) beyond the clip-and-filter regrowth the waveform has by design, combined with amplitude-dependent constellation error (outer points of 16-QAM and above compressing, and symbols with high peaks having more error than quiet ones, compared with the transmitted waveform's own value). It rises when the radio's limiter or deviation clip is working: lower the drive. |
| pick | The rung that link adaptation would choose for this SNR: the first-frame rule of SPEC §10.1 (the highest rung whose §6.2 threshold is 3 dB below the measured SNR). The "highest supported" rung (threshold at or below the SNR) is in the detail lines. **No Stage 2 hardware margin is applied**; the measured margins are exactly what you are collecting. |

Below the table are the per-frame distortion details and a summary: frames sent, detected and
missed, headers decoded, frames with every block OK, blocks OK over blocks sent (missed frames
count as lost), the SNR range (with how many frames used the fallback), and the clock offset. `rx` exits with status 1 if any frame was missed.

`--plots DIR` writes `snr_per_tone.png` (per-tone channel SNR for each frame on the 7 dB-equivalent scale, against
frequency: look for roll-off at the band edges, and for the upward tilt of a flat port's noise and the
notches of narrow radio filters) and `constellation_N.png` (equalized points of one frame, the highest
rung decoded unless `--plot-frame N` says otherwise). `--json PATH` (or `-` for stdout) writes everything,
including the per-tone SNR arrays and the distortion components.

### What to look for

- All frames decoded at every rung: the audio chain is good for the data rate.
- Even the low rungs fail or the SNR is poor: check levels first (a clipped input or an over-driven
  transmitter). A distortion index of 3 or higher means the transmitter is over-driven.
- Low rungs fine, high rungs fail: normal on a real chain; the SNR7 column against the §6.2 thresholds tells
  you how much margin the hardware costs.
- Frames missed: with VOX, the attack ate the preamble or the radio dropped between frames. With PTT,
  the tail of the radio's squelch or a TXDelay problem. Move the gap and `--txdelay`. Weak signals near
  the FM threshold also miss preambles; the figure to compare is the SNR of the frames that did decode.
- The measured SNR varying wildly between frames at the same level: noise bursts or a fading path.
- A big per-tone SNR hole: an audio filter notch, a CTCSS or tone squelch filter, or hum.

## Setting up a station with `cairn-modem setup`

Before the `cairn-modem` tests below, each station can run the guided setup
once. It writes `~/.config/cairn/cairn.toml`, which every later command
reads (flags still override it), so the long `-d ... --ptt ... --txdelay ...`
lines shrink to `cairn-modem run`. The full walk-through is in the
[operator guide](operator-guide.md); in short:

```sh
M=modem/target/release/cairn-modem
$M rigs                 # the rig profiles: aioc-ht (measured on one pair of handhelds), digirig, signalink, cm108-gpio
$M setup                # interactive; Ctrl-D at any prompt stops without writing
$M selftest             # PASS/WARN/FAIL, nothing transmitted
```

The setup goes through these steps, in order:

1. **Station**: callsign and SSID (every header and ID block carries it),
   grid locator.
2. **Audio device**: the cards that capture and play at 48 kHz, with the
   rig profiles each matches.
3. **Rig profile**: suggested from the device; pick one, make a new one
   (saved in `~/.config/cairn/rigs/`), or none.
4. **PTT**: USB serial ports and CM108-class HID devices found on the host
   (recognised by USB ID: AIOC, CP210x as on the Digirig, C-Media), or
   `vox`, `none`, `rigctld`; then the line (DTR/RTS) and polarity, or the
   GPIO pin.
5. **Real-time priority**: what the audio threads get, and how to allow it
   if they get nothing.
6. **Receive level**: with the squelch open and no signal, a 3 s measurement
   against the target of SPEC 13.5 (open-squelch noise near -20 dBFS RMS,
   peaks under -3 dBFS); optionally a second one while a peer sends the
   tone or data (peaks near -10 dBFS); whether to run the receive AGC.
7. **PTT test**: 1 s keyed, no audio.
8. **Transmit level**: the level-setting tone (4 s), with how to judge
   70% deviation without a meter (the limiter knee on a second receiver,
   minus 3 dB) and the empirical fallback, the drive sweep: lower the
   interface's output (for the AIOC, 'AIOC Audio Out Volume') 1-2 dB at a
   time while a peer reports link-test SNR, and keep the level where the
   SNR stops improving. You enter the setting you ended at.
9. **TXDelay**: optionally measured with a second station running
   `cairn-modem listen`: eight UI frames, each saying `TXDELAY n`, at 500,
   400, 300, 250, 200, 150, 100 and 50 ms. You enter the shortest that
   decoded (with every longer one), and setup recommends that plus 50 ms.
   Then TXTail.
10. **Squelch and carrier tail**: why the squelch stays open, and what a
    radio's carrier tail (about 0.7 s on the `aioc-ht` handhelds) costs
    each turnaround.
11. **Identification**: if the PTT test or the tone went out (neither
    carries a callsign), an ID frame (a BEACON with the ID block) is offered.
12. **Review**: the file is shown, then written; only the values that
    differ from the profile go in it, and the previous file is kept as
    `cairn.toml.bak`.

Steps 7, 8, 9 and 11 key the transmitter. Each one says what it will send,
for how long, and whether the transmission identifies the station, then
waits for you to type `yes`. Any other answer (`y` too) skips the step, and
the profile's values stay. Use a dummy load, or a frequency you are licensed
for that is clear.

## Two-radio broadcast test with `cairn-modem`

The real-time modem (`modem/`, the `cairn-modem` command; build with
`cargo build --release -p cairn-modem` in `modem/`) runs the same test live:
station A transmits broadcast frames with real headers, station B listens
continuously and decodes each frame header first as it arrives, with no
manifest and no recording. It is one-way: nothing is acknowledged.

```sh
M=modem/target/release/cairn-modem
# Both stations: find the interface and its PTT line.
$M devices
$M ptt-test --ptt serial:All-In-One-Cable --seconds 1

# 1. Levels. Station A sends the level-setting tone; set A's drive to 70% of
#    full deviation (see "Setting levels"). Station B watches its input and
#    sets the receive level so the tone peaks near -10 dBFS.
$M tone -d All-In-One-Cable --ptt serial:All-In-One-Cable --seconds 3          # station A
$M levels -d All-In-One-Cable --seconds 10                                     # station B

# 2. Station B listens (Ctrl-C to stop). Broadcasts need no link settings: a
#    frame wider than 24 tones tells B its width with its probe (ADR 0007).
$M listen -d All-In-One-Cable --text --record rx.wav                           # station B

# 3. Station A transmits: UI broadcasts from its callsign, each frame its own
#    keyed transmission, with the ID control block (B.6) in every frame.
$M tx -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 \
    --id --grid FN31 --mcs 0 --count 5 --gap 2                                  # station A
$M tx ... --callsign N0CALL-1 --mcs 2 --count 5 --message "test de N0CALL"      # more rungs
$M tx ... --callsign N0CALL-1 --mcs 2 --width 30 --count 5                      # wider (flat ports)
```

Each frame B decodes prints one line: the time, source and destination
(`*` for broadcast), the frame type and its flags, the frame counter, the
rung and width (and "from probe" when the width came from the probe), data
and control blocks decoded out of those sent, the channel SNR on the 7 dB
scale (ADR 0006), the sample-clock offset between the two sound cards, the
detector's rho and the decode time; then a line per control block (`ID
N0CALL-1 grid FN31`) and, with `--text`, the payload. A detection whose
header fails prints `not decoded: header did not decode`.

- **Station identification.** Every header carries the source callsign
  (SPEC 8.3), and `--id` adds the ID control block with the full callsign
  and locator; `--type beacon` sends a BEACON, which always carries it.
  That is the modem's own identification; where your rules ask for a CW or
  voice ID as well, send it between runs. Never use someone else's
  callsign.
- **What to vary.** Rungs (`--mcs 0` to `6`; 2 s frames at MCS 0-1 and 4 s
  above by default, `--blocks` or `--frame-s` to change), width (`--width`,
  24 by default; above 30 tones only on 25 kHz channels through flat
  ports), prefix (`--prefix long`, which B detects by itself). B needs no
  change for any of these. `--type data --to CALL --width W --mask ...`
  sends session-style frames, which B decodes only with the same `--width`,
  `--mask` and `--mask-id` (ADR 0011; `--peer-cap` only when A set `--cap`),
  link state the header does not carry.
- **Reading it.** Compare B's SNR with the rung thresholds of SPEC 6.2 plus
  a few dB of margin. Frames that detect but whose header fails, or a
  clock offset above about 100 ppm, point at levels or the sound cards. The
  recording (`--record`) can be re-run with `listen --wav rx.wav` at any
  time, and scored against A's frames with `cairn-bench rx` if A also wrote
  the same frames with `tx --wav` (same arguments and `--seed`).
- **Timing.** `listen --wav rx.wav --realtime` replays the recording at
  real-time pace through the same code as the sound card and reports the
  receive turnaround (SPEC 7.3: at most 100 ms from a frame's last sample
  to its decoded result).

## Two-station session with `cairn-modem run`

`cairn-modem run` is the whole modem: the link layer (sessions with
selective-repeat ARQ, rate adaptation, resume, link test, broadcast; SPEC
8-12, ADRs 0008-0013), carrier detect (SPEC 7.2: quieting, the preamble
correlator, the Bell 202 detector), a KISS server on TCP port 8001 and a
line-based control interface on stdin (and optionally a TCP port). Each
station runs one; the operators type commands. Before going on the air,
run the same session in simulation (`cargo test --release -p cairn-modem
--test two_station -- --nocapture` in `modem/`: two complete modems on a
simulated FM channel) so you know what a good run looks like.

```sh
M=modem/target/release/cairn-modem
# 0. Levels and TXDelay exactly as for the broadcast test above (tone, levels),
#    or with `cairn-modem setup` (above), after which the flags below that the
#    config holds (-d, --ptt, --txdelay, --txtail, --cap, --callsign) can go.
#    Open the squelch on both radios and keep it open: carrier detect watches
#    the noise above the signal band (2800-3200 Hz at 24 tones, 3600-4150 Hz
#    with --cap 32) and needs it. With the squelch open the two handhelds of
#    the first session ran TXDelay 300 ms (--txdelay 300) and TXTail 20 ms
#    (--txtail 20), and their AIOC playback levels ended at
#    -9 dB (station A) and -10 dB (station B) in alsamixer for 70% deviation.

# 1. Both stations start the modem (Ctrl-C or "quit" stops it). --cap 32 lets
#    each send 32 tones (B.3 width cap 31; tones 0-31 reach 3.45 kHz), tone
#    masks drop the tones the radios do not pass, --rx-dir saves what arrives.
$M run -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-1 --grid FN31 \
    --txdelay 300 --txtail 20 --cap 32 --rx-dir ~/cairn-rx --status 30 -v                        # A
$M run -d All-In-One-Cable --ptt serial:All-In-One-Cable --callsign N0CALL-2 \
    --txdelay 300 --txtail 20 --cap 32 --rx-dir ~/cairn-rx --status 30 -v                        # B
#    Each prints its devices, PTT, "cap 32 tones (B.3 width cap 31, wide), tone
#    masks tracked, quieting 3600-4150 Hz", "KISS server on 127.0.0.1:8001"
#    and waits. Leave both idle for a minute: "status" should show the
#    quieting floor armed (a level in dBFS, not "unarmed") and no
#    carrier-detect rises (rises q/p/a/c [0, 0, 0, 0]). A rise with nobody on
#    the air is a false detection: raise the receive level or move the band
#    (--quieting LO-HI).

# 2. A link test first (about 1 s of air): both directions' SNR, rung and expected kbps.
probe N0CALL-2                                   # at A
# -> link test N0CALL-2: forward SNR 24.8 dB MCS 5 7.59 kbps; reverse SNR ...

# 3. A session: connect, send text and a file each way, disconnect.
connect N0CALL-2                                 # at A -> "connected to N0CALL-2 (session ..., initiator)"
#    Both print "session width with ...: tx 32 tones (own cap 31, peer wide),
#    rx 32 tones (...); tx mask ID 0: tones 0-2 masked, 29 of 32 active; ..."
#    when the radios' low tones are as weak as on the first session's.
send hello de N0CALL-1                           # at A; B prints "rx from N0CALL-1: ..." and saves it
send @/path/to/file                              # at A: FILE-CHUNK PDUs; "delivered to N0CALL-2: file ..."
#    B: "rx file from N0CALL-1 object 1 complete: N bytes -> ~/cairn-rx/N0CALL-1_obj1.bin sha256 ..."
#    (compare with sha256sum of the file at A)
send @/path/to/other                             # at B, at the same time: both directions share the turns
status                                           # either: rung, width and mask each way, counters
disconnect                                       # at A, after the deliveries
```

What to look for:

- **Turns.** Every frame prints `tx DATA ... turn-holder` on the sender
  and, with `-v`, `rx ...` with the SNR on the 7 dB scale at the receiver.
  The replies alternate without gaps of more than a few hundred ms; a reply
  that waits seconds means the turn-holder's carrier check saw a carrier
  (with `-v`, the `dcd` lines show which detector) or a frame was lost and
  the ACK timer (about 6.6 s) brought it back.
- **Adaptation.** The rung each way (`status`, and `rungs with ...` lines)
  settles within a few frames and stays put at a constant level. Turn the
  attenuator by 6 dB during a transfer: the rung follows within 2 frames
  down and 4 up (ADR 0013).
- **Width and masks.** With `--cap 32` the session runs 32 tones each way.
  The first session's radios measured -13 to +7 dB on tones 0-2
  (350-550 Hz) against 9-28 dB above, and the reported SNR at 32 tones was
  13 dB unmasked against 21-25 dB with tones 0-2 masked: the mask should
  drop tones 0-2 from CONNECT-ACK on (`tx mask ID 0: tones 0-2 masked`),
  and the rung settle at MCS 4-5 one way where the first session (24
  tones, unmasked, about 12 dB measured) held MCS 0-2 (the best manual
  setting was 32 tones, tones 0-2 masked, MCS 5 one way and MCS 3 the
  other). A tone that fades is dropped after
  two frames (`rx mask report ID 1 ...`) and comes back after a probe shows
  it recovered. `--no-mask` turns masks off for a comparison; `--cap 24`
  (the default) is the first session's setting.
- **Carrier detect.** With `-v`, every peer transmission should give one
  `quieting: true` and one `quieting: false`, and an own transmission none
  (the first session's radios keep their carrier about 0.65 s after PTT
  release, so the receiver hears silence; quieting now waits for the noise
  to come back, and `status` reports how long that took: "receiver back
  after own PTT 640/660/700 ms"). The first session counted 71-72 quieting
  rises in 3.5 minutes; expect about one per peer frame. That tail also
  delays every reply: the peer waits until the carrier has gone (about
  0.8 s after the last sample); if the radios or the AIOC's virtual PTT
  have a tail setting, shortening it buys throughput both ways.
- **Throughput.** "delivered" lines with byte counts over the elapsed
  time, against Appendix A.2 at the rung in use (one way, 24 tones: MCS 3
  4.55 kbps, MCS 4 6.09, MCS 5 7.61, MCS 6 9.11 kbps; both ways at once
  share that total).
- **Resume.** Switch one radio off for two minutes mid-transfer: both
  ends print "link lost" (after about a minute), then `connect N0CALL-2`
  at A resumes ("connected ..., resumed") and the transfer completes with
  nothing delivered twice.
- **Sharing.** A packet station keying during the session (or `beacon` from a
  third modem) holds both stations off until it is done; with `-v` its
  carrier shows as `afsk: true` (Bell 202) or `quieting: true`.
- **KISS.** Any KISS client (e.g. a beacon from an APRS program) on port
  8001 of one station sends its AX.25 frames as UI broadcasts; the other
  station's KISS clients receive them unchanged.
- **Safety.** `--tx-budget SECONDS` caps the total keyed time of a run, and
  `--seconds` stops it; PTT is released on every exit path. Identification
  is the callsign in every header plus the ID control block in CONNECT,
  CONNECT-ACK, DISCONNECT, every BEACON and at least every 9 minutes in a
  session.

## Real-time priority, capture gaps and receive gain

In the first session a capture lost 26.6 ms of audio inside a frame
(`input: xruns 2 gaps 2 (64.8 ms)`) while the laptop ran parallel compiles,
and the frame was lost. Three things now stand against that; check them on
both machines before a session.

**Real-time priority.** `cairn-modem` asks for it for its audio threads
(`SCHED_FIFO`) and its processing threads (detection, decoding, the event
loop, transmit) without root, and prints what it got when it starts:

```
input plughw:CARD=AllInOneCable,DEV=0 (S16 x1 @ 48000 Hz, period 480 (10.0 ms), buffer 9600 (200.0 ms), alsa); capture thread SCHED_RR 20 via rtkit
event loop nice -10 via rtkit
```

A `warning: the capture thread runs without real-time priority (...)` line
means neither way below worked. Either is enough:

- **rtkit** (nothing to configure on a desktop that runs PipeWire or
  PulseAudio: it is the same daemon they use). Check it is there with
  `busctl status org.freedesktop.RealtimeKit1`; if not, install it (Arch:
  `pacman -S rtkit`; Ubuntu: `apt install rtkit`) and start it (`systemctl
  start rtkit-daemon`). It gives audio threads priority up to 20 and
  processing threads nice -10. Over SSH without a desktop session polkit may
  refuse it; use the limits below there.
- **Limits** (the classic audio setup; gives the full priorities: audio 60,
  processing `SCHED_RR` 40, `--rt-prio` changes them). Put the user in the
  `audio` group and allow the group real-time scheduling:

  ```sh
  # Arch: the realtime-privileges package does exactly this for the realtime group
  #   sudo pacman -S realtime-privileges && sudo usermod -aG realtime $USER
  # Ubuntu (and Arch by hand):
  sudo usermod -aG audio $USER
  printf '@audio - rtprio 95\n@audio - nice -19\n' | sudo tee /etc/security/limits.d/95-audio.conf
  # log out and in again (or reboot); then
  ulimit -r    # 95
  ```

  The limits apply to new login sessions only; `ulimit -r` printing 0 means
  the session predates them.

`--no-rt` turns all of it off (for comparisons).

**Capture buffer.** Capture now runs on ALSA directly with 10 ms periods
in a 200 ms buffer (`--period`, `--buffer-ms`); before, it had two periods
(20 ms), so any wake-up more than about 10 ms late lost audio. The buffer
costs no latency: the modem still takes each period as it completes. The
input line prints the sizes ALSA granted. `--audio-backend cpal` goes back
to the old path (two periods); `auto` (the default) falls back to it by
itself, with a note, if the native open fails.

**Capture gaps.** If audio is still lost, the modem puts exactly as many
zero samples in its place, so the frames after it keep their timing and
decode; it prints `audio gap: 26.6 ms lost (xrun) at 123.456 s, filled with
zeros (timing kept)`, and a frame that spans the gap is printed (also
without `-v`) with `AUDIO GAP 26.6 ms at +1.234 s`, or `rx preamble, header
failed (AUDIO GAP ...)` when its header was lost. The API flags them
(`audio_gap` in `link` and `rx` events, `link` `header_failed`), and
`status` shows the counters and the capture ring's high-water mark (how far
processing ever lagged; it should stay at a few tens of ms):

```
capture xruns 0 gaps 0 (0.0 ms, 0 samples filled) overflows 0 ring high-water 30/8000 ms
```

To see what a loaded machine does before going on the air, run the
receiver under load against the mock sound card (no audio hardware is
touched):

```sh
cd modem && cargo run --release -p cairn-modem --example rx_load -- --burn 16 --seconds 60 --matrix
```

It prints xruns, gaps, the ring high-water mark and decode latency without
real-time priority and a 20 ms buffer (the old path), with each change, and
with both.

**Receive gain (`run --rx-agc`).** Off by default. With it the modem sets the
interface's capture gain itself (SPEC 13.5): open-squelch noise near
-20 dBFS RMS, frame peaks never above -3 dBFS, no clipping, the data inside
-30 to -3 dBFS; 3 dB steps, only between frames (never while carrier detect
is up, a frame is detected or decoding, or the station is keyed, and not
within 300 ms of a frame's end), at most one every 10 s (2 s after
clipping), after the level has been out of range for two 0.5 s windows in a
row. Each change is printed (`rx gain -0.0 -> -3.0 dB: open-squelch noise
-13.1 dBFS, above -17`) and `status` and the API's `status`/`config` show
the gain. It only ever touches a capture control: on the AIOC that is
'AIOC Audio In' (`--rx-agc-control NAME` picks another); the output
(transmit) volume, 'AIOC Audio Out Volume', is never changed. Leave the
radio's volume where the first session left it (noise about -12 to
-14 dBFS, data about -14 dBFS): the AGC will step the input down to bring
the noise to about -20 dBFS and stop. It needs a real card (`hw:` or
`plughw:`), not the `default`/`pipewire` device; without one it prints a
warning and leaves the gain alone.

## What this cannot test

The offline decoder and the broadcast test above test the waveform on a real audio chain, one
direction at a time (the two-station session above covers sessions, ARQ, adaptation, DCD and
channel sharing). They cannot test, and do not claim to:

- **ACKs and the control channel.** Nothing is sent back. `cairn-modem tx` sends control
  blocks (the ID block, and a demonstration ACK block in `--type data` frames) and `listen`
  decodes them, but no station acts on them.
- **Link adaptation as a process.** The report says what the thresholds would pick, once per frame,
  without hysteresis, without a Stage 2 margin, and without ever acting on it.
- **ARQ, sessions, retries and the 7.4 timing budget.**
- **DCD and channel sharing.** Nothing here listens before it transmits or sees another station.
- **Real TXDelay.** The WAV is played with whatever lead you set. Radio keying time
  (the time from PTT to a stable carrier and audio path) is only observable here as a
  clipped preamble. Measuring it needs a separate keying-to-carrier test with a scope or SDR.
- **Automatic level control** (SPEC §13.5): the WAV tools set levels once by hand; `cairn-modem run
  --rx-agc` sets the receive gain (above), and transmit drive calibration is v1.0.
- **Mobile, fading and multipath channels** beyond what the path you actually run gives.

The Stage 1 receiver is a research receiver, not a real-time one: it decodes each frame knowing
the rung, width and block count from the manifest (a real receiver reads them from the header, as
`cairn-modem listen` does), so a header that fails to decode does not stop its blocks being tried.
`rx` tells you if the header decoded.
