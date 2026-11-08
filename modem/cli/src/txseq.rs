//! Transmit sequencing (SPEC 7.3): key, TXDelay, audio, drain, TXTail, unkey.
//!
//! The first audio sample is placed TXDelay after keying by queuing a lead
//! of silence shortened by the output latency, and the unkey waits for the
//! DAC: the output callback reports when each sample plays
//! ([`OutputPort::play_time_ns`]), so the PTT drops TXTail after the last
//! sample actually leaves the sound card, not after a guessed sleep. Every
//! exit path unkeys: the [`PttGuard`] releases on return, error and panic,
//! and the stop flag (set by the signal handler) aborts the feed.

use std::sync::atomic::{AtomicBool, Ordering};

use cairn_audio::{AudioError, OutputPort, SAMPLE_RATE};
use cairn_ptt::{PttGuard, SharedPtt};

const NS: u64 = 1_000_000_000;
/// Feed and drain poll interval.
const POLL_NS: u64 = 2_000_000;
/// Poll interval while waiting for a callback to time the first sample.
const SYNC_POLL_NS: u64 = 250_000;
/// Longest wait for that callback.
const SYNC_NS: u64 = 50_000_000;
/// Give up if the device takes nothing for this long.
const STALL_NS: u64 = 1_000_000_000;

/// TXDelay and TXTail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxTiming {
    /// PTT to first audio sample at the DAC, s (SPEC 7.3: default 150 ms).
    pub txdelay_s: f64,
    /// Last sample at the DAC to PTT release, s (SPEC 7.3: default 10 ms).
    pub txtail_s: f64,
}

impl Default for TxTiming {
    fn default() -> Self {
        TxTiming { txdelay_s: 0.150, txtail_s: 0.010 }
    }
}

/// What one keyed transmission measured (ns on the port's clock).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxReport {
    /// PTT keyed.
    pub key_ns: u64,
    /// First audio sample at the DAC (predicted by the callback timestamps).
    pub first_play_ns: u64,
    /// End of the last audio sample at the DAC.
    pub end_play_ns: u64,
    /// PTT released.
    pub unkey_ns: u64,
    /// Output underruns during the transmission.
    pub underruns: u64,
}

impl TxReport {
    /// Measured TXDelay, ms.
    pub fn txdelay_ms(&self) -> f64 {
        (self.first_play_ns as f64 - self.key_ns as f64) / 1e6
    }
    /// Measured TXTail, ms.
    pub fn txtail_ms(&self) -> f64 {
        (self.unkey_ns as f64 - self.end_play_ns as f64) / 1e6
    }
    /// Total keyed time, ms.
    pub fn keyed_ms(&self) -> f64 {
        (self.unkey_ns - self.key_ns) as f64 / 1e6
    }
}

/// Why a transmission stopped early.
#[derive(Debug)]
pub enum TxError {
    /// PTT failed.
    Ptt(std::io::Error),
    /// The audio device failed.
    Audio(AudioError),
    /// The device stopped taking samples.
    Stalled,
    /// Stopped by a signal.
    Aborted,
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::Ptt(e) => write!(f, "PTT: {e}"),
            TxError::Audio(e) => write!(f, "audio: {e}"),
            TxError::Stalled => f.write_str("the output device stopped taking samples"),
            TxError::Aborted => f.write_str("stopped"),
        }
    }
}

impl std::error::Error for TxError {}

impl From<AudioError> for TxError {
    fn from(e: AudioError) -> Self {
        TxError::Audio(e)
    }
}

/// The operator-facing text of a failed transmission: whether the PTT is
/// known released, or the device failed (`ptt_warned`) and the transmitter
/// may still be keyed.
pub fn failure_text(e: &TxError, ptt_warned: bool) -> String {
    if ptt_warned {
        format!("{e} (PTT MAY STILL BE KEYED: check the interface and the radio)")
    } else {
        format!("{e} (PTT released)")
    }
}

fn samples_ns(n: u64) -> u64 {
    n * NS / SAMPLE_RATE as u64
}

/// Keyed time a transmission of `n_samples` will take, s.
pub fn keyed_seconds(n_samples: usize, timing: &TxTiming) -> f64 {
    timing.txdelay_s + n_samples as f64 / SAMPLE_RATE as f64 + timing.txtail_s
}

/// Key, play `audio` TXDelay after keying, wait until its last sample has
/// played, hold TXTail, unkey.
pub fn transmit(
    port: &mut dyn OutputPort,
    ptt: &SharedPtt,
    audio: &[f32],
    timing: &TxTiming,
    stop: &AtomicBool,
) -> Result<TxReport, TxError> {
    static SILENCE: [f32; 1024] = [0.0; 1024];
    let underruns0 = port.stats().underruns;
    let guard = PttGuard::key(ptt).map_err(TxError::Ptt)?;
    let key_ns = port.now_ns();

    // Lead silence: from where a sample queued now will play to key + TXDelay.
    // Right after a callback the next one's DAC time is known; wait for that
    // (at most a period), else fall back on the latency estimate.
    let txdelay_ns = (timing.txdelay_s * 1e9) as u64;
    let target = key_ns + txdelay_ns;
    let mut start = None;
    while start.is_none() && port.now_ns() - key_ns < SYNC_NS {
        start = port.next_start_ns();
        if start.is_none() {
            port.sleep_ns(SYNC_POLL_NS);
        }
    }
    let start = start.unwrap_or_else(|| port.now_ns() + port.latency_ns());
    let lead = (target.saturating_sub(start) * SAMPLE_RATE as u64 / NS) as usize;
    let base = port.written();
    let first = base + lead as u64;
    let end = first + audio.len() as u64;

    let mut lead_left = lead;
    let mut pos = 0usize;
    let mut announced = false;
    // Noted as soon as known: a long transmission can outlive the timing history.
    let mut first_play = None;
    let mut last_accept = port.now_ns();
    while lead_left > 0 || pos < audio.len() {
        if stop.load(Ordering::Relaxed) {
            return Err(TxError::Aborted);
        }
        let n = if lead_left > 0 {
            let k = port.write(&SILENCE[..lead_left.min(SILENCE.len())])?;
            lead_left -= k;
            k
        } else {
            let k = port.write(&audio[pos..])?;
            pos += k;
            k
        };
        if !announced && n > 0 {
            // Only now is there something queued; running dry before `end` is an underrun.
            port.expect_until(end);
            announced = true;
        }
        if n == 0 {
            port.sleep_ns(POLL_NS);
            if first_play.is_none() {
                first_play = port.play_time_ns(first);
            }
            let now = port.now_ns();
            if now - last_accept > STALL_NS {
                return Err(TxError::Stalled);
            }
        } else {
            last_accept = port.now_ns();
        }
    }
    if !announced {
        // Nothing to play at all.
        guard.release().map_err(TxError::Ptt)?;
        let now = port.now_ns();
        return Ok(TxReport { key_ns, first_play_ns: now, end_play_ns: now, unkey_ns: now, underruns: 0 });
    }

    // Drain: wait for the device to take the last sample.
    let mut last = port.consumed();
    let mut last_progress = port.now_ns();
    while port.consumed() < end {
        if stop.load(Ordering::Relaxed) {
            return Err(TxError::Aborted);
        }
        port.sleep_ns(POLL_NS);
        if first_play.is_none() {
            first_play = port.play_time_ns(first);
        }
        let c = port.consumed();
        let now = port.now_ns();
        if c != last {
            last = c;
            last_progress = now;
        } else if now - last_progress > STALL_NS {
            return Err(TxError::Stalled);
        }
    }
    let now = port.now_ns();
    let first_play_ns = first_play.or_else(|| port.play_time_ns(first)).unwrap_or(target);
    let end_play_ns = port.play_time_ns(end - 1).map(|t| t + samples_ns(1)).unwrap_or(now + port.latency_ns());
    let unkey_at = end_play_ns + (timing.txtail_s * 1e9) as u64;
    while port.now_ns() < unkey_at {
        if stop.load(Ordering::Relaxed) {
            return Err(TxError::Aborted);
        }
        let left = unkey_at - port.now_ns();
        port.sleep_ns(left.min(10 * POLL_NS));
    }
    guard.release().map_err(TxError::Ptt)?;
    let unkey_ns = port.now_ns();
    Ok(TxReport { key_ns, first_play_ns, end_play_ns, unkey_ns, underruns: port.stats().underruns - underruns0 })
}

#[cfg(test)]
pub(crate) mod mock {
    //! A virtual-clock output device and a PTT that logs to the same clock.

    use std::io;
    use std::sync::{Arc, Mutex};

    use cairn_audio::{AudioError, OutputPort, OutputStats, SAMPLE_RATE, TimingEvent, TimingTrack};
    use cairn_ptt::Ptt;

    use super::NS;

    /// Events with virtual times.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Ev {
        Key(u64),
        Unkey(u64),
        /// A callback took `n` samples from index `first`, which play from `play_ns`.
        Played {
            first: u64,
            n: u32,
            play_ns: u64,
            nonzero: bool,
        },
    }

    #[derive(Default)]
    pub struct World {
        pub now: u64,
        pub log: Vec<Ev>,
    }

    pub type Shared = Arc<Mutex<World>>;

    pub struct MockPtt(pub Shared, pub Arc<Mutex<bool>>);

    impl Ptt for MockPtt {
        fn set(&mut self, keyed: bool) -> io::Result<()> {
            let mut w = self.0.lock().unwrap();
            let t = w.now;
            *self.1.lock().unwrap() = keyed;
            w.log.push(if keyed { Ev::Key(t) } else { Ev::Unkey(t) });
            Ok(())
        }
        fn describe(&self) -> String {
            "mock".into()
        }
    }

    /// Callbacks every `period` frames, each taking up to `period` samples
    /// from a ring of `cap`; playback `latency_ns` after the callback.
    pub struct MockPort {
        pub world: Shared,
        pub period: u64,
        pub cap: usize,
        pub latency_ns: u64,
        queue: std::collections::VecDeque<f32>,
        written: u64,
        consumed: u64,
        next_cb: u64,
        expected_end: u64,
        underruns: u64,
        track: TimingTrack,
        /// Fail the write after this many calls.
        pub fail_write_after: Option<usize>,
        /// Panic in sleep once the clock passes this time.
        pub panic_after_ns: Option<u64>,
        writes: usize,
    }

    impl MockPort {
        pub fn new(world: Shared, period: u64, cap: usize, latency_ns: u64) -> Self {
            MockPort {
                world,
                period,
                cap,
                latency_ns,
                queue: Default::default(),
                written: 0,
                consumed: 0,
                next_cb: 0,
                expected_end: 0,
                underruns: 0,
                track: TimingTrack::default(),
                fail_write_after: None,
                panic_after_ns: None,
                writes: 0,
            }
        }

        fn run_callbacks_until(&mut self, t: u64) {
            while self.next_cb <= t {
                let cb = self.next_cb;
                let n = (self.queue.len() as u64).min(self.period) as u32;
                let e = TimingEvent {
                    first: self.consumed,
                    n,
                    frames: self.period as u32,
                    cb_ns: cb,
                    play_ns: cb + self.latency_ns,
                };
                self.track.add(e);
                if n > 0 {
                    let nonzero = self.queue.iter().take(n as usize).any(|&v| v != 0.0);
                    self.queue.drain(..n as usize);
                    self.world.lock().unwrap().log.push(Ev::Played { first: e.first, n, play_ns: e.play_ns, nonzero });
                    self.consumed += n as u64;
                }
                if (n as u64) < self.period && self.consumed < self.expected_end {
                    self.underruns += 1;
                }
                self.next_cb += self.period * NS / SAMPLE_RATE as u64;
            }
        }
    }

    impl OutputPort for MockPort {
        fn write(&mut self, samples: &[f32]) -> Result<usize, AudioError> {
            self.writes += 1;
            if self.fail_write_after.is_some_and(|k| self.writes > k) {
                return Err(AudioError::StreamDead);
            }
            let n = (self.cap - self.queue.len()).min(samples.len());
            self.queue.extend(&samples[..n]);
            self.written += n as u64;
            Ok(n)
        }
        fn written(&self) -> u64 {
            self.written
        }
        fn consumed(&self) -> u64 {
            self.consumed
        }
        fn expect_until(&mut self, end: u64) {
            self.expected_end = end;
        }
        fn play_time_ns(&mut self, index: u64) -> Option<u64> {
            self.track.play_time_ns(index)
        }
        fn next_start_ns(&mut self) -> Option<u64> {
            if !self.queue.is_empty() {
                return None;
            }
            let now = self.now_ns();
            self.track.next_start_ns(now)
        }
        fn latency_ns(&self) -> u64 {
            self.latency_ns
        }
        fn now_ns(&self) -> u64 {
            self.world.lock().unwrap().now
        }
        fn sleep_ns(&mut self, ns: u64) {
            let t = {
                let mut w = self.world.lock().unwrap();
                w.now += ns.max(1);
                w.now
            };
            if self.panic_after_ns.is_some_and(|p| t > p) {
                panic!("injected panic in the audio path");
            }
            self.run_callbacks_until(t);
        }
        fn stats(&self) -> OutputStats {
            OutputStats { underruns: self.underruns, latency_ns: self.latency_ns, ..Default::default() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::*;
    use super::*;
    use std::sync::{Arc, Mutex};

    fn setup(cap: usize) -> (Shared, MockPort, SharedPtt, Arc<Mutex<bool>>) {
        let world: Shared = Arc::new(Mutex::new(World::default()));
        let line = Arc::new(Mutex::new(false));
        let port = MockPort::new(world.clone(), 480, cap, 20_000_000);
        let ptt = SharedPtt::new(Box::new(MockPtt(world.clone(), line.clone())));
        (world, port, ptt, line)
    }

    #[test]
    fn order_and_timing() {
        // Ring smaller than the audio, so the feeder has to keep up.
        let (world, mut port, ptt, line) = setup(4800);
        let audio = vec![0.5f32; 48_000]; // 1 s
        let timing = TxTiming { txdelay_s: 0.150, txtail_s: 0.010 };
        let stop = AtomicBool::new(false);
        let r = transmit(&mut port, &ptt, &audio, &timing, &stop).unwrap();
        assert!(!*line.lock().unwrap(), "unkeyed after");
        let log = world.lock().unwrap().log.clone();
        // Key first, unkey last, exactly once each.
        assert_eq!(log.iter().filter(|e| matches!(e, Ev::Key(_))).count(), 1);
        assert!(matches!(log[0], Ev::Key(_)));
        assert!(matches!(log.last().unwrap(), Ev::Unkey(_)));
        let (Ev::Key(k), Ev::Unkey(u)) = (log[0].clone(), log.last().unwrap().clone()) else { unreachable!() };
        // Every audio sample plays while keyed.
        let audio_plays: Vec<(u64, u64)> = log
            .iter()
            .filter_map(|e| match e {
                Ev::Played { n, play_ns, nonzero: true, .. } => Some((*play_ns, play_ns + samples_ns(*n as u64))),
                _ => None,
            })
            .collect();
        let first = audio_plays.first().unwrap().0;
        let last_end = audio_plays.last().unwrap().1;
        assert!(first >= k && last_end <= u);
        // TXDelay to within a sample.
        let d = (first - k) as f64 / 1e6;
        assert!((149.97..=150.03).contains(&d), "txdelay {d} ms");
        assert!((r.txdelay_ms() - d).abs() < 0.05, "reported {} measured {d}", r.txdelay_ms());
        // TXTail measured from the end of the last sample at the DAC.
        let tail = (u - last_end) as f64 / 1e6;
        assert!((9.99..=12.5).contains(&tail), "txtail {tail} ms");
        assert!((r.end_play_ns as i64 - last_end as i64).abs() < 30_000);
        assert_eq!(r.underruns, 0);
        assert!((r.keyed_ms() - 1160.0).abs() < 15.0, "keyed {}", r.keyed_ms());
    }

    #[test]
    fn releases_on_audio_error() {
        let (world, mut port, ptt, line) = setup(4800);
        port.fail_write_after = Some(3);
        let audio = vec![0.5f32; 48_000];
        let r = transmit(&mut port, &ptt, &audio, &TxTiming::default(), &AtomicBool::new(false));
        assert!(matches!(r, Err(TxError::Audio(_))));
        assert!(!*line.lock().unwrap());
        assert!(matches!(world.lock().unwrap().log.last().unwrap(), Ev::Unkey(_)));
    }

    /// A PTT whose interface "drops out" (Broken pipe): dead from the start,
    /// or, with `die_when_keyed`, the moment it is keyed.
    struct DroppingPtt {
        dead: bool,
        die_when_keyed: bool,
        line: Arc<Mutex<bool>>,
    }

    impl cairn_ptt::Ptt for DroppingPtt {
        fn set(&mut self, keyed: bool) -> std::io::Result<()> {
            if self.dead {
                return Err(std::io::Error::from_raw_os_error(32));
            }
            *self.line.lock().unwrap() = keyed;
            self.dead = keyed && self.die_when_keyed;
            Ok(())
        }
        fn describe(&self) -> String {
            "dropping".into()
        }
    }

    /// A PTT that is dead, or dies when keyed; `replugged` says whether
    /// reopening finds a healthy device.
    fn dropping(dead: bool, die_when_keyed: bool, replugged: bool) -> (SharedPtt, Arc<Mutex<bool>>) {
        let line = Arc::new(Mutex::new(false));
        let l2 = line.clone();
        let ptt = SharedPtt::with_reopen(
            Box::new(DroppingPtt { dead, die_when_keyed, line: line.clone() }),
            Box::new(move || {
                if replugged {
                    Ok(Box::new(DroppingPtt { dead: false, die_when_keyed: false, line: l2.clone() }))
                } else {
                    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))
                }
            }),
        );
        ptt.set_retry_pause(std::time::Duration::from_millis(1));
        (ptt, line)
    }

    #[test]
    fn dead_ptt_sends_no_audio_warns_and_refuses_to_key_until_reopened() {
        let (_world, mut port, _ptt, _line) = setup(4800);
        let (ptt, line) = dropping(true, false, false);
        let r = transmit(&mut port, &ptt, &[0.5f32; 48_000], &TxTiming::default(), &AtomicBool::new(false));
        assert!(matches!(r, Err(TxError::Ptt(_))), "{r:?}");
        assert_eq!(port.written(), 0, "no audio after a PTT failure");
        assert!(ptt.is_faulted());
        let w = ptt.take_warnings();
        assert!(w.len() == 1 && w[0].contains("MAY STILL BE KEYED"), "{w:?}");
        assert!(failure_text(&r.unwrap_err(), true).contains("MAY STILL BE KEYED"));
        // Refused while the device stays lost.
        let r = transmit(&mut port, &ptt, &[0.5f32; 4800], &TxTiming::default(), &AtomicBool::new(false));
        assert!(matches!(&r, Err(TxError::Ptt(e)) if e.to_string().contains("not keying")), "{r:?}");
        assert_eq!(port.written(), 0);
        assert!(!*line.lock().unwrap());
    }

    #[test]
    fn audio_error_with_a_dying_ptt_releases_by_reopening() {
        // The incident: the interface drops out as the frame starts and the
        // audio device errors mid-frame. The release fails, the device is
        // reopened idle, a warning is raised, and keying works again.
        let (_world, mut port, _ptt, _line) = setup(4800);
        let (ptt, line) = dropping(false, true, true);
        port.fail_write_after = Some(3);
        let r = transmit(&mut port, &ptt, &[0.5f32; 48_000], &TxTiming::default(), &AtomicBool::new(false));
        assert!(matches!(r, Err(TxError::Audio(_))), "{r:?}");
        assert!(!*line.lock().unwrap(), "line idle after the reopen");
        assert!(!ptt.is_faulted());
        let w = ptt.take_warnings();
        assert!(w.len() == 1 && w[0].contains("MAY HAVE BEEN LEFT KEYED"), "{w:?}");
        let r = transmit(&mut port, &ptt, &[0.5f32; 480], &TxTiming::default(), &AtomicBool::new(false));
        assert!(!matches!(r, Err(TxError::Ptt(_))), "keys again once reopened: {r:?}");
    }

    #[test]
    fn releases_on_panic() {
        let (world, mut port, ptt, line) = setup(4800);
        port.panic_after_ns = Some(300_000_000);
        let audio = vec![0.5f32; 48_000];
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = transmit(&mut port, &ptt, &audio, &TxTiming::default(), &AtomicBool::new(false));
        }));
        assert!(res.is_err());
        assert!(!*line.lock().unwrap(), "released while unwinding");
        assert!(matches!(world.lock().unwrap().log.last().unwrap(), Ev::Unkey(_)));
    }

    #[test]
    fn releases_on_stop() {
        let (_world, mut port, ptt, line) = setup(4800);
        let stop = AtomicBool::new(true);
        let r = transmit(&mut port, &ptt, &[0.5f32; 4800], &TxTiming::default(), &stop);
        assert!(matches!(r, Err(TxError::Aborted)));
        assert!(!*line.lock().unwrap());
    }

    #[test]
    fn counts_underruns_when_fed_late() {
        // A ring of 100 samples and a 2 ms feed poll cannot keep a 480-sample callback full.
        let (_world, mut port, ptt, line) = setup(100);
        let r = transmit(&mut port, &ptt, &[0.5f32; 9600], &TxTiming::default(), &AtomicBool::new(false)).unwrap();
        assert!(r.underruns > 0);
        assert!(!*line.lock().unwrap());
    }
}
