//! Real-time scheduling for the audio and processing threads (Linux).
//!
//! An audio thread that wakes late loses samples: with a loaded desktop
//! (parallel compiles) an ordinary thread can wait tens of milliseconds for
//! a core, longer than the device's buffer. Threads therefore ask for a
//! real-time policy, in this order, and never need root:
//!
//! 1. Directly (`sched_setscheduler`, with `SCHED_RESET_ON_FORK`) when the
//!    process may: running as root or with an `RLIMIT_RTPRIO` limit, e.g.
//!    `@audio - rtprio 95` in `/etc/security/limits.d/`. Audio threads get
//!    `SCHED_FIFO` at the configured priority, processing threads (detection,
//!    decoding, the event loop, transmit) `SCHED_RR` 20 below it, both
//!    capped at the limit.
//! 2. Through rtkit (`org.freedesktop.RealtimeKit1` on the system bus, the
//!    daemon desktop distributions run for PipeWire), called with `busctl`:
//!    audio threads get a real-time priority up to rtkit's maximum (20 by
//!    default; the process's `RLIMIT_RTTIME` is lowered to rtkit's limit
//!    first, as rtkit requires), processing threads a high priority (nice
//!    -10, rtkit's `MakeThreadHighPriority`), which keeps the
//!    `RLIMIT_RTTIME` watchdog away from the CPU-heavy decoder.
//! 3. Failing both, a lower nice value if `RLIMIT_NICE` allows it, else
//!    nothing: the thread runs as before and the caller prints one warning.
//!
//! Each attempt reads the policy back, so what is reported is what the
//! kernel applied.

use std::fmt;

/// What a thread does, which decides the policy it asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    /// Moves samples between the device and a ring (wakes every period,
    /// works for microseconds).
    Audio,
    /// Detection, decoding, the event loop, transmit sequencing: must keep
    /// up, may compute for tens of milliseconds at a time.
    Processing,
}

/// Real-time settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtConfig {
    /// Ask for real-time priority at all.
    pub enabled: bool,
    /// `SCHED_FIFO` priority for audio threads (1-99); processing threads
    /// ask for 20 below it.
    pub audio_priority: u8,
}

impl Default for RtConfig {
    fn default() -> Self {
        RtConfig { enabled: true, audio_priority: 60 }
    }
}

impl RtConfig {
    /// Real-time priority asked for `role`.
    pub fn priority(&self, role: ThreadRole) -> u8 {
        match role {
            ThreadRole::Audio => self.audio_priority.clamp(1, 99),
            ThreadRole::Processing => self.audio_priority.clamp(1, 99).saturating_sub(20).max(1),
        }
    }
}

/// How the policy was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// `sched_setscheduler` / `setpriority` by the process itself.
    Direct,
    /// rtkit over D-Bus.
    Rtkit,
}

/// What a thread got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtGrant {
    /// A real-time policy (`SCHED_FIFO` or `SCHED_RR`) at this priority.
    Realtime {
        /// `SCHED_FIFO` (else `SCHED_RR`).
        fifo: bool,
        /// Priority, 1-99.
        priority: u8,
        /// How.
        via: Via,
    },
    /// The normal policy at a raised priority (lower nice value).
    Nice {
        /// The nice value.
        nice: i32,
        /// How.
        via: Via,
    },
    /// Nothing changed, and why.
    Denied(String),
}

impl RtGrant {
    /// A real-time policy was granted.
    pub fn is_realtime(&self) -> bool {
        matches!(self, RtGrant::Realtime { .. })
    }

    /// Anything better than the default was granted.
    pub fn granted(&self) -> bool {
        !matches!(self, RtGrant::Denied(_))
    }
}

impl fmt::Display for RtGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let via = |v: &Via| match v {
            Via::Direct => "",
            Via::Rtkit => " via rtkit",
        };
        match self {
            RtGrant::Realtime { fifo, priority, via: v } => {
                write!(f, "{} {priority}{}", if *fifo { "SCHED_FIFO" } else { "SCHED_RR" }, via(v))
            }
            RtGrant::Nice { nice, via: v } => write!(f, "nice {nice}{}", via(v)),
            RtGrant::Denied(why) => write!(f, "normal priority ({why})"),
        }
    }
}

/// The one-line warning for a denied audio-thread request.
pub fn warning(what: &str, grant: &RtGrant) -> Option<String> {
    match grant {
        RtGrant::Denied(why) if why != DISABLED => Some(format!(
            "warning: {what} runs without real-time priority ({why}); under load it may lose audio \
(docs/bench-testing.md, \"Real-time priority\")"
        )),
        _ => None,
    }
}

const DISABLED: &str = "disabled";

/// The kernel id of the calling thread (0 where unknown).
pub fn current_tid() -> i32 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid takes no arguments and cannot fail.
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Ask for a better policy for the calling thread.
pub fn promote_current(role: ThreadRole, cfg: &RtConfig) -> RtGrant {
    promote_thread(current_tid(), role, cfg)
}

/// Ask for a better policy for thread `tid` of this process.
pub fn promote_thread(tid: i32, role: ThreadRole, cfg: &RtConfig) -> RtGrant {
    if !cfg.enabled {
        return RtGrant::Denied(DISABLED.into());
    }
    imp::promote(tid, role, cfg.priority(role))
}

/// The calling thread's current policy, for reports.
pub fn current_policy() -> RtGrant {
    imp::read_back(current_tid(), Via::Direct)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{RtGrant, ThreadRole, Via};
    use std::process::Command;
    use std::sync::OnceLock;

    const SCHED_RESET_ON_FORK: libc::c_int = 0x4000_0000;
    const PROCESSING_NICE: i32 = -10;

    #[cfg(target_env = "gnu")]
    type Resource = libc::__rlimit_resource_t;
    #[cfg(not(target_env = "gnu"))]
    type Resource = libc::c_int;

    fn rlimit(res: Resource) -> libc::rlimit {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: getrlimit writes one rlimit into a valid struct.
        unsafe { libc::getrlimit(res, &mut r) };
        r
    }

    fn set_rt(tid: i32, fifo: bool, prio: u8) -> Result<(), String> {
        let param = libc::sched_param { sched_priority: prio as libc::c_int };
        let policy = if fifo { libc::SCHED_FIFO } else { libc::SCHED_RR } | SCHED_RESET_ON_FORK;
        // SAFETY: a valid sched_param; tid names a thread of this process.
        let r = unsafe { libc::sched_setscheduler(tid, policy, &param) };
        if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
    }

    fn set_nice(tid: i32, nice: i32) -> Result<(), String> {
        // SAFETY: setpriority on one thread id (Linux: PRIO_PROCESS takes a tid).
        let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice) };
        if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
    }

    pub fn read_back(tid: i32, via: Via) -> RtGrant {
        // SAFETY: plain queries on a thread id.
        let policy = unsafe { libc::sched_getscheduler(tid) } & !SCHED_RESET_ON_FORK;
        let mut param = libc::sched_param { sched_priority: 0 };
        // SAFETY: sched_getparam writes one sched_param.
        unsafe { libc::sched_getparam(tid, &mut param) };
        if policy == libc::SCHED_FIFO || policy == libc::SCHED_RR {
            return RtGrant::Realtime {
                fifo: policy == libc::SCHED_FIFO,
                priority: param.sched_priority.clamp(0, 99) as u8,
                via,
            };
        }
        // SAFETY: getpriority on a thread id; errno is not needed here.
        let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) };
        if nice < 0 { RtGrant::Nice { nice, via } } else { RtGrant::Denied("normal policy".into()) }
    }

    /// rtkit's limits, or why it is not usable (asked once per process).
    struct Rtkit {
        max_rt: i64,
        min_nice: i64,
        rttime_max_us: i64,
    }

    fn busctl(args: &[&str]) -> Result<String, String> {
        // `--` before the arguments: a nice value is negative.
        let out = Command::new("busctl")
            .arg("--system")
            .arg(args[0])
            .arg("--")
            .args(&args[1..])
            .output()
            .map_err(|e| format!("busctl: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    const DEST: [&str; 3] =
        ["org.freedesktop.RealtimeKit1", "/org/freedesktop/RealtimeKit1", "org.freedesktop.RealtimeKit1"];

    fn property(name: &str) -> Result<i64, String> {
        let s = busctl(&["get-property", DEST[0], DEST[1], DEST[2], name])?;
        // "i 20" / "x 200000"
        s.split_whitespace().nth(1).and_then(|v| v.parse().ok()).ok_or_else(|| format!("rtkit {name}: {s:?}"))
    }

    fn rtkit() -> &'static Result<Rtkit, String> {
        static RTKIT: OnceLock<Result<Rtkit, String>> = OnceLock::new();
        RTKIT.get_or_init(|| {
            Ok(Rtkit {
                max_rt: property("MaxRealtimePriority")?,
                min_nice: property("MinNiceLevel")?,
                rttime_max_us: property("RTTimeUSecMax")?,
            })
        })
    }

    fn rtkit_call(method: &str, tid: i32, value: &str) -> Result<(), String> {
        let pid = std::process::id().to_string();
        let tid = tid.to_string();
        let sig = if method.ends_with("RealtimeWithPID") { "ttu" } else { "tti" };
        busctl(&["call", DEST[0], DEST[1], DEST[2], method, sig, &pid, &tid, value]).map(|_| ())
    }

    /// rtkit only serves a process whose hard RLIMIT_RTTIME is at most its
    /// limit; a real-time thread busy that long without sleeping then gets
    /// SIGXCPU (audio threads sleep every period).
    fn limit_rttime(max_us: i64) -> Result<(), String> {
        let cur = rlimit(libc::RLIMIT_RTTIME);
        let max = max_us.max(1) as libc::rlim_t;
        if cur.rlim_max != libc::RLIM_INFINITY && cur.rlim_max <= max {
            return Ok(());
        }
        let r = libc::rlimit { rlim_cur: max, rlim_max: max };
        // SAFETY: lowering a limit of this process.
        if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &r) } == 0 {
            Ok(())
        } else {
            Err(format!("setrlimit RTTIME: {}", std::io::Error::last_os_error()))
        }
    }

    pub fn promote(tid: i32, role: ThreadRole, prio: u8) -> RtGrant {
        let fifo = role == ThreadRole::Audio;
        // 1. Directly.
        // SAFETY: geteuid cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        let lim = rlimit(libc::RLIMIT_RTPRIO).rlim_cur;
        let allowed = if root { 99 } else { lim.min(99) as u8 };
        let direct_err = if allowed >= 1 {
            match set_rt(tid, fifo, prio.min(allowed)) {
                Ok(()) => return read_back(tid, Via::Direct),
                Err(e) => e,
            }
        } else {
            "RLIMIT_RTPRIO is 0".to_string()
        };
        // 2. rtkit.
        let rtkit_err = match rtkit() {
            Ok(rk) => {
                let r = if fifo {
                    let p = (prio as i64).min(rk.max_rt).max(1);
                    limit_rttime(rk.rttime_max_us)
                        .and_then(|_| rtkit_call("MakeThreadRealtimeWithPID", tid, &p.to_string()))
                } else {
                    let n = (PROCESSING_NICE as i64).max(rk.min_nice);
                    rtkit_call("MakeThreadHighPriorityWithPID", tid, &n.to_string())
                };
                match r {
                    Ok(()) => return read_back(tid, Via::Rtkit),
                    Err(e) => format!("rtkit: {e}"),
                }
            }
            Err(e) => format!("no rtkit ({e})"),
        };
        // 3. A lower nice value, if RLIMIT_NICE allows (limit 20 - nice).
        let nice_lim = 20 - rlimit(libc::RLIMIT_NICE).rlim_cur.min(40) as i32;
        let want = if fifo { -15 } else { PROCESSING_NICE };
        if root || nice_lim < 0 {
            let n = if root { want } else { want.max(nice_lim) };
            if set_nice(tid, n).is_ok() {
                if let g @ RtGrant::Nice { .. } = read_back(tid, Via::Direct) {
                    return g;
                }
            }
        }
        RtGrant::Denied(format!("{direct_err}; {rtkit_err}"))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{RtGrant, ThreadRole, Via};

    pub fn promote(_tid: i32, _role: ThreadRole, _prio: u8) -> RtGrant {
        RtGrant::Denied("real-time priority is implemented for Linux only".into())
    }

    pub fn read_back(_tid: i32, _via: Via) -> RtGrant {
        RtGrant::Denied("unknown".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_and_text() {
        let c = RtConfig::default();
        assert_eq!(c.priority(ThreadRole::Audio), 60);
        assert_eq!(c.priority(ThreadRole::Processing), 40);
        let low = RtConfig { enabled: true, audio_priority: 10 };
        assert_eq!(low.priority(ThreadRole::Processing), 1);
        let g = RtGrant::Realtime { fifo: true, priority: 20, via: Via::Rtkit };
        assert_eq!(g.to_string(), "SCHED_FIFO 20 via rtkit");
        assert!(g.is_realtime());
        assert_eq!(RtGrant::Nice { nice: -10, via: Via::Direct }.to_string(), "nice -10");
        assert!(warning("x", &RtGrant::Denied("disabled".into())).is_none());
        assert!(warning("capture", &RtGrant::Denied("EPERM".into())).unwrap().contains("capture"));
    }

    #[test]
    fn disabled_does_nothing() {
        let g = promote_current(ThreadRole::Audio, &RtConfig { enabled: false, audio_priority: 60 });
        assert_eq!(g, RtGrant::Denied("disabled".into()));
    }

    #[test]
    fn promotion_never_panics() {
        // Granted or refused depending on the machine; must not panic. In a
        // thread of its own so the test runner's threads keep their policy.
        std::thread::spawn(|| {
            let g = promote_current(ThreadRole::Processing, &RtConfig::default());
            let _ = g.to_string();
        })
        .join()
        .unwrap();
    }
}
