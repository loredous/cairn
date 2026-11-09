//! Session state kept after a link loss for resume (SPEC §9.2; ADR 0008
//! `session-resume-rules`, `resume-by-either-station`, `resume-flag-echo`).
//!
//! Kept: the session ID, both directions' sequence state (held and
//! unacknowledged blocks, the PDU queue and the part-way received PDU), the
//! negotiated capabilities and maximum frame time. Not kept (measured afresh
//! from the resume handshake): tone masks (IDs restart at 0), requested
//! rungs, the prefix.

use std::time::Duration;

use crate::arq::{Receiver, Sender};
use crate::frame::Address;
use crate::params::SESSION_RETENTION;
use crate::session::Role;
use crate::time::Instant;

/// A session interrupted by link loss.
#[derive(Clone, Debug)]
pub struct Retained {
    /// The peer.
    pub peer: Address,
    /// This station's role in the original setup.
    pub role: Role,
    /// Session ID.
    pub session_id: u16,
    /// Sending direction.
    pub tx: Sender,
    /// Receiving direction.
    pub rx: Receiver,
    /// Peer capabilities (B.3).
    pub peer_caps: u16,
    /// Peer width cap (B.3).
    pub peer_width_cap: u8,
    /// Peer maximum frame time (B.3).
    pub peer_max_frame: Duration,
    /// Dictionary IDs the peer listed at setup (SPEC §12.2).
    pub peer_dicts: Vec<u32>,
    /// When the link was declared lost.
    pub lost_at: Instant,
}

impl Retained {
    /// When this state is discarded.
    pub fn expires_at(&self) -> Instant {
        self.lost_at + SESSION_RETENTION
    }
}

/// The retained sessions, at most one per peer.
#[derive(Clone, Debug, Default)]
pub struct RetainedStore {
    items: Vec<Retained>,
}

impl RetainedStore {
    /// Keep a session, replacing any older one with the same peer (whose
    /// undelivered ids are returned).
    pub fn insert(&mut self, r: Retained) -> Option<Retained> {
        let old = self.take(r.peer);
        self.items.push(r);
        old
    }

    /// Take the session kept with a peer.
    pub fn take(&mut self, peer: Address) -> Option<Retained> {
        let i = self.items.iter().position(|r| r.peer == peer)?;
        Some(self.items.swap_remove(i))
    }

    /// The session kept with a peer.
    pub fn get(&self, peer: Address) -> Option<&Retained> {
        self.items.iter().find(|r| r.peer == peer)
    }

    /// Remove and return every session expired at `now`.
    pub fn expire(&mut self, now: Instant) -> Vec<Retained> {
        let (gone, keep): (Vec<_>, Vec<_>) = self.items.drain(..).partition(|r| now >= r.expires_at());
        self.items = keep;
        gone
    }

    /// The earliest expiry.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.items.iter().map(|r| r.expires_at()).min()
    }
}
