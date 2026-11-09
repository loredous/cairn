# Cairn operator guide

This guide takes a station from a fresh build to a first contact and on to
unattended operation with the reference modem, `cairn-modem`. It covers the
v0.5 operator features of SPEC §13.4 and §13.6: the configuration file, rig
profiles, the guided setup, the self-test and unattended mode. For bench
measurements, see [Bench testing](bench-testing.md). For applications that
drive the modem, see the [native API](api.md).

Callsigns in this guide (N0CALL, N0AAA and so on) are placeholders.

!!! note "Regulations"
    The regulatory notes here are for information only and are not legal
    advice. They refer to US Part 97, and other administrations have their
    own rules. You are responsible for every transmission your station makes.

## Install

```sh
# Build dependencies: Rust (rustup), pkg-config and the ALSA headers
# (libasound2-dev on Debian/Ubuntu, alsa-lib on Arch).
git clone https://github.com/loredous/cairn && cd cairn/modem
cargo build --release -p cairn-modem
install -m 755 target/release/cairn-modem ~/.local/bin/
```

Permissions the modem needs, all as your own user (never root):

- **Serial PTT** (AIOC, Digirig): membership in the group that owns
  `/dev/ttyACM*` and `/dev/ttyUSB*`. That group is `dialout` on Debian and
  Ubuntu and `uucp` on Arch.
- **CM108 GPIO PTT**: write access to the `/dev/hidrawN` node. A udev rule
  does it, for example
  `SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0d8c", MODE="0660", GROUP="audio"`.
- **Real-time priority** for the audio threads: either
  `@audio - rtprio 95` in `/etc/security/limits.d/` plus membership in
  `audio`, or a running rtkit daemon (most PipeWire desktops have one). See
  [Real-time priority](bench-testing.md#real-time-priority-capture-gaps-and-receive-gain).

Log out and back in after changing your groups.

## Set up the station

```sh
cairn-modem rigs        # the rig profiles
cairn-modem devices     # sound cards, and PTT candidates found by USB ID
cairn-modem setup       # the guided setup
```

`setup` asks for your callsign and then works through the audio device, the
rig profile, the PTT line, real-time priority, the receive level, a PTT test,
the transmit level, TXDelay, the squelch and identification. When it
finishes, it writes `~/.config/cairn/cairn.toml`. The step-by-step list is
in [Bench testing](bench-testing.md#setting-up-a-station-with-cairn-modem-setup).

Any step that keys the transmitter first tells you what it will send, for
how long, and whether the transmission carries your callsign. It then waits
for you to type `yes`. Any other answer skips the step. Ctrl-D at any prompt
stops the setup without writing anything. Run the setup with a dummy load
connected, or on a clear frequency you are licensed to use.

### Rig profiles

A rig profile holds the defaults for one interface and radio combination:
the audio device, the PTT method and lines, TXDelay and TXTail, the output
level, the squelch the radio expects, its carrier tail, its passband and the
width cap. The profiles that ship with Cairn live in
[`docs/rigs/`](https://github.com/loredous/cairn/tree/main/docs/rigs) and
are compiled into the binary:

| Profile | Interface | Status |
| --- | --- | --- |
| `aioc-ht` | AIOC on a handheld's mic and speaker jack | Measured on one pair of handhelds (field tests, 2026-11-07): TXDelay 300 ms, TXTail 20 ms, 32 tones with masks, 'AIOC Audio Out Volume' about −9.5 dB, carrier tail about 0.7 s, passband about 350 Hz to 4.25 kHz with tones 0–2 weak |
| `digirig` | Digirig Mobile (CM108-class audio, serial RTS PTT) | Unverified defaults |
| `signalink` | SignaLink USB (VOX) | Unverified defaults. Broadcast only; refuses sessions |
| `cm108-gpio` | CM108/CM119 cable with GPIO PTT | Unverified defaults; `cm108:auto` |

To add a profile, or to replace a shipped one, put a file with the same
format in `~/.config/cairn/rigs/NAME.toml`. The setup can create one for
you. `cairn-modem rigs aioc-ht` prints a profile in full and is a good
template. If you measure values that differ from a shipped profile, please
send them back, and say what you measured them on.

### The configuration file

Every command reads `~/.config/cairn/cairn.toml`. Use `--config PATH` or
`CAIRN_CONFIG` to read another file, or `--no-config` to read none. Settings
are applied in layers, and each layer overrides the one before it:

1. Built-in defaults
2. The rig profile named in `station.rig`, or with `--rig NAME`
3. The configuration file
4. Command-line flags and their environment variables

Unknown keys are errors, so a typo cannot pass silently. Here is an example
with every section. The setup writes only the keys that differ from the
profile you chose.

```toml
[station]
callsign = "N0CALL-1"
grid = "EM10"
rig = "aioc-ht"

[audio]                      # device, period, buffer_ms, backend, rt, rt_prio
rx_agc = true                # receive AGC on the capture control (SPEC 13.5)
output_db = -10.0            # where you set the drive control (recorded, never changed)

[ptt]                        # method = "serial:All-In-One-Cable", line = "dtr", invert, gpio

[timing]
txdelay_ms = 250.0           # txtail_ms

[link]                       # cap, no_mask, persistence, slot_ms, max_frame, broadcast_mcs, quieting, refuse, id_broadcasts
dict_dir = "~/cairn-dicts"   # zstd dictionaries (*.zdict) offered at session setup (SPEC 12.2)

[host]
rx_dir = "~/cairn-rx"
capture_dir = "~/cairn-captures"
heard_file = "~/.local/state/cairn/heard.json"
capture_on_failure = true
log_file = "~/.local/state/cairn/cairn.log"   # log_max_kb = 1024, log_keep = 5
# kiss = "127.0.0.1:8001", api = "127.0.0.1:8002", api_socket, control, ring_seconds

[unattended]                 # see "Unattended operation"
allow = ["N0AAA-*", "N0BBB-2"]
```

## Check the station

```sh
cairn-modem selftest
```

The self-test transmits nothing. Each check reports PASS, WARN or FAIL, and
anything other than PASS comes with a fix. The command exits non-zero if any
check fails.

| Check | What it looks at |
| --- | --- |
| config file, rig profile | The file parses and the profile exists. Unverified profiles are noted |
| callsign, settings | The callsign is valid, and the timing, width cap, PTT and unattended settings are in range |
| audio device, input, output | The device is present at 48 kHz, and its input and output open and run |
| receive level | Two seconds of input against open-squelch noise near −20 dBFS RMS, with peaks under −3 dBFS. The squelch must be open |
| PTT | The line opens and reads unkeyed while idle. Without read-back it can only open the line. With `none` or `vox`, the check warns |
| real-time priority | What an audio thread is granted |
| capture gain control | The mixer control the receive AGC needs |
| reference decode | A recording built into the binary goes through the full receive path |
| KISS port, API port | Nothing else is listening on those ports |
| unattended | The policy, plus whether the hook programs are executable |

`cairn-modem selftest --transmit` adds one keyed check. After you confirm
with `yes`, it sends an ID frame (a BEACON with the ID block) and listens
for it. You get the measured TXDelay and TXTail, and the PTT line read back
while keyed and after release. The frame is heard back only when something
feeds the output back to the input, such as a second receiver on the
frequency or a loopback cable. A half-duplex radio cannot hear its own
transmission.

## First contact

Both stations have run `setup` and `selftest`. Open the squelch on both
radios and leave it open.

```sh
cairn-modem run                 # each station; type commands on stdin
```

Wait about a minute and type `status`. The quieting floor should show a
level in dBFS rather than "unarmed", and the carrier-detect rises should
stay at zero while nobody transmits. A rise with the channel quiet is a
false detection: lower the receive level or move the quieting band
(`--quieting LO-HI`).

The operator at station A then types:

```text
probe N0BBB-2        # link test: both directions' SNR, rung and expected kbps, about 1 s of air time
connect N0BBB-2      # a session
send hello from N0CALL-1
send @notes.txt      # a file; with rx_dir set, B saves it and prints its SHA-256
disconnect
```

The ID block goes out in CONNECT, CONNECT-ACK and DISCONNECT, and at least
every 9 minutes within a session (ADR 0010 `session-id-cadence`). Every
header carries the callsign (SPEC §8.3).

### Compression

Messages and file chunks are compressed with zstd when that makes them
smaller (SPEC §12.2). With `--dict-dir` (or `link.dict_dir`) pointing at a
directory of dictionary files (`*.zdict`), `run` prints the dictionaries
it loaded and offers them at session setup; a session uses the ones both
stations have, so give both stations the same files. A 280-byte ICS-213
message took 129 bytes with the 64 kB evaluation dictionary, 219 without a
dictionary. No dictionary has been released yet: the files in
`dictionaries/eval/` are evaluation artifacts that may change, so use them
only between stations you control. Compressed content is still published:
anyone with the published dictionary and a standard zstd decoder can read
it (§97.309(b)). Already-compressed files (zip, jpeg) are sent raw, since
compression would not shrink them. A message that names a dictionary this
station does not have is shown as undecoded, with a warning.

## Unattended operation

```sh
cairn-modem run --unattended --no-stdin
```

You can also set `enabled = true` in `[unattended]`. An unattended station
answers incoming CONNECTs and keeps running by itself. In this mode the
modem enforces limits that a control operator would otherwise apply by
hand:

| Feature | Setting (default) | Behaviour |
| --- | --- | --- |
| Allow list | `allow` (empty: anyone not denied) | Only these stations get a session; everyone else is refused with a DISCONNECT that carries the ID block |
| Deny list | `deny` | Always refused, even if also on the allow list. `*` refuses everyone |
| Session limit | `max_session_min` (30; 0 for no limit) | The session is disconnected when it reaches the limit. A resumed session keeps the clock it already had |
| Daily keyed time | `daily_tx_min` (60 per UTC day; 0 for no limit) | Near the end of the budget (30 s left, or a quarter of a small budget), the station refuses new sessions and closes the open one. With nothing left, it keys nothing until 00:00 UTC. `budget_file` keeps the count across restarts |
| Periodic ID | always on in this mode | The ID block goes in a UI frame when 9 minutes have passed since the last one, the same clock as in sessions. Outside unattended mode, `--id-broadcasts` turns it on |
| Watchdog | `watchdog_margin_s` (2), `audio_stall_s` (5) | Releases the PTT from its own thread and stops the modem if a transmission stays keyed longer than its keyed time plus the margin, or if no audio arrives for the stall time (a hung event loop stops the audio too). The watchdog runs in every `run`; unattended mode will not start without it |
| Capture on failure | `host.capture_on_failure` | Saves the raw audio and decoder state shortly after a frame from the session peer fails |
| Log | `host.log_file`, `log_max_kb` (1024), `log_keep` (5) | Every output line with a UTC time stamp, rotated by size |
| Hooks | `[unattended.hooks]` | A program you name runs on `connected`, `disconnected`, `received_file`, `received_message`, `refused` and `alarm` (the watchdog tripped, or the budget is used up) |

Allow and deny patterns:

| Pattern | Matches |
| --- | --- |
| `N0CALL` | N0CALL with SSID 0 |
| `N0CALL-7` | N0CALL-7 only |
| `N0CALL-*` | Every SSID of N0CALL |
| `*` | Every station |

```toml
[unattended]
enabled = true
allow = ["N0AAA-*", "N0BBB-2"]
deny = ["N0BAD-*"]
max_session_min = 20
daily_tx_min = 45
budget_file = "~/.local/state/cairn/tx-today.json"

[unattended.hooks]
received_file = ["/home/op/bin/cairn-file-arrived", "--mail"]
alarm = ["/home/op/bin/cairn-alarm"]
timeout_s = 30
```

Hooks run the program with exactly the arguments you configured, without a
shell. The modem never puts anything received over the air on a command
line. Event data reaches the program in environment variables, which carry
printable ASCII only, and a received message's text arrives on stdin:

| Variable | Contents |
| --- | --- |
| `CAIRN_EVENT` | `connected`, `disconnected`, `received_file`, `received_message`, `refused`, `alarm` |
| `CAIRN_TIME` | UTC time stamp |
| `CAIRN_STATION`, `CAIRN_PEER` | This station and the other station |
| `CAIRN_SESSION_ID`, `CAIRN_RESUMED`, `CAIRN_INITIATOR` | For `connected` |
| `CAIRN_REASON` | `disconnect` or `link-lost`; for `alarm`, why |
| `CAIRN_FILE`, `CAIRN_OBJECT`, `CAIRN_BYTES`, `CAIRN_SHA256` | Received files (needs `host.rx_dir`) |
| `CAIRN_FILE`, `CAIRN_BYTES`, `CAIRN_BROADCAST` | Received messages (needs `host.rx_dir`) |

At most four hooks run at once. A hook still running after `timeout_s` is
killed. Treat everything a hook receives as untrusted input.

To run the modem as a systemd user service, write
`~/.config/systemd/user/cairn.service`:

```ini
[Unit]
Description=Cairn modem (unattended)

[Service]
ExecStart=%h/.local/bin/cairn-modem run --unattended --no-stdin
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
```

Then run `systemctl --user enable --now cairn`. The modem stops when the
watchdog trips, and `Restart=on-failure` brings it back. If you would rather
have a person look first, set `Restart=no`.

### Safety rationale

These notes are informational, not legal advice.

- **Automatic control.** A station under automatic control transmits with
  no control operator at the control point (§97.3(a)(6), §97.109(d)).
  §97.221 allows automatic control of RTTY and data emissions on 6 m and
  shorter wavelengths, and on some HF segments. Outside those it is allowed
  only in reply to a station under local or remote control, and within a
  bandwidth limit. Check the current rules and your band plan before you
  leave a station unattended.
- **The licensee stays responsible.** The station licensee and the control
  operator are responsible for the station's proper operation and for what
  it transmits (§97.103, §97.105). An automatically controlled station
  carrying other people's traffic is still subject to the third-party and
  content rules (§97.113, §97.115). The modem cannot judge content. The
  allow list decides who can make your station transmit.
- **Identification.** §97.119 requires identification at the end of each
  communication and at least every 10 minutes during it. Every Cairn header
  carries the callsign in the published address format. The ASCII ID block
  goes in CONNECT, CONNECT-ACK and DISCONNECT, and every 9 minutes within a
  session and, in unattended mode, in broadcasts (SPEC §20.7, ADR 0010).
- **Fail-safe keying.** A stuck PTT is the failure that hurts other users
  most. The watchdog releases the line on its own thread, even if the rest
  of the modem has hung. Every keyed transmission is capped at the
  configured maximum frame time. Signals, panics and exits release the line
  too. If the PTT interface itself fails (a write or ioctl error such as
  "Broken pipe" when a USB interface drops out), software cannot unkey dead
  hardware, but the modem stops sending audio, retries dropping the line,
  closes the device and reopens it by its configured path with the idle
  state set, and prints a `WARNING` that the transmitter may still be
  keyed. The same warning goes to API clients as a `ptt_fault` warning. It
  refuses to key until the device has reopened, so frames are reported as
  not sent (`tx_failed`) while the session layer carries on. An audio output
  error mid-frame releases the PTT at once. After a `ptt_fault`, check the
  interface and the radio by hand (power-cycle or unplug the interface if
  the radio is still keyed).
- **Predictable limits.** The session limit and the daily budget bound how
  much your station can transmit for others. The log and the captures let
  you account for every transmission afterwards.
- **Ability to stop.** Remote control of a station needs a way to turn it
  off. Keep a way to stop the service (`systemctl --user stop cairn`, or
  disconnect the interface) that works from wherever you control the
  station.

## Troubleshooting

| Symptom | Look at | Likely cause and fix |
| --- | --- | --- |
| Nothing decodes | `selftest`, `cairn-modem levels`, `status` | Receive level: open the squelch, then aim for noise near −20 dBFS and peaks near −10 dBFS |
| Frames decode, then fail at higher rungs | `status` (clipping), the far end's distortion index | A clipped receive path (lower the volume or capture gain), or overdriven transmit (lower the drive; see the drive sweep in setup) |
| The first frame of every turn is lost | Heard list and logs: frames from the peer missing at the start | TXDelay is too short for the radio and the far receiver. Measure it with `setup` |
| Carrier detect fires with nobody on | `status`: rises q/p/a/c | The receive level is too high or the quieting band is too close to the signal (`--quieting`) |
| The station never transmits | `status`: "why waiting" | The channel is busy (carrier detect), the squelch is closed, or the budget is used up (log) |
| Sessions stay at low rungs | `probe CALL` both ways | Levels at either end. Weak low tones on mic-path radios are expected: masks drop them |
| "watchdog: PTT keyed …" in the log | The log, `selftest` | A transmission ran long: check the PTT polarity (`ptt.invert`) and the interface. Report it with the log |
| "WARNING: PTT keying/release failed … MAY STILL BE KEYED" | The log, `ptt_fault` API warning | The PTT interface dropped out (USB reset, cable). Check the radio and the interface now; the modem refuses to key until the device reopens |
| "watchdog: no audio …" | `selftest`, `dmesg` | The sound card stopped (unplugged, USB reset) or the machine stalled |
| Audio gaps in the log | The input line and `status` | Real-time priority was not granted. See the setup's real-time step |

Tools for finding out what happened:

- **`heard`** (control line, or `heard` on the API) lists every station
  decoded, with time, SNR, width and frame type. `host.heard_file` keeps the
  list across restarts.
- **`capture [SECONDS] [LABEL]`** saves the last seconds of received audio
  (`host.ring_seconds`, 120 by default) and the decoder state to
  `capture_dir`. With `capture_on_failure`, this happens automatically
  shortly after a failed frame from the session peer.
- **Replay** a capture with `cairn-modem listen --wav captures/cairn-…wav`,
  or score it with `cairn-bench rx` (see [Bench testing](bench-testing.md)).
- **Reporting a problem**: attach the capture WAV, its `.json` sidecar, the
  log around that time, the `selftest` output and the rig profile.
