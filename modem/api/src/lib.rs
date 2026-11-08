//! The Cairn native host API (SPEC 13.2), transport side.
//!
//! The protocol is JSON lines: one JSON object per LF-terminated line, a
//! request `{"id":..,"cmd":..,...}`, a response `{"id":..,"ok":..,"cmd":..}`
//! or an event `{"event":..,"time":..,"modem_time":..}`. `docs/api.md` is
//! the normative schema and `docs/api.schema.json` its JSON Schema form.
//!
//! This crate knows nothing of the modem: it holds the protocol's
//! constants and message builders ([`proto`]), the TCP and Unix-socket
//! server with per-client subscriptions ([`server`]), the heard list
//! ([`heard`]), the raw-audio ring buffer and capture files ([`capture`]),
//! and a small JSON Schema validator for tests ([`schema`]). The modem side
//! (commands mapped onto the link layer, events from the runtime) is
//! `cairn_modem::hostapi`.

#![warn(missing_docs)]

pub mod capture;
pub mod heard;
pub mod proto;
pub mod schema;
pub mod server;

pub use proto::{API_VERSION, ApiError, Code};
