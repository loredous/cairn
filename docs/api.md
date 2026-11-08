# Cairn native host API

The native API of [SPEC §13.2](SPEC.md): how an application drives a Cairn
modem, with sessions, broadcasts, link quality and diagnostics. This page is
the normative schema, **version 0.2.0**; [`api.schema.json`](api.schema.json)
is the same schema as a JSON Schema (draft 2020-12) document, and the
reference implementation's tests validate every message it emits against it.
A second implementation speaks this schema to count toward v1.0 (SPEC §16.2).

The reference implementation is `cairn-modem run` (`modem/`, crate
`cairn-api` for the transport, heard list and capture buffer); a small
example client is `modem/tools/api_client.py`.

## Transport

- **TCP**, default `127.0.0.1:8002` (SPEC §13.2, decision 20.1), and a
  **Unix-domain stream socket** at a configured path. The same protocol runs
  on both; a modem may serve several clients on each at once.
- **Framing.** UTF-8 text, one JSON object (RFC 8259) per line, each line
  ended by LF (`\n`). A CR before the LF is ignored. Empty lines and lines of
  only white space are ignored. A line is at most 1 MiB (1,048,576 bytes,
  LF excluded); a longer line is discarded up to its LF and answered with a
  `line_too_long` error.
- **No authentication and no encryption.** The API can key the transmitter
  and read files the modem can read (`send` with `file`). Bind it to the
  loopback interface or a Unix socket with suitable permissions; do not
  expose it to untrusted networks.
- Three kinds of message, told apart by their members:

  | Kind | Direction | Has | Example |
  | --- | --- | --- | --- |
  | Request | client to modem | `cmd` | `{"id":1,"cmd":"status"}` |
  | Response | modem to the requesting client | `ok` | `{"id":1,"ok":true,"cmd":"status",...}` |
  | Event | modem to every subscribed client | `event` | `{"event":"heard",...}` |

- **Order.** A modem answers each request with exactly one response, in the
  order the client sent them. Events may come between a request and its
  response. Events caused by a command (for example `session` state
  `connecting` after `connect`) may arrive before or after that command's
  response; clients correlate by `ref` and by content, not by order.
- **Unknown members are ignored** in both directions, so a newer peer can
  add optional members without breaking an older one. Unknown `cmd` values
  are answered with `unknown_command`; unknown `event` names must be ignored
  by clients.

## Versioning and the `hello` event

The first line a modem sends on every new connection is a `hello` event:

```json
{"event":"hello","time":"2026-11-07T18:00:00.000Z","modem_time":12.5,"api":"cairn","api_version":"0.2.0","implementation":{"name":"cairn-modem","version":"0.1.0"},"station":"N0CALL-1","client":1,"commands":["connect","disconnect","send","beacon","probe","heard","status","config","calibrate","capture","subscribe"],"unsupported":["calibrate"],"events":["hello","rx","session","link","heard","channel","dcd","tx","error"],"encodings":["raw","zstd","zstd_dict"]}
```

| Member | Type | Meaning |
| --- | --- | --- |
| `api` | `"cairn"` | Protocol name |
| `api_version` | string | Schema version, `MAJOR.MINOR.PATCH`. Until v1.0 (when the API is frozen, SPEC §16.2) a MINOR change may break compatibility; from 1.0.0 only a MAJOR change does, and MINOR versions only add optional members, commands and events |
| `implementation` | object | `name` and `version` of the modem software |
| `station` | address | This modem's callsign |
| `client` | integer | This connection's number (1, 2, ...), for logs |
| `commands` | array | Every command the modem accepts: all of them (SPEC §13.2) |
| `unsupported` | array | Commands that always answer `unsupported` here: only `calibrate` and `capture` may appear (SPEC §13.2) |
| `events` | array | Event names this modem emits |
| `encodings` | array | Content encodings `send` can produce (SPEC §12.2) |

A client that needs a newer schema than the modem's `api_version` should say
so to its user and disconnect.

**0.2.0** (from 0.1.0): `send` takes `encoding` (default `auto`) and
`dictionary`, and its response reports `encoding`, `dictionary` and
`pdu_bytes`; `rx` reports `encoding`, `dictionary`, `decoded` and
`encoded_bytes`; new error codes `unknown_dictionary` and `ptt_fault`.

## Common types

| Type | JSON | Notes |
| --- | --- | --- |
| address (out) | string | `CALL` or `CALL-SSID` as the modem prints it: upper case, SSID 1-15 (0 is printed without a suffix); `#` and ten lower-case hex digits for a long callsign carried as a hash (B.1); `*` for broadcast |
| address (in) | string | `CALL[-SSID]`, any case, SSID 0-15; callsigns of 7-10 characters are carried as a hash |
| time | string | UTC, RFC 3339 with milliseconds: `2026-11-07T18:04:05.123Z` |
| modem_time | number | Seconds on the modem's monotonic clock since it started |
| frame type | string | `BEACON`, `UI`, `CONNECT`, `CONNECT-ACK`, `DATA`, `ACK`, `SOUND`, `DISCONNECT`, `PROBE`, `CAL-REQUEST`, `POLL`, `EXTENDED`, `RESERVED-n` (SPEC §8.4) |
| mask | string | Tone mask, bit t set when tone t is usable: `0x` and 16 lower-case hex digits (`0x00ffffffffffffff` is all 56 tones) |
| mode | string | The waveform a header was decoded in: `cairn`; reserved for the multi-mode receiver (SPEC §13.7): `afsk1200`, `g3ruh9600`, `il2p` |

Units are in member names: `_s` seconds, `_ms` milliseconds, `_db` decibels,
`_dbfs` dB relative to full scale, `_bps` bits per second, `_kbps` kilobits
per second, `_hz` hertz. SNR is always the 7 dB-equivalent channel SNR of
SPEC §7.1 and ADR 0006. MCS is the rung of SPEC §6.2 (0-6 at v0.5). Width is
in tones (24-56).

## Requests and responses

A request is an object with `cmd` (a command name) and optionally `id`, plus
the command's arguments as members:

```json
{"id":"q1","cmd":"heard","limit":10}
```

`id` is a string of at most 64 characters, an integer, or null; the response
echoes it (`null` when the request had none). A successful response has
`"ok":true`, the command name in `cmd`, and the command's result members. A
failed one has `"ok":false` and an `error` object:

```json
{"id":"q2","ok":false,"cmd":"send","error":{"code":"no_session","message":"no session: connect first, or send with \"mode\":\"broadcast\""}}
```

When the line could not be parsed as a request at all, `id` and `cmd` are
null:

```json
{"id":null,"ok":false,"cmd":null,"error":{"code":"bad_json","message":"expected value at line 1 column 1"}}
```

### Error codes

| Code | Where | Meaning |
| --- | --- | --- |
| `bad_json` | response | The line is not a JSON object |
| `bad_request` | response | A member is missing, of the wrong type, or out of its range; or arguments contradict each other |
| `unknown_command` | response | `cmd` names no command |
| `unsupported` | response | The command, or an option of it, is not implemented here. `calibrate` and `capture` may answer this (SPEC §13.2); so may `send` for an `encoding` the modem cannot produce |
| `no_session` | response | The command needs a session and there is none |
| `busy` | response | A session (or its setup) is already in progress |
| `invalid_state` | response | Not now (for example `config` `cap` during a session) |
| `not_found` | response | A named file does not exist |
| `io_error` | response | Reading or writing a file failed |
| `too_large` | response | The payload does not fit one PDU (65,527 body bytes); send it as a `file` |
| `line_too_long` | response | The request line exceeded 1 MiB |
| `read_only` | response | `config` `set` named a parameter that cannot change while running |
| `out_of_range` | response | `config` `set` value outside its limits |
| `internal` | response, event | A bug; the message says what |
| `send_rejected` | event | The link layer refused a session send (`ref`) |
| `broadcast_rejected` | event | A broadcast could not be framed (`ref`) |
| `pdu_error` | event | A received PDU failed to parse, its body failed to decompress or decoded to more than 65,528 bytes, or its CRC-32 failed; it was not delivered |
| `unknown_dictionary` | response, event (warning) | `send`: the named dictionary is not one this modem has, or (session) not one negotiated with the peer. Event: a received PDU names a dictionary this modem lacks; it is delivered undecoded (`rx` `"decoded":false`) |
| `tx_failed` | event | A transmission failed (audio or PTT error); PTT was released |
| `receive_clipping` | event, warning | This receiver's input clipped on frames from `peer`: lower the receive level (SPEC §10.1) |
| `distortion` | event, warning | `peer` reports a distortion index of 3 or more: lower the transmit drive (SPEC §13.5) |
| `capture_failed` | event | An automatic capture could not be written |
| `ptt_fault` | event, warning | The PTT device failed while keying or releasing (for example the USB interface dropped out). The transmitter may still be keyed: check the interface and the radio. The modem retried dropping the line, closed and reopened the device, and refuses to key until it reopens; the message says which |

## Commands

Every implementation accepts every command below (SPEC §13.2). `calibrate`
and `capture` may answer `unsupported` instead; every other command is
required.

### `connect`

Open a session with a peer (SPEC §9).

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `peer` | address (in) | required | The station to connect to |
| `resume` | boolean | `true` | Resume state retained from a recent session with this peer, when there is any (SPEC §9.2, kept 10 minutes after a link loss); `false` discards it (undelivered PDUs of it are reported) and starts a new session |

Result: `peer` (address), `resuming` (boolean: retained state will be offered
for resume). Errors: `bad_request`, `busy`. Progress arrives as `session`
events: `connecting`, then `open` or `failed`.

```json
{"id":2,"cmd":"connect","peer":"n0call-2"}
{"id":2,"ok":true,"cmd":"connect","peer":"N0CALL-2","resuming":false}
```

### `disconnect`

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `graceful` | boolean | `true` | Close after everything queued has been acknowledged; `false` closes now |

Result: `peer`, `graceful`. Errors: `no_session`. Cancelling a session still
connecting ends it with `session` state `failed`, reason `cancelled`.

```json
{"id":3,"cmd":"disconnect"}
{"id":3,"ok":true,"cmd":"disconnect","peer":"N0CALL-2","graceful":true}
```

### `send`

Queue data in the session or as a broadcast.

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `"session"` or `"broadcast"` | `"session"` | Session PDUs are delivered in order within their stream, exactly once, or reported undelivered (SPEC §12.4); broadcasts are UI frames to everyone, unacknowledged |
| `text` | string | | UTF-8 text, sent as its bytes |
| `data` | string | | Bytes, base64 (RFC 4648 §4, with padding) |
| `file` | string | | Path of a file the modem reads |
| `pdu_type` | `"message"` or `"ax25"` | `"message"` | PDU type for `text` and `data` (SPEC §12.1). A `file` is always sent as FILE-CHUNK PDUs |
| `stream` | integer 0-63 | 0 | Stream number (SPEC §12.1) |
| `priority` | integer 0-3 | 0 (`file`: 3) | 0 is highest. Queued PDUs start highest priority first, first in first out within a priority; a PDU in progress is finished first (ADR 0009 `stream-interleaving-at-pdu-boundaries`) |
| `encoding` | `"auto"`, `"raw"`, `"zstd"`, `"zstd_dict"` | `"auto"` | Content encoding (SPEC §12.2). `auto` sends each PDU in whichever of raw, zstd and zstd with each usable dictionary makes it smallest; `raw`, `zstd` and `zstd_dict` force encoding 0, 1 or 2 |
| `dictionary` | string, 8 lower-case hex digits | | With `zstd_dict` only: the dictionary ID. In a session it must be one negotiated with the peer; without it the best negotiated one is used. A broadcast must name one |
| `repeats` | integer 0-3 | 0 | Broadcast only: identical repeats of each frame |

Exactly one of `text`, `data` and `file` is required. `text` and `data` make
one PDU of at most 65,527 bytes (`too_large` otherwise). A file is cut into
FILE-CHUNK PDUs (SPEC §12.4: a 32-bit object ID, a 32-bit offset, then up to
2,048 data bytes each); an object whose length is a multiple of 2,048 bytes,
or is empty, ends with an empty chunk at its length, so the receiver knows
where it ends (the protocol carries no object length).

Result: `ref` (integer naming this send in later events), `mode`, `bytes`
(payload bytes), `pdus`, `object` (file sends), `encoding` (the encoding
used; `mixed` when a file's chunks differ), `dictionary` (the dictionary ID
when the `zstd_dict` PDUs use one, else null) and `pdu_bytes` (the PDUs on
the block stream, Length field to CRC-32: what the send costs on the air
before block and frame overhead). Errors: `bad_request`, `no_session`
(session mode without a session; a send while the session is still
connecting is queued), `not_found`, `io_error`, `too_large`, `unsupported`,
`unknown_dictionary`.

Completion: a session send ends with `session` `delivered` (every PDU of it
acknowledged by the peer) or `undelivered`; a broadcast with `tx`
`broadcast_sent`, or an `error` `broadcast_rejected`.

```json
{"id":4,"cmd":"send","text":"hello from N0CALL-1"}
{"id":4,"ok":true,"cmd":"send","ref":7,"mode":"session","bytes":19,"pdus":1,"encoding":"raw","dictionary":null,"pdu_bytes":28}
{"id":5,"cmd":"send","file":"/home/op/report.txt","priority":2}
{"id":5,"ok":true,"cmd":"send","ref":8,"mode":"session","bytes":9000,"pdus":5,"object":1,"encoding":"zstd_dict","dictionary":"80301843","pdu_bytes":3650}
{"id":6,"cmd":"send","mode":"broadcast","data":"AQIDBA==","stream":1}
{"id":6,"ok":true,"cmd":"send","ref":9,"mode":"broadcast","bytes":4,"pdus":1,"encoding":"raw","dictionary":null,"pdu_bytes":13}
{"id":7,"cmd":"send","text":"ICS 213 GENERAL MESSAGE. TO: Planning Section Chief. FROM: Communications Unit Leader. SUBJECT: Repeater status. MESSAGE: The county repeater on 146.940 is back on emergency power; net control will resume at 1900 local. Please relay to all field teams. APPROVED: J. Smith, COML."}
{"id":7,"ok":true,"cmd":"send","ref":10,"mode":"session","bytes":278,"pdus":1,"encoding":"zstd_dict","dictionary":"80301843","pdu_bytes":142}
```

**Encodings.** A compressed body is one standard zstd frame (RFC 8878)
with the magic number and without checksum, in-frame dictionary ID or
content size (ADR 0014 `zstd-frame-format`); the PDU's Dictionary ID names
the dictionary. The dictionaries a session may use are those both stations
listed at setup (`status` `session.dictionaries`); a broadcast has no such
agreement, so `auto` sends broadcasts raw or as zstd without a dictionary,
and compresses one only when the result fits a single frame (only raw
messages are fragmented across frames); a forced `zstd` or `zstd_dict`
broadcast that does not fit answers `too_large`. A modem built without
zstd lists only `raw` in `hello` `encodings`, sends `auto` raw, and answers
`unsupported` to `zstd` and `zstd_dict`. In this implementation bodies up
to 1 kB are compressed at zstd level 19, longer ones (file chunks) at 9;
the level is the sender's choice and not on the air. Received bodies are
decoded before `rx`: one that decodes to more than 65,528 bytes (the
largest raw body), is malformed or fails its CRC-32 is discarded and
reported as `pdu_error`; one naming a dictionary the modem lacks is
delivered undecoded with an `unknown_dictionary` warning.

### `beacon`

A BEACON (SPEC §8.4) with this station's ID block and an optional status text.

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `text` | string | | Status text, carried as a MESSAGE PDU |
| `repeats` | integer 0-3 | 0 | Identical repeats |

Result: `ref`. Completion: `tx` `broadcast_sent`.

```json
{"id":7,"cmd":"beacon","text":"N0CALL-1 qrv"}
{"id":7,"ok":true,"cmd":"beacon","ref":10}
```

### `probe`

A link test (SPEC §9.6, ADR 0012) or a BEACON request.

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `target` | address (in) | required | The station probed |
| `kind` | `"link_test"` or `"beacon"` | `"link_test"` | A link test asks for a SOUND reply that measures both directions; `beacon` asks the target to send a BEACON |
| `repeats` | integer 0-5 | 2 | Repeats when unanswered |

Result: `ref`, `target`, `kind`. A link test ends with a `link` event of kind
`probe` (answered or not); a BEACON request with the target's `heard` event,
or a `link` `probe` with `"answered":false`. A station in a session does not
answer probes (ADR 0012), and this modem sends a probe only while its own
session is idle.

```json
{"id":8,"cmd":"probe","target":"N0CALL-2"}
{"id":8,"ok":true,"cmd":"probe","ref":11,"target":"N0CALL-2","kind":"link_test"}
```

### `heard`

The heard list (SPEC §13.6): one entry per station whose header this modem
decoded, in every mode.

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `since` | time | | Only stations last heard at or after this time |
| `limit` | integer | all | At most this many entries |

Result: `stations`, most recently heard first. Each entry:

| Member | Type | Meaning |
| --- | --- | --- |
| `address` | address | The station |
| `call`, `ssid` | string, integer | Callsign text (the address for a hash) and SSID |
| `first_heard`, `last_heard` | time | |
| `last_modem_time` | number or null | Modem time of the last header; null for an entry loaded from a heard file and not heard since |
| `count` | integer | Headers decoded |
| `last_snr_db`, `best_snr_db` | number | |
| `last_width` | integer | Width of the last frame |
| `last_frame_type` | frame type | |
| `last_mcs` | integer | |
| `last_dst` | address | Destination of the last frame |
| `frame_types` | object | Headers per frame type, e.g. `{"UI":3,"DATA":12}` |
| `modes` | array | Modes heard in |

```json
{"id":9,"cmd":"heard","limit":1}
{"id":9,"ok":true,"cmd":"heard","stations":[{"address":"N0CALL-2","call":"N0CALL","ssid":2,"first_heard":"2026-11-07T18:01:02.345Z","last_heard":"2026-11-07T18:03:00.010Z","last_modem_time":184.3,"count":14,"last_snr_db":24.8,"best_snr_db":25.6,"last_width":32,"last_frame_type":"DATA","last_mcs":4,"last_dst":"N0CALL-1","frame_types":{"CONNECT-ACK":1,"DATA":12,"BEACON":1},"modes":["cairn"]}]}
```

### `status`

Result members:

| Member | Type | Meaning |
| --- | --- | --- |
| `station` | address | |
| `time`, `modem_time` | time, number | Now |
| `session` | object or null | See below |
| `channel` | object | The channel state, as the `channel` event |
| `dcd` | object | `raw` (boolean), `detectors` (`quieting`, `preamble`, `afsk`, `cos`: booleans), `quieting_floor_dbfs` (the open-squelch floor the quieting detector learnt, null while unarmed) |
| `counters` | object | `samples`, `detections`, `header_failures`, `frames`, `blocks_ok`, `blocks`, `tx_frames`, `keyed_s`, `dcd_rises` (per detector), `audio_gaps` (capture gaps filled with zeros), `gapped_frames` (detections with a gap inside) |
| `capture` | object | `ring_seconds` (raw-audio ring length, 0 when off), `buffered_seconds` (audio in it now) |
| `audio` | object or null | The capture side (below); null where the modem has no sound card (a simulator) |
| `heard_stations` | integer | Entries in the heard list |
| `dictionaries` | array | The dictionary set this modem offers at session setup (SPEC §12.2, §13.4): `id` (8 hex digits), `name` (its file name), `bytes` |

`audio`: `backend` (`alsa`, `cpal` or `mock`), `period_ms` and `buffer_ms`
(the capture device's period and buffer), `realtime` (what the capture
thread got from the scheduler, e.g. `SCHED_RR 20 via rtkit`, or why not),
`xruns`, `gaps` and `gap_ms` (audio lost before it reached the modem),
`filled_samples` (zeros put in its place, so later frames keep their
timing), `overflows` (times the processing thread fell a whole ring behind),
`ring_ms` and `ring_high_water_ms` (the capture ring and its highest fill:
how far processing ever lagged), and `rx_gain`: null without receive AGC,
else `agc` (true), `control` (the mixer control), `gain_db`, `min_db`,
`max_db`, `changes` and `last_reason` (SPEC §13.5).

`session`: `peer`, `phase` (`connecting`, `half_open`, `open`, `closing`),
`role` (`initiator`, `responder`), `session_id`, `tx` and `rx` (each a
direction: `mcs`, `width`, `mask_id`, `mask`, `masked`; in `rx`, `mcs` is the
rung this station requests of the peer), `idle` (neither station owes a
reply), `drained` (everything queued is acknowledged), `progress` (as the
`session` `progress` event), `dictionaries` (the dictionary IDs negotiated
for this session: this modem's that the peer also listed; empty until the
session is open).

```json
{"id":10,"cmd":"status"}
{"id":10,"ok":true,"cmd":"status","station":"N0CALL-1","time":"2026-11-07T18:03:01.000Z","modem_time":185.3,"session":{"peer":"N0CALL-2","phase":"open","role":"initiator","session_id":4660,"tx":{"mcs":4,"width":32,"mask_id":1,"mask":"0x00000000fffffff8","masked":[0,1,2]},"rx":{"mcs":4,"width":32,"mask_id":0,"mask":"0x00000000ffffffff","masked":[]},"idle":false,"drained":false,"progress":{"tx":{"queued_bytes":9019,"acked_bytes":4096,"pending_bytes":4923,"rate_bps":5210.0,"eta_s":7.6},"rx":{"delivered_bytes":0,"rate_bps":0.0},"retransmissions":2,"blocks_sent":48},"dictionaries":["80301843"]},"channel":{"dcd":true,"dcd_raw":true,"keyed":false,"holder":"peer","holder_address":"N0CALL-2","turn":"peer","waiting":null,"next_yield_s":12.0,"legacy":false},"dcd":{"raw":true,"detectors":{"quieting":true,"preamble":true,"afsk":false,"cos":false},"quieting_floor_dbfs":-31.5},"counters":{"samples":8894400,"detections":31,"header_failures":0,"frames":31,"blocks_ok":402,"blocks":404,"tx_frames":30,"keyed_s":61.2,"dcd_rises":{"quieting":30,"preamble":31,"afsk":0,"cos":0},"audio_gaps":0,"gapped_frames":0},"capture":{"ring_seconds":120.0,"buffered_seconds":120.0},"audio":{"backend":"alsa","period_ms":10.0,"buffer_ms":200.0,"realtime":"SCHED_RR 20 via rtkit","xruns":0,"gaps":0,"gap_ms":0.0,"filled_samples":0,"overflows":0,"ring_ms":8000.0,"ring_high_water_ms":31.2,"rx_gain":{"agc":true,"control":"hw:CARD=AllInOneCable 'AIOC Audio In'","gain_db":-6.0,"min_db":-24.0,"max_db":0.0,"changes":2,"last_reason":"open-squelch noise -16.4 dBFS, above -17"}},"heard_stations":2,"dictionaries":[{"id":"80301843","name":"eval-forms-64k.zdict","bytes":65536}]}
```

### `config`

Read the runtime parameters, and change those that are safe to change live.

| Member | Type | Meaning |
| --- | --- | --- |
| `set` | object | Parameters to change (below); omit to only read |

Result: `config` (every parameter after the change) and `changed` (the names
changed, in the order given). The change is atomic: when any member of `set` fails, nothing
changes. Errors: `bad_request` (not a known parameter, or wrong type),
`read_only`, `out_of_range`, `invalid_state`.

| Parameter | Type | Range | Settable | Meaning |
| --- | --- | --- | --- | --- |
| `callsign` | address | | no | This station |
| `persistence` | number | (0, 1] | yes | p-persistence p (SPEC §11.1) |
| `slot_ms` | integer | 10-2550 | yes | Slot time |
| `max_frame_s` | number | 0.5-4.0 | yes | Maximum frame on-air time (SPEC §7.3) |
| `broadcast_mcs` | integer | 0-3 | yes | Broadcast rung |
| `cap` | integer | 24-56 | yes, outside a session | Width cap in tones; the B.3 width cap is `cap` - 1, capability bit 0 above 24; the quieting band follows unless it was set explicitly |
| `txdelay_ms` | integer | 0-2550 | yes | PTT to first audio sample (SPEC §7.3) |
| `txtail_ms` | integer | 0-2550 | yes | Last audio sample to PTT release |
| `accept_connects` | boolean | | yes | Accept incoming CONNECTs (false refuses them) |
| `width_cap` | integer | 23-55 | no | B.3 width cap (from `cap`) |
| `track_masks` | boolean | | no | Tone masks tracked (SPEC §10.3) |
| `repeater` | boolean | | no | Repeater mode (v1.x) |
| `ring_seconds` | number | | no | Raw-audio ring length |
| `quieting_hz` | [number, number] | | no | Quieting detector band |
| `rx_agc` | boolean | | no | Receive AGC on (`run --rx-agc`, SPEC §13.5) |
| `rx_gain_db` | number or null | | no | The capture gain the AGC set; null without AGC |
| `settable` | array | | | Names of the settable parameters |

```json
{"id":11,"cmd":"config","set":{"persistence":0.5,"txdelay_ms":200}}
{"id":11,"ok":true,"cmd":"config","config":{"callsign":"N0CALL-1","persistence":0.5,"slot_ms":100,"max_frame_s":4.0,"broadcast_mcs":0,"cap":32,"txdelay_ms":200,"txtail_ms":10,"accept_connects":true,"width_cap":31,"track_masks":true,"repeater":false,"ring_seconds":120.0,"quieting_hz":[3600.0,4150.0],"rx_agc":true,"rx_gain_db":-6.0,"settable":["persistence","slot_ms","max_frame_s","broadcast_mcs","cap","txdelay_ms","txtail_ms","accept_connects"]},"changed":["persistence","txdelay_ms"]}
```

### `calibrate`

The level-setting tone, turnaround measurement and stepped-tone drive
calibration of SPEC §13.4-13.5.

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `peer` | address (in) | | The cooperating station (turnaround and drive) |
| `step` | `"tone"`, `"turnaround"`, `"drive"`, `"all"` | `"all"` | |

An implementation with host level control answers with a `result` object
(not specified in this version). Any implementation may answer `unsupported`, and
this one does: it has no receive auto-gain or CAL-REQUEST exchange yet (SPEC
§21.3 step 7).

```json
{"id":12,"cmd":"calibrate","peer":"N0CALL-2"}
{"id":12,"ok":false,"cmd":"calibrate","error":{"code":"unsupported","message":"calibrate is not implemented in this modem (no host level control yet)"}}
```

### `capture`

Save the raw-audio ring buffer and the decoder state for the last N seconds
(SPEC §13.4, §13.6).

| Member | Type | Default | Meaning |
| --- | --- | --- | --- |
| `seconds` | number > 0 | the ring's length | How much audio, up to what the ring holds |
| `label` | string `[A-Za-z0-9_-]{1,32}` | | Added to the file names |

Result: `wav` and `sidecar` (paths), `seconds`, `samples`, `sample_rate`
(48000), `start_modem_time` (modem time of the first sample). Errors:
`unsupported` (no ring buffer), `bad_request`, `io_error`. The capture
directory is the modem's configuration, not the client's.

```json
{"id":13,"cmd":"capture","seconds":30,"label":"fade"}
{"id":13,"ok":true,"cmd":"capture","wav":"captures/cairn-20261107T180301Z-fade.wav","sidecar":"captures/cairn-20261107T180301Z-fade.json","seconds":30.0,"samples":1440000,"sample_rate":48000,"start_modem_time":155.3}
```

The WAV file is 16-bit mono PCM at 48 kHz: the input exactly as the sound
card delivered it, including whatever it picked up while this station was
keyed (the receiver ignores those samples; the sidecar's `transmissions`
says when). The sidecar is a JSON object:

| Member | Meaning |
| --- | --- |
| `format` | `"cairn-capture/1"` |
| `station`, `created` | This station; UTC time of the capture |
| `reason` | `"api"`, `"control"` or `"auto"` (a capture on failure) |
| `wav`, `sample_rate`, `samples` | The WAV file's name, 48000, its length |
| `start_sample`, `start_modem_time`, `end_modem_time` | Where the audio sits in the modem's input stream and clock |
| `frames` | Headers decoded in the window: `t` (seconds from the WAV's first sample to the frame's last symbol), `modem_time`, `src`, `dst`, `frame_type`, `session`, `seq`, `mcs`, `width`, `blocks`, `blocks_ok`, `controls`, `controls_ok`, `snr_db`, `ppm`, `decode_ms`, `unknown_mask` |
| `header_failures` | Preambles whose header failed: `t`, `modem_time` |
| `dcd` | Carrier-detect timeline: every raw detector change in the window, `t`, `modem_time`, `raw`, `detectors`; the first entry is the state at the window's start |
| `transmissions` | Own transmissions in the window: `t`, `modem_time` (keying), `frame_type`, `dst`, `mcs`, `keyed_s` |
| `link` | The `status` result at capture time (session, channel, counters) |
| `config` | The `config` result |

Captures are what conformance vectors are made from (SPEC §14.5).

### `subscribe`

Choose the events this connection receives (all, by default).

| Member | Type | Meaning |
| --- | --- | --- |
| `events` | array of event names, or `"all"` | Events to receive from now on; `hello` is always sent once at connect |

Result: `events` (the subscription now in force).

```json
{"id":14,"cmd":"subscribe","events":["rx","session","heard"]}
{"id":14,"ok":true,"cmd":"subscribe","events":["rx","session","heard"]}
```

## Events

Every event has `event` (its name), `time` and `modem_time` (when the modem
emitted it). Events go to every connection subscribed to them.

### `rx`

A PDU delivered to the host (SPEC §12.4): session PDUs in order within their
stream, exactly once; broadcast PDUs as received, duplicates within 60 s
suppressed.

| Member | Type | Meaning |
| --- | --- | --- |
| `from` | address | Source |
| `mode` | `"session"` or `"broadcast"` | |
| `frame_type` | `"DATA"`, `"UI"`, `"BEACON"` | What carried it (session PDUs: `DATA`) |
| `pdu_type`, `pdu_type_code` | string, integer | `ax25` 0, `message` 1, `file_chunk` 2, `signature` 3, `control` 4, else `unknown` (SPEC §12.1) |
| `stream`, `priority` | integer | From the PDU's stream byte |
| `encoding` | string | `raw`, `zstd`, `zstd_dict`, `unknown` |
| `dictionary` | string or null | Dictionary ID (8 hex digits) |
| `decoded` | boolean | False when the body is still compressed (a dictionary or encoding this modem lacks); `data` is then the body as received |
| `encoded_bytes` | integer | The body's bytes on the air (the compressed size for `zstd` and `zstd_dict`) |
| `crc` | `"ok"` or `"unchecked"` | `ok`: the PDU's CRC-32 checked over the decoded body. `unchecked`: the body is not decoded. PDUs that fail, or whose body fails to decompress, are not delivered; see `pdu_error` |
| `verify` | string | Signature result (SPEC §12.3): `unsigned`, `valid`, `invalid`, `unknown_key`. This implementation has no signatures yet: always `unsigned` |
| `bytes` | integer | Body length (`file_chunk`: the chunk's data) |
| `data` | string | Body, base64 (`file_chunk`: the data after object and offset) |
| `text` | string | `message` PDUs that are valid UTF-8: the body as text |
| `object`, `offset` | integer | `file_chunk` only |
| `saved` | object | When the modem saves received objects (`run --rx-dir`): `path`, `complete`, and for a completed file `bytes` and `sha256` |
| `audio_gap` | boolean | The frame that completed the PDU had a capture gap inside (audio lost and replaced by zeros); its blocks passed their CRC all the same |

```json
{"event":"rx","time":"2026-11-07T18:02:10.120Z","modem_time":154.4,"from":"N0CALL-2","mode":"session","frame_type":"DATA","pdu_type":"message","pdu_type_code":1,"stream":0,"priority":0,"encoding":"raw","dictionary":null,"decoded":true,"encoded_bytes":5,"crc":"ok","verify":"unsigned","bytes":5,"data":"aGVsbG8=","text":"hello","audio_gap":false}
{"event":"rx","time":"2026-11-07T18:02:14.500Z","modem_time":158.8,"from":"N0CALL-2","mode":"session","frame_type":"DATA","pdu_type":"file_chunk","pdu_type_code":2,"stream":0,"priority":3,"encoding":"raw","dictionary":null,"decoded":true,"encoded_bytes":11,"crc":"ok","verify":"unsigned","bytes":3,"data":"AAEC","object":1,"offset":8192,"saved":{"path":"rx/N0CALL-2_obj1.bin","complete":true,"bytes":8195,"sha256":"0f5a6e5e28e5a1d1c5a7e8b2a8f1c7c2b0a4d9a4c3f2e1d0c9b8a7f6e5d4c3b2"},"audio_gap":false}
```

### `session`

Session state and transfer progress (SPEC §13.6). `kind` says which:

- `state`: `state` is one of `connecting` (a CONNECT is being sent),
  `open` (the session is up; `initiator`, `resumed`), `closing` (the host
  asked to disconnect), `disconnected` (closed by the DISCONNECT handshake),
  `link_lost` (declared lost; state kept 10 minutes for `connect` with
  resume), `failed` (setup failed; `reason`: `refused`, `no_answer` after
  three retries, `half_open_timeout`, `busy`, `cancelled`), `refused` (this
  modem refused an incoming CONNECT), `expired` (retained state expired
  without a resume). `session_id`, `initiator`, `resumed` and `reason` are
  null when they do not apply.
- `progress`: `progress` has `tx` (`queued_bytes`, `acked_bytes`,
  `pending_bytes`, `rate_bps` over the last 30 s, `eta_s`), `rx`
  (`delivered_bytes`, `rate_bps`), `retransmissions` (data blocks sent
  again) and `blocks_sent`. Sent when the numbers change, at most twice a
  second, and when they reach their final values. Byte counts are payload
  bytes (text, data, file contents); `tx` counts the sends made through
  the API since the session with this peer began (a resumed session
  continues the count), not data queued through other interfaces such as
  KISS or a console.
- `delivered`: every PDU of send `ref` has been delivered by the peer;
  `bytes` is its payload length.
- `undelivered`: sends `refs` (and their `bytes` in total) were not
  delivered when the session ended (SPEC §12.4).

```json
{"event":"session","time":"2026-11-07T18:01:00.000Z","modem_time":104.3,"kind":"state","peer":"N0CALL-2","state":"connecting","session_id":null,"initiator":true,"resumed":null,"reason":null}
{"event":"session","time":"2026-11-07T18:01:01.900Z","modem_time":106.2,"kind":"state","peer":"N0CALL-2","state":"open","session_id":4660,"initiator":true,"resumed":false,"reason":null}
{"event":"session","time":"2026-11-07T18:01:20.000Z","modem_time":124.3,"kind":"progress","peer":"N0CALL-2","progress":{"tx":{"queued_bytes":9019,"acked_bytes":4096,"pending_bytes":4923,"rate_bps":5210.0,"eta_s":7.6},"rx":{"delivered_bytes":0,"rate_bps":0.0},"retransmissions":2,"blocks_sent":48}}
{"event":"session","time":"2026-11-07T18:01:28.000Z","modem_time":132.3,"kind":"delivered","peer":"N0CALL-2","ref":8,"bytes":9000}
{"event":"session","time":"2026-11-07T18:02:00.000Z","modem_time":164.3,"kind":"undelivered","peer":"N0CALL-2","refs":[12],"bytes":42}
```

### `link`

`kind` `frame`: one per frame decoded from the session peer (SPEC §13.6 link
dashboard).

| Member | Type | Meaning |
| --- | --- | --- |
| `peer` | address | |
| `frame_type`, `seq` | frame type, integer | The frame |
| `snr_db` | number | This frame's channel SNR as measured here |
| `ppm` | number | Sample-clock offset of the peer against this receiver |
| `frame_mcs` | integer | The rung the frame was sent at |
| `blocks`, `block_errors` | integer | Data blocks carried, and failed, in this frame |
| `bler` | number 0-1 | Block error rate over the last 16 frames from the peer that carried data |
| `rx` | direction | Peer to this station: `mcs` is the rung this station requests, `width`, mask |
| `tx` | direction | This station to the peer: the rung, width and mask it sends with |
| `far_end` | object or null | The peer's report of this station's last frame, from its ACK block (B.2): `snr_db`, `block_errors`, `distortion` (0-7), `requested_mcs`, `clipped`; null before the first |
| `distortion` | integer 0-7 | The distortion index this receiver measured (0 until the B.2 measurement is implemented) |
| `clipped` | boolean | This receiver's input clipped during the frame |
| `audio_gap` | object or null | A capture gap inside the frame (an xrun: audio lost before it reached the modem, replaced by as many zeros so the timing holds): `ms` (missing), `offset_ms` (from the frame's start), `gaps`; null when the frame's audio was complete |

`kind` `header_failed`: during a session, a preamble whose header did not
decode; the link layer counts it as any header failure. `peer`, `at` (modem
time of the frame's start) and `audio_gap` (as above: a gap explains the
failure, not the channel).

`kind` `probe`: the result of a `probe` (`ref`): `target`, `answered`, and
`forward` (this station to the target, measured there) and `reverse` (target
to this station), each `snr_db`, `mcs` (the rung the SNR supports) and
`net_kbps` (expected net rate), or null when unanswered or for a BEACON
request.

```json
{"event":"link","time":"2026-11-07T18:01:20.000Z","modem_time":124.3,"kind":"frame","peer":"N0CALL-2","frame_type":"ACK","seq":48,"snr_db":24.6,"ppm":-12.3,"frame_mcs":4,"blocks":0,"block_errors":0,"bler":0.02,"rx":{"mcs":4,"width":32,"mask_id":0,"mask":"0x00000000ffffffff","masked":[]},"tx":{"mcs":4,"width":32,"mask_id":1,"mask":"0x00000000fffffff8","masked":[0,1,2]},"far_end":{"snr_db":24.25,"block_errors":0,"distortion":0,"requested_mcs":4,"clipped":false},"distortion":0,"clipped":false,"audio_gap":null}
{"event":"link","time":"2026-11-07T18:01:24.410Z","modem_time":128.7,"kind":"header_failed","peer":"N0CALL-2","at":128.6,"audio_gap":{"ms":26.6,"offset_ms":61.0,"gaps":1}}
{"event":"link","time":"2026-11-07T18:00:30.000Z","modem_time":74.3,"kind":"probe","ref":11,"target":"N0CALL-2","answered":true,"forward":{"snr_db":14.2,"mcs":2,"net_kbps":3.02},"reverse":{"snr_db":14.0,"mcs":2,"net_kbps":3.02}}
```

### `heard`

Every decoded header, from any station (SPEC §13.6 heard list), whether or
not it was addressed here.

| Member | Type | Meaning |
| --- | --- | --- |
| `address`, `call`, `ssid` | | The source |
| `dst` | address | Destination (`*` for broadcast) |
| `frame_type`, `mode`, `session` | | Frame type, waveform, session flag |
| `mcs`, `width` | integer | |
| `snr_db`, `ppm` | number | |
| `blocks`, `blocks_ok` | integer | Data blocks carried and decoded |
| `count` | integer | Headers from this station so far (the heard-list entry's count) |

```json
{"event":"heard","time":"2026-11-07T18:01:01.880Z","modem_time":106.2,"address":"N0CALL-2","call":"N0CALL","ssid":2,"dst":"N0CALL-1","frame_type":"CONNECT-ACK","mode":"cairn","session":true,"mcs":0,"width":32,"snr_db":25.1,"ppm":-12.1,"blocks":0,"blocks_ok":0,"count":1}
```

### `channel`

Etiquette status (SPEC §13.6), sent whenever one of `dcd`, `dcd_raw`,
`keyed`, `holder`, `holder_address`, `turn`, `waiting.for`, `waiting.reason`
or `legacy` changes. The members are also the `channel` object of `status`.

| Member | Type | Meaning |
| --- | --- | --- |
| `dcd` | boolean | Carrier sense with hold (what channel access uses, SPEC §7.2) |
| `dcd_raw` | boolean | The OR of the raw detectors |
| `keyed` | boolean | This modem is transmitting |
| `holder` | string | Who holds the channel: `none`, `self`, `peer` (the session peer's frame is on the air, or carrier detect is still up after it), `other` (another decoded station, likewise), `unknown` (carrier, no header decoded yet) |
| `holder_address` | address or null | For `peer` and `other` |
| `turn` | `"self"`, `"peer"` or null | In a session, who owes the next frame |
| `waiting` | object or null | What this modem wants to send and why it has not: `for` (`connect`, `session_reply`, `session_data`, `keepalive`, `disconnect`, `owed_reply`, `broadcast`, `probe`), `reason` (below), `until_s` (seconds until the wait ends at the latest, or null) |
| `next_yield_s` | number or null | Seconds until the next yield window falls due in an open session (SPEC §11.1); 0 when due |
| `legacy` | boolean | Legacy activity in the last 10 minutes (SPEC §11.3) |

`waiting.reason`: `backoff` (a retry or a broadcast's spacing timer),
`carrier` (DCD is held: someone else is on the channel), `carrier_gone`
(a turn-holder reply waiting for the peer's carrier to drop), `turn_check`
(the 50 ms raw-DCD window before a turn-holder reply, ADR 0010), 
`yield_window` (holding off so other stations can take the channel),
`persistence` (p-persistence slots, SPEC §11.1), `clear` (just granted or
about to be).

```json
{"event":"channel","time":"2026-11-07T18:01:20.050Z","modem_time":124.35,"dcd":false,"dcd_raw":false,"keyed":false,"holder":"none","holder_address":null,"turn":"self","waiting":{"for":"session_reply","reason":"turn_check","until_s":0.05},"next_yield_s":4.2,"legacy":false}
```

### `dcd`

Every change of a raw carrier-detect detector (SPEC §7.2), without hold.
Clients that do not need this level of detail can leave it out with
`subscribe`.

| Member | Type | Meaning |
| --- | --- | --- |
| `raw` | boolean | OR of the detectors |
| `detectors` | object | `quieting`, `preamble`, `afsk`, `cos` |
| `at` | modem_time | The time of the edge in the audio |

```json
{"event":"dcd","time":"2026-11-07T18:01:18.010Z","modem_time":122.32,"raw":true,"detectors":{"quieting":true,"preamble":false,"afsk":false,"cos":false},"at":122.29}
```

### `tx`

Own transmissions. `kind` `start`: the modem keys for a frame (`frame_type`,
`dst`, `seq`, `mcs`, `width`, `blocks`, `turn_holder`, `keyed_s` the planned
keyed time). `kind` `done`: PTT released; `keyed_ms`, `txdelay_ms` and
`txtail_ms` as measured at the DAC, null where not measured (a simulator).
`kind` `broadcast_sent`: broadcast and beacon sends `refs` completely sent.

```json
{"event":"tx","time":"2026-11-07T18:01:00.100Z","modem_time":104.4,"kind":"start","frame_type":"CONNECT","dst":"N0CALL-2","seq":0,"mcs":0,"width":32,"blocks":0,"turn_holder":false,"keyed_s":0.69}
{"event":"tx","time":"2026-11-07T18:01:00.790Z","modem_time":105.09,"kind":"done","keyed_ms":690.2,"txdelay_ms":150.0,"txtail_ms":10.2}
{"event":"tx","time":"2026-11-07T18:00:05.000Z","modem_time":49.3,"kind":"broadcast_sent","refs":[9]}
```

### `error`

Asynchronous errors and warnings: `code` (see the table above), `message`,
`severity` (`error` or `warning`), `ref` (the send it concerns, or null),
`peer` (or null).

```json
{"event":"error","time":"2026-11-07T18:01:40.000Z","modem_time":144.3,"code":"receive_clipping","message":"receive input clipping on frames from N0CALL-2; lower the receive level","severity":"warning","ref":null,"peer":"N0CALL-2"}
```

## A session, start to finish

What a client sees (events filtered to `session`, `rx` and `tx` for brevity):

```json
{"id":1,"cmd":"connect","peer":"N0CALL-2"}
{"id":1,"ok":true,"cmd":"connect","peer":"N0CALL-2","resuming":false}
{"event":"session","time":"2026-11-07T18:01:00.000Z","modem_time":104.3,"kind":"state","peer":"N0CALL-2","state":"connecting","session_id":null,"initiator":true,"resumed":null,"reason":null}
{"event":"tx","time":"2026-11-07T18:01:00.100Z","modem_time":104.4,"kind":"start","frame_type":"CONNECT","dst":"N0CALL-2","seq":0,"mcs":0,"width":24,"blocks":0,"turn_holder":false,"keyed_s":0.58}
{"event":"session","time":"2026-11-07T18:01:01.900Z","modem_time":106.2,"kind":"state","peer":"N0CALL-2","state":"open","session_id":4660,"initiator":true,"resumed":false,"reason":null}
{"id":2,"cmd":"send","text":"hello","audio_gap":false}
{"id":2,"ok":true,"cmd":"send","ref":1,"mode":"session","bytes":5,"pdus":1,"encoding":"raw","dictionary":null,"pdu_bytes":14}
{"event":"session","time":"2026-11-07T18:01:06.000Z","modem_time":110.3,"kind":"delivered","peer":"N0CALL-2","ref":1,"bytes":5}
{"id":3,"cmd":"disconnect"}
{"id":3,"ok":true,"cmd":"disconnect","peer":"N0CALL-2","graceful":true}
{"event":"session","time":"2026-11-07T18:01:07.000Z","modem_time":111.3,"kind":"state","peer":"N0CALL-2","state":"closing","session_id":4660,"initiator":null,"resumed":null,"reason":null}
{"event":"session","time":"2026-11-07T18:01:09.000Z","modem_time":113.3,"kind":"state","peer":"N0CALL-2","state":"disconnected","session_id":4660,"initiator":null,"resumed":null,"reason":null}
```

## Conformance

- A modem implements every command (answering `unsupported` only for
  `calibrate` and `capture`) and emits every event that applies to what it
  does; `hello` lists them.
- Every line it sends validates against [`api.schema.json`](api.schema.json).
  The reference implementation's tests check every response and event they
  produce, and the repository's Python tests check every example on this page.
- `unsupported` is a valid answer to `calibrate` and `capture` in the API
  schema tests (SPEC §13.2).
