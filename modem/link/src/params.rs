//! Every timer, limit and constant of the link layer, in one place (SPEC
//! §21.3: "every algorithm parameter comes from one `params.rs`").
//!
//! Each item cites the SPEC section or ADR decision (by slug) it mirrors.
//! Values that are configuration rather than protocol constants are the
//! defaults; [`crate::Config`] carries the configured values. Items whose
//! name starts with `READING_` record a reading this crate chose where the
//! SPEC or the ADRs leave room (see the README, "Readings").

use std::time::Duration;

const fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

// --- SPEC §7.3 timing parameters ---------------------------------------------------

/// TXDelay default, PTT to first preamble sample (§7.3).
pub const TXDELAY_DEFAULT: Duration = ms(150);
/// TXDelay lower bound of the configurable range (§7.3).
pub const TXDELAY_MIN: Duration = ms(20);
/// TXDelay ceiling (§7.3); also the D term of the ACK timer (ADR 0010 `ack-timer-rules`).
pub const TXDELAY_MAX: Duration = ms(500);
/// Repeater access delay default (§7.3, §11.5).
pub const REPEATER_ACCESS_DELAY_DEFAULT: Duration = ms(400);
/// Repeater access delay ceiling (§7.3); added to D in repeater mode (ADR 0010 `ack-timer-rules`).
pub const REPEATER_ACCESS_DELAY_MAX: Duration = ms(1000);
/// TXTail default, silence after the last symbol before PTT release (§7.3).
pub const TXTAIL_DEFAULT: Duration = ms(10);
/// TXTail ceiling (§7.3).
pub const TXTAIL_MAX: Duration = ms(50);
/// Receive turnaround budget, end of the peer frame to own PTT (§7.3).
pub const RX_TURNAROUND: Duration = ms(100);
/// DCD hold default (§7.2, §7.3: 100 ms, range 50-300 ms).
pub const DCD_HOLD_DEFAULT: Duration = ms(100);
/// DCD hold lower bound (§7.3).
pub const DCD_HOLD_MIN: Duration = ms(50);
/// DCD hold upper bound (§7.3).
pub const DCD_HOLD_MAX: Duration = ms(300);
/// Maximum frame on-air time, a hard ceiling (§7.3, B.3; ADR 0010 `frame-time-definition`).
pub const MAX_FRAME_TIME: Duration = ms(4000);
/// Lowest configurable maximum frame on-air time (§7.3, B.3: 0.5 s).
pub const MIN_FRAME_TIME: Duration = ms(500);
/// Unit of B.3's Max frame time field (B.3: 0.1 s, values 5-40).
pub const FRAME_TIME_UNIT: Duration = ms(100);

// --- ACK timer (§7.3, §9.3; ADR 0010 `ack-timer-rules`) ----------------------------

/// Fixed 0.1 s term of T = 0.1 s + D + Y + F + 1.0 s.
pub const ACK_TIMER_BASE: Duration = ms(100);
/// Fixed 1.0 s margin of T.
pub const ACK_TIMER_MARGIN: Duration = ms(1000);
/// D in simplex: the TXDelay ceiling.
pub const ACK_TIMER_D_SIMPLEX: Duration = TXDELAY_MAX;
/// Extra D in repeater mode: the access-delay ceiling.
pub const ACK_TIMER_D_REPEATER_EXTRA: Duration = REPEATER_ACCESS_DELAY_MAX;
/// Offset added by the station that answered the CONNECT.
pub const ACK_TIMER_RESPONDER_OFFSET: Duration = ms(500);
/// Upper bound of the uniform jitter added at each start.
pub const ACK_TIMER_JITTER_MAX: Duration = ms(250);
/// Expirations 1..=this resend the frame (ADR 0010 `ack-timer-rules`).
pub const ACK_EXPIRY_LAST_RESEND: u8 = 2;
/// Expirations after the resends, up to and including this one, send SOUND.
pub const ACK_EXPIRY_LAST_SOUND: u8 = 7;
/// The expiration that declares the link lost.
pub const ACK_EXPIRY_LINK_LOST: u8 = 8;
/// CONNECT-ACK resends on ACK-timer expiry before the responder gives up
/// (ADR 0010 `duplicate-connect-handling`).
pub const CONNECT_ACK_MAX_RESENDS: u8 = 3;
/// DISCONNECT resends before the closing station closes anyway
/// (ADR 0010 `disconnect-handshake`).
pub const DISCONNECT_MAX_RESENDS: u8 = 3;
/// After replying to DISCONNECT a station answers repeats for "one ACK-timer
/// period" (ADR 0010 `disconnect-handshake`). Counted from the reply, that
/// period ends before the closer's first resend can arrive; this crate adds
/// the responder offset, the maximum jitter and the carrier-gone deadline
/// (plus one DISCONNECT's air time, computed by the station).
pub const READING_LINGER_EXTRA: Duration = Duration::from_millis(500 + 250 + 100);

// --- Session setup (§9.2; ADR 0010 `connect-retry-schedule`) -----------------------

/// CONNECT-ACK wait after releasing PTT, simplex.
pub const CONNECT_WAIT: Duration = ms(3000);
/// CONNECT-ACK wait in repeater mode.
pub const CONNECT_WAIT_REPEATER: Duration = ms(4000);
/// CONNECT is retried at most this many times.
pub const CONNECT_MAX_RETRIES: u8 = 3;
/// Lower bound of the uniform random backoff (ADR 0010 `retry-backoff-uniform`);
/// CONNECT retry k multiplies it by k.
pub const BACKOFF_MIN: Duration = ms(1000);
/// Upper bound of the uniform random backoff.
pub const BACKOFF_MAX: Duration = ms(5000);
/// Session state kept after link loss for resume (§9.2; ADR 0008
/// `session-resume-rules`, ADR 0010 `ack-timer-rules`).
pub const SESSION_RETENTION: Duration = Duration::from_secs(600);

// --- Idle sessions, yield, identification (§9.3, §11.2; ADR 0010) -------------------

/// Keepalive SOUND by the original initiator after this much idle time
/// (ADR 0010 `idle-keepalive`).
pub const KEEPALIVE_INITIATOR: Duration = Duration::from_secs(30);
/// Fallback keepalive by the original responder (plus jitter).
pub const KEEPALIVE_RESPONDER: Duration = Duration::from_secs(35);
/// Upper bound of the responder's keepalive jitter, drawn once per idle period.
pub const KEEPALIVE_RESPONDER_JITTER_MAX: Duration = ms(1000);
/// Yield clock period (ADR 0010 `yield-window-clock`).
pub const YIELD_INTERVAL: Duration = Duration::from_secs(30);
/// Yield window, simplex (§7.3, §11.2).
pub const YIELD_WINDOW: Duration = ms(1000);
/// Yield window in repeater mode (§11.5).
pub const YIELD_WINDOW_REPEATER: Duration = ms(2000);
/// An ID block goes on the first session frame once this much time has
/// passed since the last one (ADR 0010 `session-id-cadence`).
pub const ID_INTERVAL: Duration = Duration::from_secs(9 * 60);
/// Regulatory ceiling the 9-minute cadence protects (§3.3, B.6).
pub const ID_MAX_GAP: Duration = Duration::from_secs(10 * 60);

// --- Channel access (§11; ADR 0010 `csma-procedure-details`, `turn-holder-reply-check`)

/// DCD must be clear this long before the p-persistent draws (§11.1).
pub const CSMA_CLEAR_WAIT: Duration = ms(250);
/// Slot time default (§11.1).
pub const SLOT_TIME_DEFAULT: Duration = ms(100);
/// Persistence p default (§11.1).
pub const PERSISTENCE_DEFAULT: f64 = 0.25;
/// Turn-holder raw-DCD check window (§11.2).
pub const TURN_HOLDER_CHECK: Duration = ms(50);
/// If the peer's carrier has not gone this long after its last symbol the
/// channel counts as busy (§11.2).
pub const CARRIER_GONE_DEADLINE: Duration = ms(100);
/// Legacy activity within this window caps frames and halves p (§11.3).
pub const LEGACY_WINDOW: Duration = Duration::from_secs(600);
/// Frame on-air cap while legacy activity is recent (§11.3).
pub const LEGACY_FRAME_CAP: Duration = ms(2000);
/// KISS persistence mapping p = (P + 1) / 256 (§13.1).
pub fn kiss_persistence(p: u8) -> f64 {
    (p as f64 + 1.0) / 256.0
}
/// KISS TXDelay and slot-time unit (§13.1: 10 ms).
pub const KISS_TIME_UNIT: Duration = ms(10);

// --- Frames and blocks (§8, B; ADR 0008, ADR 0011) ----------------------------------

/// Protocol version (§8.2).
pub const VERSION: u8 = 1;
/// Data blocks per frame at most (§8.2: data block count 1-128).
pub const MAX_DATA_BLOCKS: usize = 128;
/// Control blocks per frame at most (§8.2, ADR 0010 `control-block-order`).
pub const MAX_CONTROL_BLOCKS: usize = 3;
/// Payload bytes of a data block (B.4).
pub const BLOCK_PAYLOAD: usize = 126;
/// Send window: never send a block at or beyond lowest unacked + 128
/// (ADR 0008 `send-window`).
pub const SEND_WINDOW: u16 = 128;
/// ACK and block-map bitmap span (B.2, B.8).
pub const BITMAP_BITS: usize = 128;
/// Serial-number arithmetic width (RFC 1982; ADR 0008 `block-sequence-rules`).
pub const SERIAL_BITS: u32 = 16;
/// Mask IDs are 2 bits (ADR 0011 `mask-id`).
pub const MASK_ID_MODULUS: u8 = 4;
/// Measured SNR field step (B.2: 0.25 dB).
pub const SNR_STEP_DB: f64 = 0.25;
/// Measured SNR field ceiling (B.2: 63.75 dB, saturating).
pub const SNR_MAX_DB: f64 = 63.75;
/// Distortion index at or above which a frame is excluded from step-down
/// counting (§10.1, ADR 0010 `adaptation-rules-revised`).
pub const DISTORTION_EXCLUDE_INDEX: u8 = 3;

// --- Width and masks (§5.4; ADR 0011) ----------------------------------------------

/// Base tones, the v0.5 width (§5.4, §16.2).
pub const BASE_WIDTH: u8 = 24;
/// Widest width (§5.2).
pub const MAX_WIDTH: u8 = 56;
/// Lowest width cap (B.3).
pub const WIDTH_CAP_MIN: u8 = 23;
/// Highest width cap (B.3).
pub const WIDTH_CAP_MAX: u8 = 55;
/// Pilot tones (§5.2).
pub const PILOT_TONES: [u8; 4] = [3, 20, 37, 52];
/// Pilots that cannot be masked (§5.4).
pub const UNMASKABLE_TONES: [u8; 2] = [3, 20];
/// All 56 tones usable.
pub const ALL_TONES: u64 = (1u64 << 56) - 1;
/// A tone is dropped when its SNR is more than this far below the
/// active-tone average (§10.3, ADR 0011 `mask-in-every-session-ack`).
pub const MASK_DROP_DB: f64 = 6.0;
/// ... for this many consecutive frames.
pub const MASK_DROP_FRAMES: u8 = 2;
/// A receiver with dropped tones asks for a re-probe (ACK flag bit 2) when
/// it has not seen a width probe for this long. The SPEC leaves the cadence
/// open (§10.3); this is the crate's choice.
pub const READING_REPROBE_INTERVAL: Duration = Duration::from_secs(10);

// --- Link adaptation (§6.2, §10; ADR 0010 `adaptation-rules-revised`) --------------

/// Number of rungs (§6.2).
pub const RUNGS: usize = 7;
/// §6.2 thresholds: 7 dB-equivalent SNR for 1% block error at 24 tones.
pub const RUNG_THRESHOLDS_DB: [f64; RUNGS] = [3.5, 6.0, 8.5, 12.5, 17.5, 21.0, 25.5];
/// Stage 2 hardware margin added to every threshold (§10.1); the SPEC leaves
/// the value to measurement, the default here is 1.0 dB.
pub const THRESHOLD_MARGIN_DEFAULT_DB: f64 = 1.0;
/// More than this fraction of failed data blocks steps down one rung.
pub const STEP_DOWN_FRACTION: f64 = 0.20;
/// More than this fraction steps down two rungs.
pub const STEP_DOWN_TWO_FRACTION: f64 = 0.50;
/// Clean frames with data needed before a step up.
pub const STEP_UP_RUN: u8 = 3;
/// SNR margin above the next rung's threshold needed to step up.
pub const STEP_UP_MARGIN_DB: f64 = 2.0;
/// ADR 0013 (multi-rung step-up): after the clean run, request the highest
/// rung whose threshold is at least [`STEP_UP_MARGIN_DB`] below the measured
/// SNR, not just the next rung (ADR 0010 `adaptation-rules-revised` stepped
/// one rung per run). `false` restores the one-rung rule.
pub const STEP_UP_MULTI_RUNG: bool = true;
/// READING (ADR 0013 names "the measured SNR" without saying which frame's):
/// the multi-rung target is taken from the lowest SNR of the frames in the
/// clean run, the conservative choice when the SNR moved during the run.
pub const READING_STEP_UP_SNR_IS_RUN_MINIMUM: bool = true;
/// A reply to CONNECT, CONNECT-ACK or SOUND requests the highest rung whose
/// threshold is at least this far below the measured SNR (also the link-test
/// suggestion, ADR 0012 `probe-link-test-reply`).
pub const REPLY_RULE_MARGIN_DB: f64 = 3.0;
/// §10.2 maximum frame time for rungs 0-1.
pub const RUNG_LOW_FRAME_LIMIT: Duration = ms(2000);
/// §10.2 maximum frame time for rungs 2-6.
pub const RUNG_HIGH_FRAME_LIMIT: Duration = ms(4000);

/// §10.2 maximum frame on-air time for a rung.
pub fn rung_frame_limit(mcs: u8) -> Duration {
    if mcs <= 1 { RUNG_LOW_FRAME_LIMIT } else { RUNG_HIGH_FRAME_LIMIT }
}

/// The §10.2 per-rung limit also applies to broadcast frames that carry
/// data. §10.2 sits under link adaptation; applying it everywhere is the
/// conservative reading (shorter frames on a shared channel).
pub const READING_RUNG_LIMIT_APPLIES_TO_BROADCAST: bool = true;

// --- Link test and broadcast (§9.1, §9.6; ADR 0009, ADR 0012) -----------------------

/// A PROBE with no reply within this long after PTT release is unreachable
/// (ADR 0012 `probe-link-test-reply`).
pub const PROBE_REPLY_TIMEOUT: Duration = ms(5000);
/// Broadcast duplicates are suppressed by source and frame counter within
/// this window (§12.4).
pub const BROADCAST_DEDUP_WINDOW: Duration = Duration::from_secs(60);
/// A partial fragmented message may be discarded after this long
/// (ADR 0009 `broadcast-message-fragments`).
pub const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(60);
/// Default broadcast rung (§9.1, §10.4).
pub const BROADCAST_MCS_DEFAULT: u8 = 0;
/// Highest permitted broadcast rung (§9.1: MCS 1-3 permitted).
pub const BROADCAST_MCS_MAX: u8 = 3;

// --- PDU framing (§12.1; ADR 0009) -------------------------------------------------

/// Length field bytes.
pub const PDU_LENGTH_BYTES: usize = 2;
/// Type, Stream and Encoding bytes.
pub const PDU_FIXED_BYTES: usize = 3;
/// Dictionary ID bytes (Encoding 2 only).
pub const PDU_DICT_BYTES: usize = 4;
/// CRC-32 bytes.
pub const PDU_CRC_BYTES: usize = 4;
/// Largest Length field value.
pub const PDU_MAX_LENGTH: usize = 65_535;
/// CRC-32/ISO-HDLC check value over "123456789" (ADR 0009 `pdu-crc32`).
pub const CRC32_CHECK: u32 = 0xCBF4_3926;

// --- Appendix A frame model --------------------------------------------------------

/// Symbol time with the short prefix (§5.2: 11.25 ms).
pub const SYMBOL_SHORT: Duration = Duration::from_micros(11_250);
/// Symbol time with the long prefix (§5.2, §11.5: 12.5 ms).
pub const SYMBOL_LONG: Duration = Duration::from_micros(12_500);
/// Preamble symbols (§6.1).
pub const PREAMBLE_SYMBOLS: u32 = 4;
/// Header symbols (A.1).
pub const HEADER_SYMBOLS: u32 = 7;
/// Width probe symbols (A.1).
pub const PROBE_SYMBOLS: u32 = 2;
/// Symbols per control block (A.1).
pub const CONTROL_SYMBOLS: u32 = 12;
/// Coded bits per data block by rung (A.3).
pub const CODED_BLOCK_BITS: [u32; RUNGS] = [2048, 1368, 2048, 1368, 1536, 1232, 1368];
/// Constellation bits per tone-symbol by rung (§6.2).
pub const BITS_PER_POINT: [u32; RUNGS] = [2, 2, 4, 4, 6, 6, 8];
