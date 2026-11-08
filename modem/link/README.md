# cairn-link

The Cairn link layer (SPEC §8–§12, ADRs 0008–0012) as a sans-IO Rust
crate. It consumes decoded-frame events and produces transmit requests; it
never touches audio, the PHY or a clock.

```sh
cd modem
cargo test --release -p cairn-link                         # unit, scenario and property tests
PROPTEST_CASES=2000 cargo test --release -p cairn-link --test properties
SIM_TRACE=1 cargo test --release -p cairn-link --test smoke -- --nocapture   # frame-by-frame trace
cargo build -p cairn-link --no-default-features        # the link layer alone, without cairn-phy
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
| `pdu` | ADR 0009 packing and parsing, filler, CRC-32/ISO-HDLC, broadcast fragmentation, reassembly with duplicate suppression. Kept alongside `cairn_phy::pdu` because the ARQ needs per-PDU completion ids, priority queues and lost-block parsing; `tests/pdu_vectors.rs` shows it byte-identical with `vectors/link/pdu_*.json` and, by property test, with `cairn_phy::pdu` |
| `seq` | RFC 1982 16-bit sequences |
| `arq` | Sender (window, retransmissions lowest first, block map, frame filling against a capacity), receiver (cumulative ACK, reassembly buffer, in-order delivery) |
| `adapt` | §10.1 rules as revised by ADR 0010 and ADR 0013 (multi-rung step-up), thresholds = §6.2 + margin, mask tracker with IDs |
| `access` | DCD hold, p-persistent CSMA, slot draws, turn-holder 50 ms raw-DCD check, yield windows |
| `session` | The ADR 0010 state machine |
| `resume` | State retained 10 minutes after link loss |
| `linktest` | PROBE link test and BEACON request (ADR 0012) |
| `broadcast` | UI / BEACON send path: frame counter, several PDUs per UI frame, fragments, identical repeats |
| `station` | The entry point and the arbitration between session, owed replies, PROBEs and broadcasts |
| `adapter` | The `FieldCodec` trait and, with the default `phy` feature, `PhyCodec`: field-by-field conversion to and from `cairn_phy::fields` (headers, ACK, B.3, ID, block map; CAL and path blocks opaque), the transmit `LinkState`, and `RxResult` to `RxFrame`. `tests/phy_adapter.rs` round-trips every frame type through the PHY bit packing, runs `check_controls` on every frame the simulated stations send, and carries session frames over the real waveform |
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

## ADR 0013

ADR 0013 settled the findings of the first version of this crate; each is
implemented with a test:

| Decision | Where | Test |
| --- | --- | --- |
| `adaptation-rules-multi-rung`: after 3 clean frames, the highest rung whose threshold is ≥ 2 dB below the lowest SNR of the three; clipped or distortion-index-≥3 frames neither step down nor up and restart the run | `adapt::RateAdapter`, `params::STEP_UP_MULTI_RUNG` | `adapt::tests::multi_rung_step_up`, `clipped_or_distorted_frames_restart_the_run` |
| `v1-performance-targets-revised`: right rung within 2 frames of a 6 dB drop and 4 of a rise; a 60 s outage recovers in the session, a 120 s one resumes | | scenarios `rung_settles_within_two_frames_after_6db_drop`, `rung_climbs_within_four_frames_after_6db_rise`, `transfer_survives_60s_...`, `resume_after_120s_link_loss_...` |
| `reprobe-request-cadence` (SHOULD): re-probe flag 10 s after the last probe while a tone is dropped, until a probe decodes | `adapt::MaskTracker`, `params::REPROBE_INTERVAL` | `adapt::tests::reprobe_requested_after_10s_...` |
| `duplicate-connect-and-half-open-retention`: a responder giving up on a half-open session keeps its state 10 minutes for resume | `station::end_session` | scenario `half_open_session_is_kept_for_resume` |
| `crossing-new-connects`: the station already connecting refuses the peer's CONNECT and keeps its own | `station::on_connect` | scenario `crossing_new_connects_leave_one_attempt` |
| `resume-keeps-roles`: roles (keepalive duty, responder offset) survive a resume by either station | `session::new_initiator`, `new_responder`, the idle clock restarted by role when CONNECT-ACK opens the session | scenario `resume_by_the_responder_keeps_the_original_roles` |
| `disconnect-handshake-linger`: L = T + 0.5 + 0.25 + 0.1 s + the reply's air time, restarted after each reply | `station::end_session`, `on_tx_done`, `params::LINGER_EXTRA` | scenarios `lost_disconnect_reply_is_answered_again_while_lingering`, `linger_restarts_after_each_disconnect_reply` |
| `broadcast-control-block-order`: BEACON and UI control blocks go ID, path, others | `LinkFrame::validate` (and `cairn_phy::fields::check_controls`) | `frame::tests::broadcast_control_order` (and `phy/tests/link_vectors.rs`) |

The resume-by-the-responder scenario found that a resuming original
responder ran the initiator's 30 s keepalive clock until the first frame
from its peer; the idle clock now restarts by role when CONNECT-ACK opens
the session.

## Readings

Where the SPEC and ADRs leave room the crate takes the conservative reading
and names it (`READING_*` constants in `params`, `READING:` comments):

- The §10.2 per-rung frame limit also applies to broadcast frames.
- A resend on ACK-timer expiry without data blocks after a DATA frame goes
  out as SOUND (a DATA frame cannot be empty).
- A yield window before a transmission from idle is followed by the full
  carrier sense; the yield window starts after held DCD is clear.
- The turn-holder's window and the yield window fail on any raw-DCD rising
  edge; a peer carrier still up 100 ms after its last symbol counts as busy.
- CSMA slot draws start at the current time, not at past slot boundaries.
- A graceful disconnect waits until everything queued is acknowledged.
