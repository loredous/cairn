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

/// A PTT shared between the transmitter and an emergency release path
/// (signal handler thread). Locks are held only for one key/unkey call.
#[derive(Clone)]
pub struct SharedPtt(Arc<Mutex<Box<dyn Ptt>>>);

impl SharedPtt {
    /// Share `ptt`.
    pub fn new(ptt: Box<dyn Ptt>) -> Self {
        SharedPtt(Arc::new(Mutex::new(ptt)))
    }

    fn lock(&self) -> MutexGuard<'_, Box<dyn Ptt>> {
        // A panic while holding the lock must not stop a later release.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Key or unkey.
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
                let _ = p.set(false);
                true
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                let _ = e.into_inner().set(false);
                true
            }
            Err(std::sync::TryLockError::WouldBlock) => false,
        }
    }

    /// Read the line back, where the backend can.
    pub fn read_back(&self) -> io::Result<Option<bool>> {
        self.lock().read_back()
    }

    /// Backend description.
    pub fn describe(&self) -> String {
        self.lock().describe()
    }

    /// True for none and VOX.
    pub fn is_null(&self) -> bool {
        self.lock().is_null()
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
