//! Virtual time. The link layer never reads a clock: every event carries the
//! caller's `now`, and timers are expressed as [`Instant`]s on that clock.

use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::time::Duration;

/// A point on the caller's monotonic clock, in microseconds from an
/// arbitrary origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(pub u64);

impl Instant {
    /// The clock origin.
    pub const ZERO: Instant = Instant(0);
    /// A time later than any the link layer will see.
    pub const FAR: Instant = Instant(u64::MAX / 2);

    /// From seconds.
    pub fn from_secs_f64(s: f64) -> Instant {
        Instant((s * 1e6).round() as u64)
    }

    /// As seconds.
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1e6
    }

    /// Time from `earlier` to `self`, zero if `earlier` is later.
    pub fn saturating_since(self, earlier: Instant) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(d.as_micros() as u64))
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Duration {
        self.saturating_since(rhs)
    }
}

impl Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, d: Duration) -> Instant {
        Instant(self.0.saturating_sub(d.as_micros() as u64))
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.6}s", self.as_secs_f64())
    }
}

/// A countdown that pauses while DCD is asserted (ADR 0010 `ack-timer-rules`,
/// `idle-keepalive`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PausableTimer {
    total: Duration,
    used: Duration,
    running_since: Option<Instant>,
}

impl PausableTimer {
    /// A timer of `total`, running from `now` unless `paused`.
    pub fn start(total: Duration, now: Instant, paused: bool) -> Self {
        PausableTimer { total, used: Duration::ZERO, running_since: if paused { None } else { Some(now) } }
    }

    /// Stop counting.
    pub fn pause(&mut self, now: Instant) {
        if let Some(s) = self.running_since.take() {
            self.used += now.saturating_since(s);
        }
    }

    /// Resume counting.
    pub fn resume(&mut self, now: Instant) {
        if self.running_since.is_none() {
            self.running_since = Some(now);
        }
    }

    /// Time counted so far.
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.used + self.running_since.map_or(Duration::ZERO, |s| now.saturating_since(s))
    }

    /// When the timer expires if it keeps running; `None` while paused.
    pub fn deadline(&self) -> Option<Instant> {
        self.running_since.map(|s| s + self.total.saturating_sub(self.used))
    }

    /// Whether the timer has run its full length.
    pub fn expired(&self, now: Instant) -> bool {
        self.elapsed(now) >= self.total
    }

    /// The configured length.
    pub fn total(&self) -> Duration {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pausable_timer_counts_only_while_running() {
        let mut t = PausableTimer::start(Duration::from_secs(3), Instant::ZERO, false);
        t.pause(Instant::from_secs_f64(1.0));
        assert_eq!(t.deadline(), None);
        t.resume(Instant::from_secs_f64(5.0));
        assert_eq!(t.deadline(), Some(Instant::from_secs_f64(7.0)));
        assert!(!t.expired(Instant::from_secs_f64(6.9)));
        assert!(t.expired(Instant::from_secs_f64(7.0)));
    }
}
