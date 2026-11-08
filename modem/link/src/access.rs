//! Channel access (SPEC §7.2, §11; ADR 0010 `csma-procedure-details`,
//! `turn-holder-reply-check`, `yield-window-clock`, `transmission-from-idle`,
//! `retry-backoff-uniform`).
//!
//! DCD arrives as edges of the raw detectors (the OR of quieting or the
//! repeater-mode modulation detector, the preamble correlator, the AFSK
//! detector and COS, without hold). [`Dcd`] applies the hold itself, so the
//! turn-holder check can watch the raw detectors while carrier sense and
//! the timers use DCD with hold.
//!
//! An [`Attempt`] is one pending transmission's access procedure. The
//! station polls it whenever something changes; it answers "grant now" or
//! "wait until".

use std::time::Duration;

use crate::params::{CARRIER_GONE_DEADLINE, CSMA_CLEAR_WAIT, TURN_HOLDER_CHECK};
use crate::rng::Rng;
use crate::time::Instant;

/// DCD state from raw detector edges.
#[derive(Clone, Debug)]
pub struct Dcd {
    raw: bool,
    last_up: Option<Instant>,
    last_down: Option<Instant>,
    hold: Duration,
}

impl Dcd {
    /// Clear DCD with a hold time.
    pub fn new(hold: Duration) -> Self {
        Dcd { raw: false, last_up: None, last_down: None, hold }
    }

    /// A raw detector edge.
    pub fn set_raw(&mut self, up: bool, now: Instant) {
        if up && !self.raw {
            self.last_up = Some(now);
        } else if !up && self.raw {
            self.last_down = Some(now);
        }
        self.raw = up;
    }

    /// Raw detectors asserted.
    pub fn raw(&self) -> bool {
        self.raw
    }

    /// When the raw detectors last dropped.
    pub fn last_down(&self) -> Option<Instant> {
        self.last_down
    }

    /// DCD with hold.
    pub fn held(&self, now: Instant) -> bool {
        self.raw || self.last_down.is_some_and(|d| now < d + self.hold)
    }

    /// When held DCD last became clear (`None` while held).
    pub fn clear_since(&self, now: Instant) -> Option<Instant> {
        if self.held(now) { None } else { Some(self.last_down.map_or(Instant::ZERO, |d| d + self.hold)) }
    }

    /// When held DCD will clear if no detector asserts again (`None` while
    /// raw is up or already clear).
    pub fn held_clears_at(&self, now: Instant) -> Option<Instant> {
        if self.raw {
            return None;
        }
        self.last_down.map(|d| d + self.hold).filter(|&t| t > now)
    }

    /// A raw detector asserted after `t`, or is asserted now.
    pub fn rose_after(&self, t: Instant) -> bool {
        self.raw || self.last_up.is_some_and(|u| u > t)
    }
}

/// How a transmission gets the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A turn-holder reply (§11.2): the 50 ms raw-DCD check, no p-persistence.
    TurnHolder(Trigger),
    /// p-persistent carrier sense (§11.1), after an optional backoff.
    Csma {
        /// End of the random backoff (ADR 0010 `retry-backoff-uniform`).
        not_before: Instant,
    },
}

/// What a turn-holder reply answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A peer frame whose last symbol was at this time: the check window
    /// starts when the peer's carrier has gone.
    PeerFrame(Instant),
    /// An ACK-timer expiry at this time: the window starts at the expiry.
    Expiry(Instant),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterClear {
    Window,
    Yield { then_csma: bool },
    Sense,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Backoff(Instant),
    WaitCarrierGone(Instant),
    Window { start: Instant, until: Instant },
    WaitHeldClear(AfterClear),
    Yield { start: Instant, until: Instant, then_csma: bool },
    Sense,
    Slot { start: Instant, at: Instant },
}

/// Channel-access parameters for one poll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccessParams {
    /// Persistence p, already halved for legacy activity.
    pub p: f64,
    /// Slot time.
    pub slot: Duration,
    /// Yield window length.
    pub yield_window: Duration,
}

/// The result of polling an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Key now.
    Grant,
    /// Poll again at this time (or on the next DCD edge when `None`).
    Wait(Option<Instant>),
}

/// One pending transmission's access procedure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    origin: Mode,
    turn_holder: bool,
    phase: Phase,
    yield_due: bool,
    yield_taken: Option<Instant>,
}

impl Attempt {
    /// Start an attempt. `yield_due` asks for a yield window first
    /// (ADR 0010 `yield-window-clock`).
    pub fn new(mode: Mode, yield_due: bool, dcd: &Dcd, now: Instant) -> Self {
        let phase = match mode {
            Mode::TurnHolder(Trigger::PeerFrame(last)) => Phase::WaitCarrierGone(last + CARRIER_GONE_DEADLINE),
            Mode::TurnHolder(Trigger::Expiry(at)) => {
                if dcd.raw() {
                    Phase::WaitHeldClear(if yield_due {
                        AfterClear::Yield { then_csma: false }
                    } else {
                        AfterClear::Window
                    })
                } else if yield_due {
                    Phase::Yield { start: at, until: at + Duration::ZERO, then_csma: false }
                } else {
                    Phase::Window { start: at, until: at + TURN_HOLDER_CHECK }
                }
            }
            Mode::Csma { not_before } => {
                if not_before > now {
                    Phase::Backoff(not_before)
                } else {
                    Phase::WaitHeldClear(if yield_due {
                        AfterClear::Yield { then_csma: true }
                    } else {
                        AfterClear::Sense
                    })
                }
            }
        };
        Attempt { origin: mode, turn_holder: matches!(mode, Mode::TurnHolder(_)), phase, yield_due, yield_taken: None }
    }

    /// The mode the attempt was started with.
    pub fn origin(&self) -> Mode {
        self.origin
    }

    /// Whether a grant is still a turn-holder reply (a failed yield turns
    /// the rest of the attempt into a deferral under carrier sense).
    pub fn is_turn_holder(&self) -> bool {
        self.turn_holder
    }

    /// When a yield window was taken (completed or interrupted), once.
    pub fn take_yield(&mut self) -> Option<Instant> {
        self.yield_taken.take()
    }

    /// Advance the procedure to `now`.
    pub fn poll(&mut self, dcd: &Dcd, now: Instant, prm: &AccessParams, rng: &mut Rng) -> Poll {
        loop {
            let next = match self.phase {
                Phase::Backoff(until) => {
                    if now < until {
                        return Poll::Wait(Some(until));
                    }
                    Phase::WaitHeldClear(if self.yield_due {
                        AfterClear::Yield { then_csma: true }
                    } else {
                        AfterClear::Sense
                    })
                }
                Phase::WaitCarrierGone(deadline) => {
                    if dcd.raw() {
                        if now >= deadline {
                            // Carrier not gone 100 ms after the last symbol: busy.
                            Phase::WaitHeldClear(self.after_clear_th())
                        } else {
                            return Poll::Wait(Some(deadline));
                        }
                    } else {
                        let last_symbol = deadline - CARRIER_GONE_DEADLINE;
                        let gone = dcd.last_down().map_or(last_symbol, |d| d.max(last_symbol));
                        if self.yield_due {
                            Phase::Yield { start: gone, until: gone + prm.yield_window, then_csma: false }
                        } else {
                            Phase::Window { start: gone, until: gone + TURN_HOLDER_CHECK }
                        }
                    }
                }
                Phase::Window { start, until } => {
                    if dcd.rose_after(start) {
                        Phase::WaitHeldClear(AfterClear::Window)
                    } else if now >= until {
                        return Poll::Grant;
                    } else {
                        return Poll::Wait(Some(until));
                    }
                }
                Phase::WaitHeldClear(after) => match dcd.clear_since(now) {
                    None => return Poll::Wait(dcd.held_clears_at(now)),
                    Some(clear) => match after {
                        AfterClear::Window => Phase::Window { start: clear, until: clear + TURN_HOLDER_CHECK },
                        AfterClear::Yield { then_csma } => {
                            let start = if then_csma { now } else { clear };
                            Phase::Yield { start, until: start + prm.yield_window, then_csma }
                        }
                        AfterClear::Sense => Phase::Sense,
                    },
                },
                Phase::Yield { start, until, then_csma } => {
                    let until = until.max(start + prm.yield_window);
                    if dcd.rose_after(start) {
                        // Other stations took the channel: defer as a new
                        // transmission once DCD clears (§11.1).
                        self.yield_taken = Some(now);
                        self.yield_due = false;
                        self.turn_holder = false;
                        Phase::WaitHeldClear(AfterClear::Sense)
                    } else if now >= until {
                        self.yield_taken = Some(until);
                        self.yield_due = false;
                        if then_csma {
                            Phase::Sense
                        } else {
                            return Poll::Grant;
                        }
                    } else {
                        return Poll::Wait(Some(until));
                    }
                }
                Phase::Sense => match dcd.clear_since(now) {
                    None => return Poll::Wait(dcd.held_clears_at(now)),
                    Some(clear) => Phase::Slot { start: clear, at: (clear + CSMA_CLEAR_WAIT).max(now) },
                },
                Phase::Slot { start, at } => {
                    if dcd.held(now) || dcd.rose_after(start) {
                        Phase::Sense
                    } else if now < at {
                        return Poll::Wait(Some(at));
                    } else if rng.chance(prm.p) {
                        return Poll::Grant;
                    } else {
                        Phase::Slot { start, at: at + prm.slot }
                    }
                }
            };
            self.phase = next;
        }
    }

    fn after_clear_th(&self) -> AfterClear {
        if self.yield_due { AfterClear::Yield { then_csma: false } } else { AfterClear::Window }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: f64) -> Instant {
        Instant::from_secs_f64(s)
    }

    fn prm() -> AccessParams {
        AccessParams { p: 1.0, slot: Duration::from_millis(100), yield_window: Duration::from_secs(1) }
    }

    #[test]
    fn turn_holder_keys_50ms_after_carrier_drops() {
        let mut dcd = Dcd::new(Duration::from_millis(100));
        let mut rng = Rng::new(1);
        dcd.set_raw(true, t(0.0));
        dcd.set_raw(false, t(1.01));
        let mut a = Attempt::new(Mode::TurnHolder(Trigger::PeerFrame(t(1.0))), false, &dcd, t(1.03));
        assert_eq!(a.poll(&dcd, t(1.03), &prm(), &mut rng), Poll::Wait(Some(t(1.06))));
        assert_eq!(a.poll(&dcd, t(1.06), &prm(), &mut rng), Poll::Grant);
    }

    #[test]
    fn turn_holder_waits_out_a_third_station() {
        let mut dcd = Dcd::new(Duration::from_millis(100));
        let mut rng = Rng::new(1);
        dcd.set_raw(true, t(0.0));
        dcd.set_raw(false, t(1.01));
        let mut a = Attempt::new(Mode::TurnHolder(Trigger::PeerFrame(t(1.0))), false, &dcd, t(1.02));
        assert_eq!(a.poll(&dcd, t(1.02), &prm(), &mut rng), Poll::Wait(Some(t(1.06))));
        dcd.set_raw(true, t(1.04)); // third station keys up inside the window
        assert_eq!(a.poll(&dcd, t(1.04), &prm(), &mut rng), Poll::Wait(None));
        dcd.set_raw(false, t(3.0));
        assert_eq!(a.poll(&dcd, t(3.0), &prm(), &mut rng), Poll::Wait(Some(t(3.1))));
        assert_eq!(a.poll(&dcd, t(3.1), &prm(), &mut rng), Poll::Wait(Some(t(3.15))));
        assert_eq!(a.poll(&dcd, t(3.15), &prm(), &mut rng), Poll::Grant);
        assert!(a.is_turn_holder());
    }

    #[test]
    fn carrier_that_never_drops_counts_as_busy() {
        let mut dcd = Dcd::new(Duration::from_millis(100));
        let mut rng = Rng::new(1);
        dcd.set_raw(true, t(0.0));
        let mut a = Attempt::new(Mode::TurnHolder(Trigger::PeerFrame(t(1.0))), false, &dcd, t(1.02));
        assert_eq!(a.poll(&dcd, t(1.02), &prm(), &mut rng), Poll::Wait(Some(t(1.1))));
        assert_eq!(a.poll(&dcd, t(1.1), &prm(), &mut rng), Poll::Wait(None));
    }

    #[test]
    fn csma_waits_250ms_then_draws() {
        let mut dcd = Dcd::new(Duration::from_millis(100));
        let mut rng = Rng::new(7);
        dcd.set_raw(true, t(0.0));
        dcd.set_raw(false, t(1.0));
        let mut a = Attempt::new(Mode::Csma { not_before: t(0.5) }, false, &dcd, t(0.5));
        assert_eq!(a.poll(&dcd, t(0.5), &prm(), &mut rng), Poll::Wait(Some(t(1.1))));
        assert_eq!(a.poll(&dcd, t(1.1), &prm(), &mut rng), Poll::Wait(Some(t(1.35))));
        assert_eq!(a.poll(&dcd, t(1.35), &prm(), &mut rng), Poll::Grant);
        // With p = 0.25 the draws land on slot boundaries.
        let mut a = Attempt::new(Mode::Csma { not_before: t(2.0) }, false, &dcd, t(2.0));
        let p = AccessParams { p: 0.25, ..prm() };
        let mut now = t(2.0);
        loop {
            match a.poll(&dcd, now, &p, &mut rng) {
                Poll::Grant => break,
                Poll::Wait(Some(w)) => now = w,
                Poll::Wait(None) => panic!(),
            }
        }
        let since = (now - t(2.0)).as_micros() % 100_000;
        assert_eq!(since, 0);
    }

    #[test]
    fn failed_yield_defers_under_csma() {
        let mut dcd = Dcd::new(Duration::from_millis(100));
        let mut rng = Rng::new(1);
        dcd.set_raw(true, t(0.0));
        dcd.set_raw(false, t(1.01));
        let mut a = Attempt::new(Mode::TurnHolder(Trigger::PeerFrame(t(1.0))), true, &dcd, t(1.02));
        assert_eq!(a.poll(&dcd, t(1.02), &prm(), &mut rng), Poll::Wait(Some(t(2.01))));
        dcd.set_raw(true, t(1.5));
        assert_eq!(a.poll(&dcd, t(1.5), &prm(), &mut rng), Poll::Wait(None));
        assert_eq!(a.take_yield(), Some(t(1.5)));
        assert!(!a.is_turn_holder());
        dcd.set_raw(false, t(4.0));
        assert_eq!(a.poll(&dcd, t(4.0), &prm(), &mut rng), Poll::Wait(Some(t(4.1))));
        assert_eq!(a.poll(&dcd, t(4.1), &prm(), &mut rng), Poll::Wait(Some(t(4.35))));
        assert_eq!(a.poll(&dcd, t(4.35), &prm(), &mut rng), Poll::Grant);
    }
}
