# cairn-link

The Cairn link layer (SPEC §8–§12, ADRs 0008–0012) as a sans-IO Rust
crate. It consumes decoded-frame events and produces transmit requests; it
never touches audio, the PHY or a clock.

```sh
cd modem
cargo test --release -p cairn-link                         # unit, scenario and property tests
PROPTEST_CASES=2000 cargo test --release -p cairn-link --test properties
SIM_TRACE=1 cargo test --release -p cairn-link --test smoke -- --nocapture   # frame-by-frame trace
```

## API

```rust
let mut st = Station::new(Config::for_callsign("N0AAA", 0), Box::new(AppendixA::default()));
let actions = st.handle(Event::Host(HostCommand::Connect { peer, new_session: false }), now);
```

`Station::handle(event, now) -> Vec<Action>` is the whole interface.

| Events | Actions |
| --- | --- |
| `Frame(RxFrame)`: header decoded, per-block results (`None` = CRC failed), SNR, distortion index, per-tone SNR, probe flag | `Transmit { frame, turn_holder }`: key PTT now |
| `HeaderFailed`: preamble detected, header failed | `Deliver { from, pdu }`: session PDU, in order, exactly once |
| `Dcd(bool)`: raw detector edges (the station applies the hold) | `DeliverBroadcast { from, ftype, pdu }` |
| `LegacyActivity`: AFSK detector or legacy frame (§11.3) | `Host(HostEvent)`: connected, refused, link lost, undelivered ids, acked ids, link-test result, warnings |
| `TxDone`: PTT released | `Wakeup(t)`: call again with `Tick` at `t` (the latest one wins) |
| `Host(HostCommand)`: connect (resumes retained state), disconnect, send, broadcast, beacon, probe | |
| `Tick` | |

`Station::rx_mask(&RxHeader)` tells the receiver which tone mask demaps a
frame's blocks (ADR 0011 `mask-id`). The caller supplies a `FrameModel`
(on-air time and blocks per frame for a frame shape); `AppendixA` is the
Appendix A symbol-count model, which reproduces the A.2 blocks-per-frame
table.

## Modules

| Module | Contents |
| --- | --- |
| `params` | Every timer, limit and constant, each with its SPEC/ADR reference |
| `frame` | `LinkFrame`, `RxFrame`, addresses, flags, ACK / B.3 / ID / block-map control blocks, control-block order validation |
| `pdu` | ADR 0009 packing and parsing, filler, CRC-32/ISO-HDLC, broadcast fragmentation, reassembly with duplicate suppression; bit-exact with `vectors/link/pdu_blocks.json` of `cairn-phy` PR #21 |
| `seq` | RFC 1982 16-bit sequences |
| `arq` | Sender (window, retransmissions lowest first, block map, frame filling against a capacity), receiver (cumulative ACK, reassembly buffer, in-order delivery) |
| `adapt` | §10.1 rules as revised by ADR 0010, thresholds = §6.2 + margin, mask tracker with IDs |
| `access` | DCD hold, p-persistent CSMA, slot draws, turn-holder 50 ms raw-DCD check, yield windows |
| `session` | The ADR 0010 state machine |
| `resume` | State retained 10 minutes after link loss |
| `linktest` | PROBE link test and BEACON request (ADR 0012) |
| `broadcast` | UI / BEACON send path: frame counter, several PDUs per UI frame, fragments, identical repeats |
| `station` | The entry point and the arbitration between session, owed replies, PROBEs and broadcasts |
| `adapter` | The `FieldCodec` trait and the field-by-field mapping onto `cairn_phy` |
| `capacity` | `FrameModel`, `AppendixA`, expected net rate |

## Simulation harness (`tests/common`)

A frame-level discrete-event air: stations are real `Station`s; a
transmission occupies the air from keying to PTT release (Appendix A
symbol counts with each station's TXDelay); listeners see raw DCD for that
interval and, 30 ms after the last symbol, a decoded frame, a failed header
or nothing. Losses: forced header, ACK-block and per-block loss per directed
link, a logistic block-error curve around the §6.2 thresholds (1% at the
threshold, 0.4 dB slope), tone impairments that cost 12 dB when the applied
mask still uses the tone, collisions between audible overlapping
transmissions, half-duplex deafness, scheduled outages and SNR steps,
foreign carriers, and a scripted per-reception filter. The harness flags
any transmission keyed while the station could hear a carrier, any frame
breaking the layout rules, and any mask mismatch between sender and
receiver.

## Readings

Where the SPEC and ADRs leave room the crate takes the conservative reading
and names it (`READING_*` constants in `params`, `READING:` comments):

- The re-probe cadence is unspecified: a receiver with dropped tones asks
  for a probe when it has seen none for 10 s.
- The §10.2 per-rung frame limit also applies to broadcast frames.
- Step-up needs the SNR condition on each of the three clean frames; a
  clipped or distortion-index-≥3 frame neither steps up nor down and
  restarts the run.
- A resend on ACK-timer expiry without data blocks after a DATA frame goes
  out as SOUND (a DATA frame cannot be empty).
- Crossing *new* CONNECTs (each station connecting to the other) are
  refused by the station already connecting; the other attempt proceeds.
- After a resume the original roles stay (keepalive duty, the +0.5 s ACK
  timer offset).
- A yield window before a transmission from idle is followed by the full
  carrier sense; the yield window starts after held DCD is clear.
- The turn-holder's window and the yield window fail on any raw-DCD rising
  edge; a peer carrier still up 100 ms after its last symbol counts as busy.
- CSMA slot draws start at the current time, not at past slot boundaries.
- A graceful disconnect waits until everything queued is acknowledged.
