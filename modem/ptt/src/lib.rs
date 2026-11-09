//! Push-to-talk backends for the Cairn modem (SPEC 13.3, 21.3 step 2).
//!
//! | Backend | Spec string | Notes |
//! | --- | --- | --- |
//! | none | `none` | No keying (receive only, or an external keyer) |
//! | VOX | `vox` | No keying; the radio keys on audio (broadcast mode only, SPEC 13.3) |
//! | Serial RTS/DTR | `serial:PATH` | Classic interfaces and the AIOC (DTR high, RTS low keys) |
//! | CM108/CM119 HID GPIO | `cm108:/dev/hidrawN` | Digirig, DRA-xx, the AIOC's HID interface |
//! | Hamlib rigctld | `rigctld[:HOST[:PORT]]` | CAT keying with `T 1` / `T 0` |
//!
//! Every backend starts unkeyed and unkeys when dropped. [`PttGuard`] keys
//! for a scope and releases on every exit path, panics included;
//! [`SharedPtt`] lets a signal handler release the line from another thread.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

mod cm108;
mod rigctld;
#[cfg(unix)]
mod serial;

pub use cm108::Cm108Ptt;
pub use rigctld::RigctldPtt;
#[cfg(unix)]
pub use serial::{SerialLine, SerialPtt};

/// A transmitter keying line.
pub trait Ptt: Send {
    /// Key (`true`) or unkey (`false`) the transmitter. Idempotent.
    fn set(&mut self, keyed: bool) -> io::Result<()>;

    /// What the backend last reported for the line, where it can read it
    /// back (the serial driver's modem-control state, rigctld's `t`).
    fn read_back(&mut self) -> io::Result<Option<bool>> {
        Ok(None)
    }

    /// Human-readable description, e.g. `serial /dev/ttyACM0 (DTR high, RTS low)`.
    fn describe(&self) -> String;

    /// True when keying does nothing (none and VOX).
    fn is_null(&self) -> bool {
        false
    }
}

/// No keying: `none` (receive only) or `vox` (the radio keys on audio).
#[derive(Debug, Clone)]
pub struct NullPtt {
    vox: bool,
    keyed: bool,
}

impl NullPtt {
    /// `none` or, with `vox`, the VOX label.
    pub fn new(vox: bool) -> Self {
        NullPtt { vox, keyed: false }
    }
}

impl Ptt for NullPtt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        self.keyed = keyed;
        Ok(())
    }
    fn read_back(&mut self) -> io::Result<Option<bool>> {
        Ok(Some(self.keyed))
    }
    fn describe(&self) -> String {
        if self.vox { "VOX (no keying line)".into() } else { "none (no keying line)".into() }
    }
    fn is_null(&self) -> bool {
        true
    }
}

/// Parsed PTT configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PttSpec {
    /// No keying.
    None,
    /// VOX: no keying line.
    Vox,
    /// Serial RTS or DTR. `port` is a path, or a substring matched against
    /// `/dev/serial/by-id/` names (e.g. `All-In-One-Cable`).
    Serial {
        /// Device path or by-id substring.
        port: String,
        /// Line that keys.
        line: SerialLineSpec,
        /// Key with the line low (and idle high).
        invert: bool,
    },
    /// CM108/CM119 HID GPIO through a hidraw node.
    Cm108 {
        /// hidraw device path.
        path: String,
        /// GPIO pin, 1-8 (3 on most interfaces).
        gpio: u8,
    },
    /// Hamlib `rigctld` over TCP.
    Rigctld {
        /// host:port
        addr: String,
    },
}

/// Which serial modem-control line keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialLineSpec {
    /// Data Terminal Ready.
    Dtr,
    /// Request To Send.
    Rts,
}

impl std::str::FromStr for SerialLineSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "dtr" => Ok(SerialLineSpec::Dtr),
            "rts" => Ok(SerialLineSpec::Rts),
            _ => Err(format!("serial PTT line must be dtr or rts, not {s:?}")),
        }
    }
}

/// Default rigctld address.
pub const RIGCTLD_DEFAULT: &str = "127.0.0.1:4532";
/// Default CM108 GPIO pin.
pub const CM108_DEFAULT_GPIO: u8 = 3;

impl PttSpec {
    /// Parse `none`, `vox`, `serial:PATH`, `cm108:PATH`, `rigctld[:HOST[:PORT]]`.
    /// Serial line and polarity, and the CM108 pin, come from the extra
    /// arguments (defaults: DTR, active high, GPIO 3: the AIOC's wiring).
    pub fn parse(spec: &str, line: SerialLineSpec, invert: bool, gpio: u8) -> Result<PttSpec, String> {
        let (kind, rest) = match spec.split_once(':') {
            Some((k, r)) => (k, Some(r)),
            None => (spec, None),
        };
        let need =
            |what: &str| rest.filter(|r| !r.is_empty()).map(str::to_string).ok_or(format!("{what} needs a path"));
        match kind.to_ascii_lowercase().as_str() {
            "none" => Ok(PttSpec::None),
            "vox" => Ok(PttSpec::Vox),
            "serial" => Ok(PttSpec::Serial { port: need("serial PTT")?, line, invert }),
            "cm108" => {
                if !(1..=8).contains(&gpio) {
                    return Err(format!("CM108 GPIO must be 1-8, not {gpio}"));
                }
                Ok(PttSpec::Cm108 { path: need("cm108 PTT")?, gpio })
            }
            "rigctld" | "hamlib" => {
                let addr = match rest {
                    None | Some("") => RIGCTLD_DEFAULT.to_string(),
                    Some(r) if r.contains(':') => r.to_string(),
                    Some(r) => format!("{r}:4532"),
                };
                Ok(PttSpec::Rigctld { addr })
            }
            "gpio" => Err("GPIO PTT is not implemented yet; use serial, cm108 or rigctld".into()),
            _ => Err(format!(
                "unknown PTT method {kind:?}; use none, vox, serial:PATH, cm108:PATH or rigctld[:HOST:PORT]"
            )),
        }
    }

    /// Open the backend. It starts unkeyed.
    pub fn open(&self) -> io::Result<Box<dyn Ptt>> {
        Ok(match self {
            PttSpec::None => Box::new(NullPtt::new(false)),
            PttSpec::Vox => Box::new(NullPtt::new(true)),
            #[cfg(unix)]
            PttSpec::Serial { port, line, invert } => {
                let path = serial::resolve_port(port)?;
                let line = match line {
                    SerialLineSpec::Dtr => SerialLine::Dtr,
                    SerialLineSpec::Rts => SerialLine::Rts,
                };
                Box::new(SerialPtt::open(&path, line, !invert)?)
            }
            #[cfg(not(unix))]
            PttSpec::Serial { .. } => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "serial PTT needs a Unix host"));
            }
            PttSpec::Cm108 { path, gpio } => Box::new(Cm108Ptt::open(path, *gpio)?),
            PttSpec::Rigctld { addr } => Box::new(RigctldPtt::connect(addr)?),
        })
    }
}

impl fmt::Display for PttSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PttSpec::None => write!(f, "none"),
            PttSpec::Vox => write!(f, "vox"),
            PttSpec::Serial { port, line, invert } => {
                write!(f, "serial:{port} ({:?} {})", line, if *invert { "active low" } else { "active high" })
            }
            PttSpec::Cm108 { path, gpio } => write!(f, "cm108:{path} (GPIO {gpio})"),
            PttSpec::Rigctld { addr } => write!(f, "rigctld:{addr}"),
        }
    }
}

/// Opens the PTT backend again (resolving its stable path afresh), used to
/// recover from a dead interface.
pub type Reopen = Box<dyn Fn() -> io::Result<Box<dyn Ptt>> + Send + Sync>;

/// Times a failed key or release is retried (dropping the line) before the
/// device is closed and reopened.
pub const RELEASE_RETRIES: usize = 3;
/// Pause between those retries.
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);

/// Stands in for a backend that was closed after it failed.
struct DeadPtt;

impl Ptt for DeadPtt {
    fn set(&mut self, _keyed: bool) -> io::Result<()> {
        Err(io::Error::other("PTT device is closed"))
    }
    fn describe(&self) -> String {
        "closed after a failure".into()
    }
}

struct Inner {
    ptt: Box<dyn Ptt>,
    reopen: Option<Reopen>,
    /// Why the device is not trusted, until it has been reopened.
    fault: Option<String>,
    /// Operator-facing warnings not yet taken by [`SharedPtt::take_warnings`].
    warnings: Vec<String>,
    pause: std::time::Duration,
}

impl Inner {
    fn warn(&mut self, text: String) {
        eprintln!("WARNING: {text}");
        self.warnings.push(text);
    }

    /// Reopen (or, with no way to reopen, just drop the line again) and
    /// put the idle state on the line. Clears the fault on success.
    fn recover(&mut self) -> io::Result<()> {
        match &self.reopen {
            Some(open) => {
                let mut fresh = open()?;
                // Opening already sets the idle state; make it explicit.
                fresh.set(false)?;
                self.ptt = fresh;
            }
            None => self.ptt.set(false)?,
        }
        self.fault = None;
        Ok(())
    }

    /// A key or release failed: do what can be done for a transmitter that
    /// may still be keyed (retry dropping the line, close and reopen the
    /// device), warn loudly, and refuse to key until it works again.
    fn failed(&mut self, what: &str, err: &io::Error) {
        let desc = self.ptt.describe();
        for _ in 0..RELEASE_RETRIES {
            std::thread::sleep(self.pause);
            if self.ptt.set(false).is_ok() {
                self.warn(format!(
                    "PTT {what} failed on {desc}: {err}. The line was dropped on a retry, but the interface is \
                     misbehaving: check it before relying on it."
                ));
                return;
            }
        }
        // Close the dead device and reopen it by its stable path.
        self.ptt = Box::new(DeadPtt);
        self.fault = Some(format!("{desc}: {err}"));
        match self.recover() {
            Ok(()) => self.warn(format!(
                "PTT {what} failed on {desc}: {err}. The transmitter MAY HAVE BEEN LEFT KEYED. The device was \
                 reopened and set idle; check the interface and the radio now."
            )),
            Err(e) => self.warn(format!(
                "PTT {what} failed on {desc}: {err}. The transmitter MAY STILL BE KEYED and the device could not be \
                 reopened ({e}). Check the interface and the radio now (unplug it if need be). Keying is refused \
                 until the PTT device reopens."
            )),
        }
    }

    fn set(&mut self, keyed: bool) -> io::Result<()> {
        if let Some(why) = self.fault.clone() {
            // Not keying until the device is back; releasing is always tried.
            if let Err(e) = self.recover() {
                return Err(io::Error::other(if keyed {
                    format!("PTT device lost ({why}) and not reopened ({e}); not keying")
                } else {
                    format!("PTT device lost ({why}) and not reopened ({e})")
                }));
            }
            self.warn("PTT device reopened and set idle; check the interface.".into());
        }
        match self.ptt.set(keyed) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.failed(if keyed { "keying" } else { "release" }, &e);
                Err(e)
            }
        }
    }
}

/// A PTT shared between the transmitter and an emergency release path
/// (signal handler thread). Locks are held only for one key/unkey call
/// (and, after a failure, the recovery of the device).
///
/// A failed key or release is not final: the line is dropped again a few
/// times, then the device is closed and reopened by its stable path (when a
/// [`Reopen`] was given), with the idle state set. An operator warning is
/// queued ([`SharedPtt::take_warnings`]) and printed, and until the device
/// reopens, keying is refused.
#[derive(Clone)]
pub struct SharedPtt(Arc<Mutex<Inner>>);

impl SharedPtt {
    /// Share `ptt`, with no way to reopen it.
    pub fn new(ptt: Box<dyn Ptt>) -> Self {
        SharedPtt(Arc::new(Mutex::new(Inner {
            ptt,
            reopen: None,
            fault: None,
            warnings: Vec::new(),
            pause: RETRY_PAUSE,
        })))
    }

    /// Share `ptt`, reopened with `reopen` after a failure.
    pub fn with_reopen(ptt: Box<dyn Ptt>, reopen: Reopen) -> Self {
        let s = Self::new(ptt);
        s.lock().reopen = Some(reopen);
        s
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock must not stop a later release.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Key or unkey. A failure runs the recovery described above and is
    /// still returned. While the device is lost, keying is refused.
    pub fn set(&self, keyed: bool) -> io::Result<()> {
        self.lock().set(keyed)
    }

    /// Unkey, ignoring errors after reporting them on stderr.
    pub fn release(&self) {
        if let Err(e) = self.set(false) {
            eprintln!("warning: PTT release failed: {e}");
        }
    }

    /// Unkey if the lock is free (for panic hooks, where the panicking
    /// thread may already hold it); returns whether it tried.
    pub fn try_release(&self) -> bool {
        match self.0.try_lock() {
            Ok(mut p) => {
                let _ = p.ptt.set(false);
                true
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                let _ = e.into_inner().ptt.set(false);
                true
            }
            Err(std::sync::TryLockError::WouldBlock) => false,
        }
    }

    /// Read the line back, where the backend can.
    pub fn read_back(&self) -> io::Result<Option<bool>> {
        self.lock().ptt.read_back()
    }

    /// Backend description.
    pub fn describe(&self) -> String {
        self.lock().ptt.describe()
    }

    /// True for none and VOX.
    pub fn is_null(&self) -> bool {
        self.lock().ptt.is_null()
    }

    /// True while the device failed and has not been reopened: keying is
    /// refused and the transmitter may still be keyed.
    pub fn is_faulted(&self) -> bool {
        self.lock().fault.is_some()
    }

    /// If the device is lost, try to reopen it now and set it idle. The
    /// result is returned, not queued as a warning: the caller reports it
    /// (the warning queued by the failure itself is left for the
    /// transmitting thread, which reports a transmission's outcome).
    pub fn try_recover(&self) -> Recovery {
        let mut g = self.lock();
        let Some(why) = g.fault.clone() else { return Recovery::Healthy };
        match g.recover() {
            Ok(()) => {
                let text =
                    format!("PTT device is back ({why}): reopened and set idle; check the interface and the radio.");
                eprintln!("WARNING: {text}");
                Recovery::Reopened(text)
            }
            Err(e) => Recovery::StillLost(e.to_string()),
        }
    }

    /// Warnings raised since the last call (a failure and its recovery),
    /// for the operator and the host API.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().warnings)
    }

    /// Shorten the pause between release retries (tests).
    #[doc(hidden)]
    pub fn set_retry_pause(&self, pause: std::time::Duration) {
        self.lock().pause = pause;
    }
}

/// What [`SharedPtt::try_recover`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Nothing was wrong.
    Healthy,
    /// Still gone; the reason the reopen failed.
    StillLost(String),
    /// Reopened and set idle; the operator text for it.
    Reopened(String),
}

/// First wait before reopening a lost device again.
pub const REOPEN_MIN: std::time::Duration = std::time::Duration::from_secs(1);
/// The wait stops doubling here.
pub const REOPEN_MAX: std::time::Duration = std::time::Duration::from_secs(10);

/// Retries reopening a lost PTT device on a timer with backoff (1 s, 2 s,
/// 4 s, 8 s, then every 10 s), so the device is back, and the operator
/// told, soon after it returns rather than at the next transmission.
///
/// Time is passed in, so the owner polls it from a loop that is awake
/// anyway: there is no thread and no sleeping here.
#[derive(Debug)]
pub struct ReopenTimer {
    min: std::time::Duration,
    max: std::time::Duration,
    delay: std::time::Duration,
    due: Option<std::time::Instant>,
}

impl Default for ReopenTimer {
    fn default() -> Self {
        Self::new(REOPEN_MIN, REOPEN_MAX)
    }
}

impl ReopenTimer {
    /// Waits start at `min` and double up to `max`.
    pub fn new(min: std::time::Duration, max: std::time::Duration) -> Self {
        ReopenTimer { min, max, delay: min, due: None }
    }

    /// Call often. When `ptt` is faulted and an attempt is due, reopen it:
    /// returns the operator text once when it came back. Does nothing (and
    /// resets the backoff) while the device is healthy.
    pub fn poll(&mut self, ptt: &SharedPtt, now: std::time::Instant) -> Option<String> {
        if !ptt.is_faulted() {
            self.delay = self.min;
            self.due = None;
            return None;
        }
        let due = *self.due.get_or_insert(now + self.delay);
        if now < due {
            return None;
        }
        match ptt.try_recover() {
            Recovery::Reopened(text) => {
                self.delay = self.min;
                self.due = None;
                Some(text)
            }
            Recovery::Healthy => {
                self.due = None;
                None
            }
            Recovery::StillLost(_) => {
                self.delay = (self.delay * 2).min(self.max);
                self.due = Some(now + self.delay);
                None
            }
        }
    }

    /// When the next attempt is due, if the device is lost.
    pub fn next_due(&self) -> Option<std::time::Instant> {
        self.due
    }
}

/// Keys the transmitter on creation and unkeys it when dropped (normal
/// return, `?` error, or panic unwinding).
pub struct PttGuard {
    ptt: SharedPtt,
    keyed: bool,
}

impl PttGuard {
    /// Key `ptt`; on failure the line is released again and the error returned.
    pub fn key(ptt: &SharedPtt) -> io::Result<Self> {
        let g = PttGuard { ptt: ptt.clone(), keyed: true };
        // If keying fails half way, Drop still releases.
        g.ptt.set(true)?;
        Ok(g)
    }

    /// Unkey now and report the result.
    pub fn release(mut self) -> io::Result<()> {
        self.keyed = false;
        self.ptt.set(false)
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        if self.keyed {
            self.ptt.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Probe(Arc<AtomicBool>, bool);
    impl Ptt for Probe {
        fn set(&mut self, keyed: bool) -> io::Result<()> {
            self.0.store(keyed, Ordering::SeqCst);
            if keyed && self.1 { Err(io::Error::other("fails after keying")) } else { Ok(()) }
        }
        fn describe(&self) -> String {
            "probe".into()
        }
    }

    #[test]
    fn guard_releases_on_drop_error_and_panic() {
        let line = Arc::new(AtomicBool::new(false));
        let ptt = SharedPtt::new(Box::new(Probe(line.clone(), false)));
        {
            let _g = PttGuard::key(&ptt).unwrap();
            assert!(line.load(Ordering::SeqCst));
        }
        assert!(!line.load(Ordering::SeqCst));

        let p2 = ptt.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = PttGuard::key(&p2).unwrap();
            panic!("boom");
        });
        assert!(r.is_err());
        assert!(!line.load(Ordering::SeqCst), "released while unwinding");

        // Keying reports an error after the line went up: still released.
        let bad = SharedPtt::new(Box::new(Probe(line.clone(), true)));
        assert!(PttGuard::key(&bad).is_err());
        assert!(!line.load(Ordering::SeqCst));
    }

    /// A backend that fails every call once `dead` is set, logging the
    /// last state it was told to take.
    struct Flaky {
        dead: Arc<AtomicBool>,
        line: Arc<AtomicBool>,
    }
    impl Ptt for Flaky {
        fn set(&mut self, keyed: bool) -> io::Result<()> {
            if self.dead.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(32)); // Broken pipe
            }
            self.line.store(keyed, Ordering::SeqCst);
            Ok(())
        }
        fn describe(&self) -> String {
            "flaky".into()
        }
    }

    /// A flaky PTT whose reopen works only while `usb_back` is set.
    #[allow(clippy::type_complexity)]
    fn flaky() -> (SharedPtt, Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>, Arc<Mutex<usize>>) {
        let dead = Arc::new(AtomicBool::new(false));
        let line = Arc::new(AtomicBool::new(false));
        let usb_back = Arc::new(AtomicBool::new(false));
        let opens = Arc::new(Mutex::new(0usize));
        let (l2, b2, o2) = (line.clone(), usb_back.clone(), opens.clone());
        let ptt = SharedPtt::with_reopen(
            Box::new(Flaky { dead: dead.clone(), line: line.clone() }),
            Box::new(move || {
                *o2.lock().unwrap() += 1;
                if b2.load(Ordering::SeqCst) {
                    Ok(Box::new(Flaky { dead: Arc::new(AtomicBool::new(false)), line: l2.clone() }) as Box<dyn Ptt>)
                } else {
                    Err(io::Error::new(io::ErrorKind::NotFound, "no such device"))
                }
            }),
        );
        ptt.set_retry_pause(std::time::Duration::from_millis(1));
        (ptt, dead, line, usb_back, opens)
    }

    #[test]
    fn dead_device_is_warned_about_and_keying_refused_until_reopened() {
        let (ptt, dead, line, usb_back, opens) = flaky();
        ptt.set(true).unwrap();
        assert!(line.load(Ordering::SeqCst));
        // The interface dies while keyed: the release fails, retries fail,
        // the reopen fails: loud warning, device marked lost.
        dead.store(true, Ordering::SeqCst);
        assert!(ptt.set(false).is_err());
        assert!(ptt.is_faulted());
        assert_eq!(*opens.lock().unwrap(), 1);
        let w = ptt.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("MAY STILL BE KEYED") && w[0].contains("Check the interface"), "{}", w[0]);
        assert!(ptt.take_warnings().is_empty(), "taken once");
        // Keying is refused (and the reopen retried) while it stays gone.
        let e = ptt.set(true).unwrap_err();
        assert!(e.to_string().contains("not keying"), "{e}");
        assert_eq!(*opens.lock().unwrap(), 2);
        assert!(line.load(Ordering::SeqCst), "the line was never raised again");
        // The interface comes back: reopened, idle, keying works again.
        usb_back.store(true, Ordering::SeqCst);
        ptt.set(true).unwrap();
        assert!(!ptt.is_faulted());
        assert!(line.load(Ordering::SeqCst));
        ptt.set(false).unwrap();
        assert!(!line.load(Ordering::SeqCst));
        assert!(ptt.take_warnings().iter().any(|w| w.contains("reopened")));
    }

    #[test]
    fn failed_key_reopens_at_once_when_the_device_is_back() {
        let (ptt, dead, line, usb_back, _) = flaky();
        usb_back.store(true, Ordering::SeqCst);
        dead.store(true, Ordering::SeqCst);
        assert!(PttGuard::key(&ptt).is_err());
        // Reopened and set idle right away; the next key is allowed.
        assert!(!ptt.is_faulted());
        assert!(!line.load(Ordering::SeqCst));
        let w = ptt.take_warnings();
        assert!(w[0].contains("MAY HAVE BEEN LEFT KEYED"), "{w:?}");
        ptt.set(true).unwrap();
        assert!(line.load(Ordering::SeqCst));
    }

    #[test]
    fn fault_during_key_refuses_keying_and_never_raises_the_line() {
        let (ptt, dead, line, _usb_back, opens) = flaky();
        dead.store(true, Ordering::SeqCst);
        let e = ptt.set(true).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(32));
        assert!(ptt.is_faulted());
        assert!(!line.load(Ordering::SeqCst), "the line never went up");
        let w = ptt.take_warnings();
        assert!(w[0].contains("keying failed") && w[0].contains("MAY STILL BE KEYED"), "{w:?}");
        // Another try is refused without touching the dead device.
        assert!(ptt.set(true).unwrap_err().to_string().contains("not keying"));
        assert!(!line.load(Ordering::SeqCst));
        assert_eq!(*opens.lock().unwrap(), 2, "one reopen after the failure, one per refused key");
    }

    #[test]
    fn fault_during_release_warns_and_the_device_is_recovered_when_it_returns() {
        let (ptt, dead, line, usb_back, _) = flaky();
        let g = PttGuard::key(&ptt).unwrap();
        assert!(line.load(Ordering::SeqCst));
        dead.store(true, Ordering::SeqCst);
        let e = g.release().unwrap_err();
        assert_eq!(e.raw_os_error(), Some(32));
        assert!(ptt.is_faulted());
        assert!(line.load(Ordering::SeqCst), "a dead interface stays keyed: nothing here can change that");
        let w = ptt.take_warnings();
        assert!(w[0].contains("release failed") && w[0].contains("MAY STILL BE KEYED"), "{w:?}");
        usb_back.store(true, Ordering::SeqCst);
        assert!(matches!(ptt.try_recover(), Recovery::Reopened(_)));
        assert!(!line.load(Ordering::SeqCst), "reopening puts the idle state on the line");
    }

    #[test]
    fn try_recover_reports_without_taking_the_failure_warning() {
        let (ptt, dead, _line, usb_back, _) = flaky();
        assert_eq!(ptt.try_recover(), Recovery::Healthy);
        dead.store(true, Ordering::SeqCst);
        assert!(ptt.set(true).is_err());
        assert!(matches!(ptt.try_recover(), Recovery::StillLost(_)));
        usb_back.store(true, Ordering::SeqCst);
        let r = ptt.try_recover();
        assert!(matches!(&r, Recovery::Reopened(t) if t.contains("back")), "{r:?}");
        assert!(!ptt.is_faulted());
        // The failure's own warning is still queued for the transmitting thread.
        let w = ptt.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("MAY STILL BE KEYED"));
        ptt.set(true).unwrap();
    }

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    #[test]
    fn reopen_timer_backs_off_then_reports_the_device_coming_back() {
        let (ptt, dead, _line, usb_back, opens) = flaky();
        let mut timer = ReopenTimer::default();
        let t0 = std::time::Instant::now();
        assert_eq!(timer.poll(&ptt, t0), None);
        assert!(timer.next_due().is_none(), "nothing to do while healthy");
        dead.store(true, Ordering::SeqCst);
        assert!(ptt.set(true).is_err());
        assert_eq!(*opens.lock().unwrap(), 1);
        // Attempts at +1 s, +3 s, +7 s, +15 s, +25 s, +35 s: delays 1, 2, 4, 8, 10, 10.
        assert_eq!(timer.poll(&ptt, t0), None, "arms the timer");
        let mut at = ms(0);
        for (i, wait) in [1000u64, 2000, 4000, 8000, 10000, 10000].into_iter().enumerate() {
            assert_eq!(timer.poll(&ptt, t0 + at + ms(wait - 1)), None, "not yet, step {i}");
            assert_eq!(*opens.lock().unwrap(), 1 + i);
            at += ms(wait);
            assert_eq!(timer.poll(&ptt, t0 + at), None);
            assert_eq!(*opens.lock().unwrap(), 2 + i, "one attempt, step {i}");
        }
        // The device returns: the next attempt reports it, once.
        usb_back.store(true, Ordering::SeqCst);
        let n = *opens.lock().unwrap();
        assert_eq!(timer.poll(&ptt, t0 + at + ms(9999)), None);
        let text = timer.poll(&ptt, t0 + at + ms(10000)).expect("came back");
        assert!(text.contains("back"), "{text}");
        assert_eq!(*opens.lock().unwrap(), n + 1);
        assert!(!ptt.is_faulted());
        assert_eq!(timer.poll(&ptt, t0 + at + ms(60000)), None, "reported once, idle afterwards");
        assert_eq!(*opens.lock().unwrap(), n + 1);
        // The next fault starts from the short delay again.
        dead.store(false, Ordering::SeqCst);
        ptt.set(true).unwrap();
    }

    #[test]
    fn reopen_timer_does_not_spin_when_the_device_never_returns() {
        let (ptt, dead, _line, _usb_back, opens) = flaky();
        let mut timer = ReopenTimer::new(ms(10), ms(40));
        dead.store(true, Ordering::SeqCst);
        assert!(ptt.set(true).is_err());
        let t0 = std::time::Instant::now();
        // Polled every millisecond for a simulated minute: attempts follow
        // the schedule 10, 20, 40, 40, ... ms, not the polling rate.
        for i in 0..60_000u64 {
            assert_eq!(timer.poll(&ptt, t0 + ms(i)), None);
        }
        let n = *opens.lock().unwrap();
        assert!((1490..=1520).contains(&n), "about one attempt per 40 ms, got {n}");
        assert!(ptt.is_faulted());
    }

    #[test]
    fn parse_specs() {
        let d = SerialLineSpec::Dtr;
        assert_eq!(PttSpec::parse("none", d, false, 3).unwrap(), PttSpec::None);
        assert_eq!(
            PttSpec::parse("serial:/dev/ttyACM0", SerialLineSpec::Rts, true, 3).unwrap(),
            PttSpec::Serial { port: "/dev/ttyACM0".into(), line: SerialLineSpec::Rts, invert: true }
        );
        assert_eq!(PttSpec::parse("rigctld", d, false, 3).unwrap(), PttSpec::Rigctld { addr: RIGCTLD_DEFAULT.into() });
        assert_eq!(
            PttSpec::parse("rigctld:radio", d, false, 3).unwrap(),
            PttSpec::Rigctld { addr: "radio:4532".into() }
        );
        assert!(PttSpec::parse("serial", d, false, 3).is_err());
        assert!(PttSpec::parse("cm108:/dev/hidraw1", d, false, 9).is_err());
        assert!(PttSpec::parse("gpio:17", d, false, 3).is_err());
    }
}
