//! Unattended operation (SPEC 13.4, 13.6; `run --unattended`).
//!
//! An unattended station answers CONNECTs on its own, so the modem carries
//! the limits a control operator would otherwise apply by hand:
//!
//! - **Who may connect**: allow and deny lists of callsign patterns
//!   ([`CallPattern`]: `CALL` is SSID 0, `CALL-N` one SSID, `CALL-*` every
//!   SSID, `*` everyone), applied by the link layer before any reply.
//! - **How long**: a maximum session duration ([`SessionClock`]) and a daily
//!   keyed-time budget per UTC day ([`TxLedger`]); near the end of the budget
//!   the station stops accepting sessions and closes the open one, then stops
//!   keying altogether.
//! - **Identification**: the ID block in every CONNECT, CONNECT-ACK and
//!   DISCONNECT and at least every 9 minutes in a session (ADR 0010
//!   `session-id-cadence`), and with `id_broadcasts` in UI frames on the same
//!   clock; every header carries the callsign (SPEC 8.3).
//! - **Fail-safe keying**: a [`Watchdog`] thread releases the PTT and stops
//!   the modem when a transmission stays keyed longer than it should (plus a
//!   margin) or when the audio input stalls (a hung event loop or a dead
//!   device stalls it too).
//! - **A record**: every output line to a rotating log file ([`RotatingLog`]).
//! - **Hooks**: a configured program per event ([`Hooks`]), run without a
//!   shell; received content reaches it only through environment variables
//!   and standard input, never its command line.
//!
//! Regulatory background (informational, not legal advice; US Part 97,
//! other administrations differ): a station under *automatic control*
//! (§97.109(d)) transmits without a control operator at the control point,
//! which §97.221 permits for RTTY and data emissions only on certain
//! frequency segments and, outside them, only in reply to a station under
//! local or remote control. The station licensee and the control operator
//! remain responsible for every transmission (§97.103, §97.105) and for
//! identification (§97.119). These limits make an unattended station
//! predictable and quick to stop; they do not decide where automatic control
//! is allowed. See docs/operator-guide.md.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_link::{Address, HostEvent, Instant};
use cairn_ptt::{Ptt, SharedPtt};

use crate::config::{HooksSection, UnattendedSection};
use crate::runtime::Output;
use crate::rxdir::Saved;

// --- Callsign patterns ---------------------------------------------------------------

/// A callsign pattern for the allow and deny lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallPattern {
    /// `*`: every station.
    Everyone,
    /// `CALL` (SSID 0), `CALL-N`, or `CALL-*` (ssid None: any SSID).
    Call {
        /// The callsign, upper case.
        call: String,
        /// The SSID, or None for any.
        ssid: Option<u8>,
    },
}

impl CallPattern {
    /// Parse `*`, `CALL`, `CALL-N` or `CALL-*`.
    pub fn parse(s: &str) -> Result<CallPattern, String> {
        let s = s.trim();
        if s == "*" {
            return Ok(CallPattern::Everyone);
        }
        let (call, ssid) = match s.rsplit_once('-') {
            Some((c, "*")) => (c, None),
            Some((c, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                let n: u8 = n.parse().map_err(|_| format!("bad SSID in {s:?}"))?;
                if n > 15 {
                    return Err(format!("SSID above 15 in {s:?}"));
                }
                (c, Some(n))
            }
            _ => (s, Some(0)),
        };
        if call.contains('*') {
            return Err(format!("{s:?}: only the SSID may be a wildcard (CALL-*), or the whole pattern (*)"));
        }
        // Validate with the address codec (long callsigns are hashed, B.1).
        crate::control::parse_call(call).map_err(|e| format!("pattern {s:?}: {e}"))?;
        Ok(CallPattern::Call { call: call.to_ascii_uppercase(), ssid })
    }

    /// The addresses it stands for (None for everyone).
    pub fn addresses(&self) -> Option<Vec<Address>> {
        match self {
            CallPattern::Everyone => None,
            CallPattern::Call { call, ssid: Some(n) } => {
                Some(vec![crate::control::parse_call(&format!("{call}-{n}")).expect("checked")])
            }
            CallPattern::Call { call, ssid: None } => {
                Some((0..=15u8).map(|n| crate::control::parse_call(&format!("{call}-{n}")).expect("checked")).collect())
            }
        }
    }

    /// True when `a` matches.
    pub fn matches(&self, a: Address) -> bool {
        self.addresses().is_none_or(|v| v.contains(&a))
    }
}

impl std::fmt::Display for CallPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallPattern::Everyone => write!(f, "*"),
            CallPattern::Call { call, ssid: Some(0) } => write!(f, "{call}"),
            CallPattern::Call { call, ssid: Some(n) } => write!(f, "{call}-{n}"),
            CallPattern::Call { call, ssid: None } => write!(f, "{call}-*"),
        }
    }
}

/// Parse a list of patterns.
pub fn parse_patterns(v: &[String]) -> Result<Vec<CallPattern>, String> {
    v.iter().map(|s| CallPattern::parse(s)).collect()
}

/// Apply allow and deny lists to the link layer's configuration. An empty
/// allow list (or one holding `*`) lets anyone not denied connect; `*` in
/// the deny list refuses everyone.
pub fn apply_lists(cfg: &mut cairn_link::Config, allow: &[CallPattern], deny: &[CallPattern]) {
    if !allow.is_empty() && !allow.contains(&CallPattern::Everyone) {
        let mut v: Vec<Address> = allow.iter().flat_map(|p| p.addresses().unwrap_or_default()).collect();
        v.sort();
        v.dedup();
        cfg.allow = Some(v);
    }
    for p in deny {
        match p.addresses() {
            None => cfg.accept_connects = false,
            Some(v) => cfg.deny.extend(v),
        }
    }
}

// --- Policy ------------------------------------------------------------------------------

/// Default maximum session, minutes.
pub const DEFAULT_MAX_SESSION_MIN: f64 = 30.0;
/// Default daily keyed time, minutes.
pub const DEFAULT_DAILY_TX_MIN: f64 = 60.0;
/// Default watchdog margin, s.
pub const DEFAULT_WATCHDOG_MARGIN_S: f64 = 2.0;
/// Default audio stall, s.
pub const DEFAULT_AUDIO_STALL_S: f64 = 5.0;
/// Keyed time kept back at the end of the daily budget for closing a
/// session (DISCONNECT and its resends), s.
pub const BUDGET_RESERVE: Duration = Duration::from_secs(30);

/// The unattended settings, checked.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Accept CONNECTs only from these (empty: anyone not denied).
    pub allow: Vec<CallPattern>,
    /// Refuse CONNECTs from these.
    pub deny: Vec<CallPattern>,
    /// Close a session after this long.
    pub max_session: Option<Duration>,
    /// Keyed time per UTC day.
    pub daily_tx: Option<Duration>,
    /// Where the day's keyed time is kept.
    pub budget_file: Option<PathBuf>,
    /// Hooks.
    pub hooks: Hooks,
}

fn minutes(v: Option<f64>, default: f64, what: &str) -> Result<Option<Duration>, String> {
    let m = v.unwrap_or(default);
    if !(0.0..=24.0 * 60.0).contains(&m) {
        return Err(format!("unattended.{what} must be 0-1440 minutes"));
    }
    Ok((m > 0.0).then(|| Duration::from_secs_f64(m * 60.0)))
}

impl Policy {
    /// From the config's `[unattended]` section.
    pub fn from_section(u: &UnattendedSection) -> Result<Policy, String> {
        Ok(Policy {
            allow: parse_patterns(&u.allow).map_err(|e| format!("unattended.allow: {e}"))?,
            deny: parse_patterns(&u.deny).map_err(|e| format!("unattended.deny: {e}"))?,
            max_session: minutes(u.max_session_min, DEFAULT_MAX_SESSION_MIN, "max_session_min")?,
            daily_tx: minutes(u.daily_tx_min, DEFAULT_DAILY_TX_MIN, "daily_tx_min")?,
            budget_file: u.budget_file.as_deref().map(crate::config::expand_home),
            hooks: Hooks::from_section(&u.hooks)?,
        })
    }

    /// One line for the start of a run.
    pub fn describe(&self) -> String {
        let list = |v: &[CallPattern], none: &str| {
            if v.is_empty() { none.to_string() } else { v.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ") }
        };
        format!(
            "unattended: allow {}; deny {}; sessions up to {}; keyed time up to {} per UTC day; ID block in UI frames on the 9-minute clock; hooks: {}",
            list(&self.allow, "anyone"),
            list(&self.deny, "nobody"),
            self.max_session.map_or("no limit".into(), |d| format!("{:.0} min", d.as_secs_f64() / 60.0)),
            self.daily_tx.map_or("no limit".into(), |d| format!("{:.0} min", d.as_secs_f64() / 60.0)),
            self.hooks.describe()
        )
    }
}

// --- Session duration -------------------------------------------------------------------

/// Tracks the open session's age.
#[derive(Clone, Debug, Default)]
pub struct SessionClock {
    since: Option<(Address, Instant)>,
    asked: bool,
}

impl SessionClock {
    /// Follow the station's host events.
    pub fn on_event(&mut self, e: &HostEvent, now: Instant) {
        match e {
            HostEvent::Connected { peer, resumed, .. } => {
                // A resume continues the same session's clock.
                if !(*resumed && self.since.is_some_and(|(p, _)| p == *peer)) {
                    self.since = Some((*peer, now));
                    self.asked = false;
                }
            }
            HostEvent::Disconnected { .. } | HostEvent::SessionExpired { .. } => {
                self.since = None;
                self.asked = false;
            }
            _ => {}
        }
    }

    /// True once, when the open session has run past `limit`.
    pub fn overdue(&mut self, limit: Option<Duration>, now: Instant) -> Option<Address> {
        let (peer, t0) = self.since?;
        if self.asked || limit.is_none_or(|l| now.saturating_since(t0) < l) {
            return None;
        }
        self.asked = true;
        Some(peer)
    }
}

// --- Daily keyed-time budget ------------------------------------------------------------

/// Where the budget stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Budget {
    /// Plenty left (or no limit).
    Open,
    /// Under [`BUDGET_RESERVE`] left: accept no sessions, close the open one.
    Reserve,
    /// Nothing left: key nothing until the UTC day ends.
    Exhausted,
}

/// Keyed time per UTC day.
#[derive(Clone, Debug)]
pub struct TxLedger {
    limit: Option<Duration>,
    day: i64,
    used: Duration,
    file: Option<PathBuf>,
}

fn utc_day(unix_s: i64) -> i64 {
    unix_s.div_euclid(86_400)
}

impl TxLedger {
    /// A ledger for `limit` per day, continuing the file's count when it is
    /// from the same UTC day.
    pub fn new(limit: Option<Duration>, file: Option<PathBuf>, unix_s: i64) -> TxLedger {
        let day = utc_day(unix_s);
        let used = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(|v| v["utc_day"].as_i64() == Some(day))
            .and_then(|v| v["keyed_s"].as_f64())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map_or(Duration::ZERO, Duration::from_secs_f64);
        TxLedger { limit, day, used, file }
    }

    fn roll(&mut self, unix_s: i64) {
        let d = utc_day(unix_s);
        if d != self.day {
            self.day = d;
            self.used = Duration::ZERO;
        }
    }

    /// Keyed time used today.
    pub fn used(&mut self, unix_s: i64) -> Duration {
        self.roll(unix_s);
        self.used
    }

    /// The state of the budget.
    pub fn state(&mut self, unix_s: i64) -> Budget {
        self.roll(unix_s);
        match self.limit {
            None => Budget::Open,
            Some(l) if self.used >= l => Budget::Exhausted,
            Some(l) if l - self.used < BUDGET_RESERVE.min(l / 4) => Budget::Reserve,
            Some(_) => Budget::Open,
        }
    }

    /// Whether a transmission of `keyed` fits.
    pub fn may_key(&mut self, keyed: Duration, unix_s: i64) -> bool {
        self.roll(unix_s);
        self.limit.is_none_or(|l| self.used + keyed <= l)
    }

    /// Count a transmission (and save the count).
    pub fn record(&mut self, keyed: Duration, unix_s: i64) -> io::Result<()> {
        self.roll(unix_s);
        self.used += keyed;
        if let Some(f) = &self.file {
            let v = serde_json::json!({ "utc_day": self.day, "keyed_s": self.used.as_secs_f64() });
            crate::config::write_with_backup(f, &format!("{v}\n")).map(|_| ())?;
        }
        Ok(())
    }
}

// --- Watchdog -------------------------------------------------------------------------------

/// Shared between the PTT wrapper, the event loop and the watchdog thread.
/// Times are microseconds since `origin`; 0 means "none".
#[derive(Debug)]
pub struct WatchState {
    origin: std::time::Instant,
    keyed_since: AtomicU64,
    limit: AtomicU64,
    audio: AtomicU64,
    done: AtomicBool,
    tripped: Mutex<Option<String>>,
}

impl WatchState {
    /// A fresh state; the audio clock starts now.
    pub fn new(default_limit: Duration) -> Arc<WatchState> {
        let s = WatchState {
            origin: std::time::Instant::now(),
            keyed_since: AtomicU64::new(0),
            limit: AtomicU64::new(default_limit.as_micros() as u64),
            audio: AtomicU64::new(1),
            done: AtomicBool::new(false),
            tripped: Mutex::new(None),
        };
        Arc::new(s)
    }

    fn now_us(&self) -> u64 {
        (self.origin.elapsed().as_micros() as u64).max(1)
    }

    /// The PTT went up or down.
    pub fn keyed(&self, on: bool) {
        if on {
            // Keep the first key time if keyed twice.
            let _ = self.keyed_since.compare_exchange(0, self.now_us(), Ordering::SeqCst, Ordering::SeqCst);
        } else {
            self.keyed_since.store(0, Ordering::SeqCst);
        }
    }

    /// The longest the next keyed transmission may take (its keyed time
    /// plus the margin).
    pub fn expect(&self, limit: Duration) {
        self.limit.store(limit.as_micros() as u64, Ordering::SeqCst);
    }

    /// Audio arrived.
    pub fn audio(&self) {
        self.audio.store(self.now_us(), Ordering::Relaxed);
    }

    /// Why the watchdog tripped, if it did.
    pub fn tripped(&self) -> Option<String> {
        self.tripped.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop the watchdog thread.
    pub fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

/// Watchdog limits.
#[derive(Clone, Copy, Debug)]
pub struct Watchdog {
    /// No audio for this long stops the modem.
    pub audio_stall: Duration,
}

impl Watchdog {
    /// The verdict at `now_us`: why to release and stop, or None.
    pub fn verdict(&self, now_us: u64, keyed_since_us: u64, limit_us: u64, audio_us: u64) -> Option<String> {
        if keyed_since_us != 0 && now_us.saturating_sub(keyed_since_us) > limit_us {
            return Some(format!(
                "watchdog: PTT keyed {:.1} s, longer than the {:.1} s allowed; PTT released, modem stopped",
                (now_us - keyed_since_us) as f64 / 1e6,
                limit_us as f64 / 1e6
            ));
        }
        let stall = now_us.saturating_sub(audio_us);
        if stall > self.audio_stall.as_micros() as u64 {
            return Some(format!(
                "watchdog: no audio for {:.1} s (device or event loop stalled); PTT released, modem stopped",
                stall as f64 / 1e6
            ));
        }
        None
    }

    /// Check `state` now.
    pub fn check(&self, state: &WatchState) -> Option<String> {
        self.verdict(
            state.now_us(),
            state.keyed_since.load(Ordering::SeqCst),
            state.limit.load(Ordering::SeqCst),
            state.audio.load(Ordering::Relaxed),
        )
    }

    /// Run the watchdog on its own thread: every 50 ms it checks `state`;
    /// on a trip it releases `ptt` (from this thread: the event loop may be
    /// the thing that hung), records why and raises `stop`.
    pub fn spawn(
        self,
        state: Arc<WatchState>,
        ptt: SharedPtt,
        stop: &'static AtomicBool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("cairn-watchdog".into())
            .spawn(move || {
                while !state.done.load(Ordering::SeqCst) {
                    if let Some(why) = self.check(&state) {
                        ptt.release();
                        state.keyed(false);
                        eprintln!("{why}");
                        *state.tripped.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                        stop.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
            .expect("spawn the watchdog")
    }
}

/// A PTT that tells the watchdog when it is keyed.
pub struct WatchedPtt {
    inner: Box<dyn Ptt>,
    state: Arc<WatchState>,
}

impl WatchedPtt {
    /// Wrap `inner`.
    pub fn new(inner: Box<dyn Ptt>, state: Arc<WatchState>) -> Self {
        WatchedPtt { inner, state }
    }
}

impl Ptt for WatchedPtt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        let r = self.inner.set(keyed);
        // Count a failed key as keyed (the line may be up); a release as
        // released only when it worked.
        if keyed || r.is_ok() {
            self.state.keyed(keyed);
        }
        r
    }
    fn read_back(&mut self) -> io::Result<Option<bool>> {
        self.inner.read_back()
    }
    fn describe(&self) -> String {
        format!("{} (watchdog)", self.inner.describe())
    }
    fn is_null(&self) -> bool {
        self.inner.is_null()
    }
}

// --- Rotating log -----------------------------------------------------------------------------

/// A log file rotated by size: `PATH`, `PATH.1` (newest) … `PATH.keep`.
pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    file: File,
    size: u64,
}

impl RotatingLog {
    /// Open (append) `path`, creating its directory.
    pub fn open(path: &Path, max_bytes: u64, keep: u32) -> io::Result<RotatingLog> {
        if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(d)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(RotatingLog { path: path.to_path_buf(), max_bytes: max_bytes.max(1024), keep: keep.max(1), file, size })
    }

    fn numbered(&self, n: u32) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let _ = std::fs::remove_file(self.numbered(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            if from.exists() {
                std::fs::rename(&from, self.numbered(n + 1))?;
            }
        }
        std::fs::rename(&self.path, self.numbered(1))?;
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }

    /// Append one line with a UTC time stamp.
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        let l = format!("{} {}\n", cairn_api::proto::utc_string(cairn_api::proto::now_unix_us()), text);
        if self.size > 0 && self.size + l.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(l.as_bytes())?;
        self.size += l.len() as u64;
        Ok(())
    }
}

// --- Hooks ---------------------------------------------------------------------------------------

/// Events a hook can follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// A session came up.
    Connected,
    /// A session ended.
    Disconnected,
    /// A file arrived complete.
    ReceivedFile,
    /// A text message arrived.
    ReceivedMessage,
    /// A CONNECT was refused.
    Refused,
    /// The watchdog or the budget stopped transmitting.
    Alarm,
}

impl HookEvent {
    /// The name in `CAIRN_EVENT` and the config.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::Connected => "connected",
            HookEvent::Disconnected => "disconnected",
            HookEvent::ReceivedFile => "received_file",
            HookEvent::ReceivedMessage => "received_message",
            HookEvent::Refused => "refused",
            HookEvent::Alarm => "alarm",
        }
    }
}

/// One hook invocation: the event, its environment and its input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookCall {
    /// The event.
    pub event: HookEvent,
    /// Extra environment variables (`CAIRN_*`).
    pub env: Vec<(String, String)>,
    /// Standard input (a received message's text).
    pub stdin: Vec<u8>,
}

impl HookCall {
    fn new(event: HookEvent) -> Self {
        HookCall { event, env: Vec::new(), stdin: Vec::new() }
    }

    fn var(mut self, k: &str, v: impl ToString) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }
}

/// Printable ASCII only: an environment value never carries control
/// characters or NUL, whatever arrived over the air.
pub fn sanitize(v: &str) -> String {
    v.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).take(1024).collect()
}

/// The configured hooks.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    commands: Vec<(HookEvent, Vec<String>)>,
    timeout: Duration,
    running: Arc<AtomicUsize>,
}

/// Hooks allowed to run at once; more are skipped (and logged).
pub const MAX_RUNNING_HOOKS: usize = 4;

impl Hooks {
    /// From `[unattended.hooks]`.
    pub fn from_section(h: &HooksSection) -> Result<Hooks, String> {
        let mut commands = Vec::new();
        for (ev, argv) in [
            (HookEvent::Connected, &h.connected),
            (HookEvent::Disconnected, &h.disconnected),
            (HookEvent::ReceivedFile, &h.received_file),
            (HookEvent::ReceivedMessage, &h.received_message),
            (HookEvent::Refused, &h.refused),
            (HookEvent::Alarm, &h.alarm),
        ] {
            if argv.is_empty() {
                continue;
            }
            if argv[0].trim().is_empty() {
                return Err(format!("unattended.hooks.{}: the program is empty", ev.name()));
            }
            let mut argv = argv.clone();
            argv[0] = crate::config::expand_home(Path::new(&argv[0])).to_string_lossy().into_owned();
            commands.push((ev, argv));
        }
        let t = h.timeout_s.unwrap_or(60.0);
        if !(0.1..=3600.0).contains(&t) {
            return Err("unattended.hooks.timeout_s must be 0.1-3600".into());
        }
        Ok(Hooks { commands, timeout: Duration::from_secs_f64(t), running: Arc::new(AtomicUsize::new(0)) })
    }

    /// The events with a hook.
    pub fn describe(&self) -> String {
        if self.commands.is_empty() {
            return "none".into();
        }
        self.commands.iter().map(|(e, _)| e.name()).collect::<Vec<_>>().join(", ")
    }

    /// The configured programs, for the self-test.
    pub fn programs(&self) -> Vec<(HookEvent, String)> {
        self.commands.iter().map(|(e, a)| (*e, a[0].clone())).collect()
    }

    /// The command for `call` (not started): the configured program and
    /// arguments exactly as written, `CAIRN_EVENT` plus the call's variables
    /// (sanitized) in its environment.
    pub fn command(&self, call: &HookCall) -> Option<std::process::Command> {
        let argv = &self.commands.iter().find(|(e, _)| *e == call.event)?.1;
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..]);
        c.env("CAIRN_EVENT", call.event.name());
        c.env("CAIRN_TIME", cairn_api::proto::utc_string(cairn_api::proto::now_unix_us()));
        for (k, v) in &call.env {
            c.env(k, sanitize(v));
        }
        Some(c)
    }

    /// Run the hook for `call` on its own thread (stdin written, then waited
    /// for, killed after the timeout). Returns the outcome line when the
    /// hook finishes, through `report`.
    pub fn fire(&self, call: HookCall, report: impl FnOnce(String) + Send + 'static) -> bool {
        let Some(mut cmd) = self.command(&call) else { return false };
        if self.running.load(Ordering::SeqCst) >= MAX_RUNNING_HOOKS {
            report(format!("hook {}: skipped ({MAX_RUNNING_HOOKS} hooks still running)", call.event.name()));
            return false;
        }
        self.running.fetch_add(1, Ordering::SeqCst);
        let running = self.running.clone();
        let timeout = self.timeout;
        std::thread::spawn(move || {
            let name = call.event.name();
            let r = (|| -> io::Result<String> {
                use std::process::Stdio;
                let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
                if let Some(mut si) = child.stdin.take() {
                    // A hook that does not read its input must not block us.
                    let data = call.stdin.clone();
                    std::thread::spawn(move || {
                        let _ = si.write_all(&data);
                    });
                }
                let t0 = std::time::Instant::now();
                loop {
                    if let Some(st) = child.try_wait()? {
                        return Ok(format!("hook {name}: {st}"));
                    }
                    if t0.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(format!("hook {name}: killed after {:.0} s", timeout.as_secs_f64()));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })();
            running.fetch_sub(1, Ordering::SeqCst);
            report(r.unwrap_or_else(|e| format!("hook {name}: failed to start: {e}")));
        });
        true
    }
}

/// The hook calls an output of the modem (and where `--rx-dir` saved it)
/// stands for.
pub fn hook_calls(o: &Output, saved: Option<&Saved>, me: Address) -> Vec<HookCall> {
    let station = me.to_string();
    let mut v = Vec::new();
    match o {
        Output::Host(HostEvent::Connected { peer, session_id, resumed, initiator }) => v.push(
            HookCall::new(HookEvent::Connected)
                .var("CAIRN_STATION", &station)
                .var("CAIRN_PEER", peer)
                .var("CAIRN_SESSION_ID", session_id)
                .var("CAIRN_RESUMED", resumed)
                .var("CAIRN_INITIATOR", initiator),
        ),
        Output::Host(HostEvent::Disconnected { peer }) => v.push(
            HookCall::new(HookEvent::Disconnected)
                .var("CAIRN_STATION", &station)
                .var("CAIRN_PEER", peer)
                .var("CAIRN_REASON", "disconnect"),
        ),
        Output::Host(HostEvent::LinkLost { peer }) => v.push(
            HookCall::new(HookEvent::Disconnected)
                .var("CAIRN_STATION", &station)
                .var("CAIRN_PEER", peer)
                .var("CAIRN_REASON", "link-lost"),
        ),
        Output::Host(HostEvent::Refused { peer }) => {
            v.push(HookCall::new(HookEvent::Refused).var("CAIRN_STATION", &station).var("CAIRN_PEER", peer))
        }
        _ => {}
    }
    let from = match o {
        Output::Deliver { from, .. } | Output::DeliverBroadcast { from, .. } => Some(*from),
        _ => None,
    };
    if let (Some(s), Some(from)) = (saved, from) {
        if let Some(c) = &s.completed {
            v.push(
                HookCall::new(HookEvent::ReceivedFile)
                    .var("CAIRN_STATION", &station)
                    .var("CAIRN_PEER", c.from)
                    .var("CAIRN_FILE", c.path.display())
                    .var("CAIRN_OBJECT", c.object)
                    .var("CAIRN_BYTES", c.bytes)
                    .var("CAIRN_SHA256", &c.sha256),
            );
        } else if s.message {
            let (body, broadcast) = match o {
                Output::Deliver { pdu, .. } => (pdu.body.clone(), false),
                Output::DeliverBroadcast { pdu, .. } => (pdu.body.clone(), true),
                _ => (Vec::new(), false),
            };
            let mut c = HookCall::new(HookEvent::ReceivedMessage)
                .var("CAIRN_STATION", &station)
                .var("CAIRN_PEER", from)
                .var("CAIRN_FILE", s.path.display())
                .var("CAIRN_BYTES", body.len())
                .var("CAIRN_BROADCAST", broadcast);
            c.stdin = body;
            v.push(c);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_link::Pdu;

    fn a(s: &str) -> Address {
        crate::control::parse_call(s).unwrap()
    }

    #[test]
    fn patterns() {
        let p = |s| CallPattern::parse(s).unwrap();
        assert_eq!(p("n0call"), CallPattern::Call { call: "N0CALL".into(), ssid: Some(0) });
        assert!(p("N0CALL").matches(a("N0CALL")));
        assert!(!p("N0CALL").matches(a("N0CALL-1")), "a bare callsign is SSID 0");
        assert!(p("N0CALL-7").matches(a("N0CALL-7")));
        assert!(!p("N0CALL-7").matches(a("N0CALL-8")));
        for n in 0..=15 {
            assert!(p("N0CALL-*").matches(a(&format!("N0CALL-{n}"))));
        }
        assert!(!p("N0CALL-*").matches(a("N0CALM-1")));
        assert!(p("*").matches(a("W1XYZ-3")));
        // Long callsigns are hashed; patterns hash the same way.
        assert!(p("VE3LONGCAL-*").matches(a("VE3LONGCAL-2")));
        assert_eq!(p("N0CALL-*").to_string(), "N0CALL-*");
        for bad in ["N0*", "N0CALL-16", "", "N0 CALL", "TOOLONGCALLSIGN"] {
            assert!(CallPattern::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lists_reach_the_link_config() {
        let mut c = cairn_link::Config::for_callsign("N0ME", 0);
        let allow = parse_patterns(&["N0AAA-*".into(), "N0BBB-2".into()]).unwrap();
        let deny = parse_patterns(&["N0AAA-5".into()]).unwrap();
        apply_lists(&mut c, &allow, &deny);
        assert_eq!(c.allow.as_ref().unwrap().len(), 17);
        assert_eq!(c.deny, vec![a("N0AAA-5")]);
        assert!(c.accept_connects);
        let mut c = cairn_link::Config::for_callsign("N0ME", 0);
        apply_lists(&mut c, &parse_patterns(&["*".into()]).unwrap(), &parse_patterns(&["*".into()]).unwrap());
        assert_eq!(c.allow, None);
        assert!(!c.accept_connects, "deny * refuses everyone");
    }

    #[test]
    fn policy_defaults_and_limits() {
        let p = Policy::from_section(&UnattendedSection::default()).unwrap();
        assert_eq!(p.max_session, Some(Duration::from_secs(30 * 60)));
        assert_eq!(p.daily_tx, Some(Duration::from_secs(60 * 60)));
        assert!(p.describe().contains("allow anyone"));
        let u = UnattendedSection { max_session_min: Some(0.0), daily_tx_min: Some(5000.0), ..Default::default() };
        assert!(Policy::from_section(&u).is_err());
        let u = UnattendedSection { max_session_min: Some(0.0), ..Default::default() };
        assert_eq!(Policy::from_section(&u).unwrap().max_session, None);
        let u = UnattendedSection { allow: vec!["N0*".into()], ..Default::default() };
        assert!(Policy::from_section(&u).unwrap_err().contains("unattended.allow"));
    }

    #[test]
    fn session_clock_asks_once_and_resume_keeps_the_clock() {
        let mut s = SessionClock::default();
        let limit = Some(Duration::from_secs(60));
        let t = |s: u64| Instant(s * 1_000_000);
        let peer = a("N0AAA-1");
        s.on_event(&HostEvent::Connected { peer, session_id: 1, resumed: false, initiator: false }, t(0));
        assert_eq!(s.overdue(limit, t(59)), None);
        s.on_event(&HostEvent::LinkLost { peer }, t(30));
        s.on_event(&HostEvent::Connected { peer, session_id: 1, resumed: true, initiator: false }, t(40));
        assert_eq!(s.overdue(limit, t(61)), Some(peer), "the resumed session's clock started at 0");
        assert_eq!(s.overdue(limit, t(62)), None, "asked once");
        s.on_event(&HostEvent::Disconnected { peer }, t(63));
        s.on_event(&HostEvent::Connected { peer, session_id: 2, resumed: false, initiator: true }, t(100));
        assert_eq!(s.overdue(limit, t(150)), None);
        assert_eq!(s.overdue(None, t(1000)), None);
    }

    #[test]
    fn ledger_counts_per_utc_day_and_persists() {
        let f = std::env::temp_dir().join(format!("cairn-ledger-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&f);
        let day = 20_000 * 86_400;
        let mut l = TxLedger::new(Some(Duration::from_secs(120)), Some(f.clone()), day + 100);
        assert_eq!(l.state(day + 100), Budget::Open);
        assert!(l.may_key(Duration::from_secs(4), day + 100));
        l.record(Duration::from_secs(85), day + 200).unwrap();
        assert_eq!(l.state(day + 200), Budget::Open, "35 s left; the reserve is min(30 s, limit / 4)");
        l.record(Duration::from_secs(10), day + 200).unwrap();
        assert_eq!(l.state(day + 200), Budget::Reserve);
        assert!(!l.may_key(Duration::from_secs(26), day + 200));
        assert!(l.may_key(Duration::from_secs(25), day + 200));
        // A restart the same day continues the count.
        let mut l2 = TxLedger::new(Some(Duration::from_secs(120)), Some(f.clone()), day + 300);
        assert_eq!(l2.used(day + 300), Duration::from_secs(95));
        l2.record(Duration::from_secs(30), day + 300).unwrap();
        assert_eq!(l2.state(day + 300), Budget::Exhausted);
        // The next UTC day starts afresh.
        assert_eq!(l2.state(day + 86_400), Budget::Open);
        let mut l3 = TxLedger::new(Some(Duration::from_secs(120)), Some(f.clone()), day + 86_400 + 5);
        assert_eq!(l3.used(day + 86_400 + 5), Duration::ZERO);
        let mut none = TxLedger::new(None, None, day);
        assert!(none.may_key(Duration::from_secs(100_000), day));
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(f.with_extension("json.bak"));
    }

    #[test]
    fn watchdog_verdicts() {
        let w = Watchdog { audio_stall: Duration::from_secs(5) };
        let s = 1_000_000u64;
        assert_eq!(w.verdict(10 * s, 0, 6 * s, 9 * s), None, "unkeyed, audio fresh");
        assert_eq!(w.verdict(10 * s, 5 * s, 6 * s, 9 * s), None, "keyed 5 s of 6");
        assert!(w.verdict(12 * s, 5 * s, 6 * s, 12 * s).unwrap().contains("keyed 7.0 s"));
        assert!(w.verdict(20 * s, 0, 6 * s, 14 * s).unwrap().contains("no audio for 6.0 s"));
    }

    struct Line(Arc<AtomicBool>);
    impl Ptt for Line {
        fn set(&mut self, k: bool) -> io::Result<()> {
            self.0.store(k, Ordering::SeqCst);
            Ok(())
        }
        fn describe(&self) -> String {
            "mock line".into()
        }
    }

    #[test]
    fn watchdog_thread_releases_a_stuck_ptt_and_stops() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let line = Arc::new(AtomicBool::new(false));
        let state = WatchState::new(Duration::from_millis(200));
        let ptt = SharedPtt::new(Box::new(WatchedPtt::new(Box::new(Line(line.clone())), state.clone())));
        assert!(ptt.describe().contains("watchdog"));
        let h = Watchdog { audio_stall: Duration::from_secs(30) }.spawn(state.clone(), ptt.clone(), &STOP);
        // A transmitter that never releases.
        ptt.set(true).unwrap();
        assert!(line.load(Ordering::SeqCst));
        let t0 = std::time::Instant::now();
        while line.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            state.audio();
            std::thread::sleep(Duration::from_millis(10));
        }
        h.join().unwrap();
        assert!(!line.load(Ordering::SeqCst), "released by the watchdog");
        assert!(STOP.load(Ordering::SeqCst));
        assert!(state.tripped().unwrap().contains("PTT keyed"));
        let held = t0.elapsed();
        assert!(held >= Duration::from_millis(200) && held < Duration::from_secs(2), "{held:?}");
    }

    #[test]
    fn watchdog_thread_stops_on_an_audio_stall_and_exits_when_finished() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let line = Arc::new(AtomicBool::new(false));
        let state = WatchState::new(Duration::from_secs(10));
        let ptt = SharedPtt::new(Box::new(WatchedPtt::new(Box::new(Line(line.clone())), state.clone())));
        let h = Watchdog { audio_stall: Duration::from_millis(150) }.spawn(state.clone(), ptt.clone(), &STOP);
        h.join().unwrap();
        assert!(state.tripped().unwrap().contains("no audio"));
        assert!(STOP.load(Ordering::SeqCst));

        static STOP2: AtomicBool = AtomicBool::new(false);
        let state = WatchState::new(Duration::from_secs(10));
        let h = Watchdog { audio_stall: Duration::from_secs(60) }.spawn(state.clone(), ptt, &STOP2);
        state.finish();
        h.join().unwrap();
        assert!(state.tripped().is_none() && !STOP2.load(Ordering::SeqCst));
    }

    #[test]
    fn log_rotates_by_size() {
        let dir = std::env::temp_dir().join(format!("cairn-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("cairn.log");
        let mut l = RotatingLog::open(&p, 1024, 2).unwrap();
        for i in 0..100 {
            l.line(&format!("line {i:03} {}", "x".repeat(40))).unwrap();
        }
        let cur = std::fs::read_to_string(&p).unwrap();
        let one = std::fs::read_to_string(dir.join("cairn.log.1")).unwrap();
        assert!(dir.join("cairn.log.2").exists());
        assert!(!dir.join("cairn.log.3").exists(), "only two rotated files kept");
        assert!(cur.len() <= 1024 && one.len() <= 1024);
        assert!(cur.trim_end().ends_with(&format!("line 099 {}", "x".repeat(40))));
        assert!(cur.starts_with("20"), "UTC stamp: {cur}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hook_calls_for_events_and_saved_objects() {
        let me = a("N0ME-1");
        let peer = a("N0AAA-2");
        let c = hook_calls(
            &Output::Host(HostEvent::Connected { peer, session_id: 7, resumed: false, initiator: false }),
            None,
            me,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].event, HookEvent::Connected);
        assert!(c[0].env.contains(&("CAIRN_PEER".into(), "N0AAA-2".into())));
        assert!(c[0].env.contains(&("CAIRN_SESSION_ID".into(), "7".into())));
        let c = hook_calls(&Output::Host(HostEvent::LinkLost { peer }), None, me);
        assert!(c[0].env.contains(&("CAIRN_REASON".into(), "link-lost".into())));
        assert!(hook_calls(&Output::Host(HostEvent::Acked { peer, ids: vec![1] }), None, me).is_empty());

        let pdu = Pdu::message(b"hello; $(rm -rf ~)".to_vec());
        let saved = Saved { path: "/rx/N0AAA-2_msg1.txt".into(), completed: None, message: true };
        let coding = crate::runtime::Coding::raw(pdu.body.len());
        let c = hook_calls(&Output::Deliver { from: peer, pdu: pdu.clone(), coding }, Some(&saved), me);
        assert_eq!(c[0].event, HookEvent::ReceivedMessage);
        assert_eq!(c[0].stdin, pdu.body);
        assert!(c[0].env.contains(&("CAIRN_FILE".into(), "/rx/N0AAA-2_msg1.txt".into())));
        let saved = Saved {
            path: "/rx/N0AAA-2_obj1.bin".into(),
            completed: Some(crate::rxdir::Completed {
                from: peer,
                object: 1,
                path: "/rx/N0AAA-2_obj1.bin".into(),
                bytes: 10,
                sha256: "ab".into(),
            }),
            message: false,
        };
        let coding = crate::runtime::Coding::raw(pdu.body.len());
        let c = hook_calls(&Output::Deliver { from: peer, pdu, coding }, Some(&saved), me);
        assert_eq!(c[0].event, HookEvent::ReceivedFile);
        assert!(c[0].env.contains(&("CAIRN_BYTES".into(), "10".into())));
    }

    #[cfg(unix)]
    #[test]
    fn hooks_run_without_a_shell_and_receive_content_only_in_env_and_stdin() {
        let dir = std::env::temp_dir().join(format!("cairn-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        // The test's hook is a small script; its arguments come from the
        // config, never from received data.
        let script = dir.join("hook.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s|' \"$CAIRN_EVENT\" \"$CAIRN_PEER\" \"$1\" > {o}\ncat >> {o}\n",
                o = out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = Hooks::from_section(&HooksSection {
            received_message: vec![script.display().to_string(), "fixed-arg".into()],
            timeout_s: Some(5.0),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(hooks.describe(), "received_message");
        let evil = format!("x$(touch {})\n;`id`", dir.join("pwned").display());
        let mut call = HookCall::new(HookEvent::ReceivedMessage).var("CAIRN_PEER", "N0AAA\u{0}-2\n");
        call.stdin = evil.clone().into_bytes();
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(hooks.fire(call, move |l| tx.send(l).unwrap()));
        let line = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(line.contains("exit status: 0"), "{line}");
        let got = std::fs::read_to_string(&out).unwrap();
        assert_eq!(got, format!("received_message|N0AAA?-2?|fixed-arg|{evil}"));
        assert!(!dir.join("pwned").exists(), "nothing interpolated");
        // No hook for other events.
        assert!(!hooks.fire(HookCall::new(HookEvent::Connected), |_| {}));
        // A hung hook is killed.
        let slow = Hooks::from_section(&HooksSection {
            alarm: vec!["sleep".into(), "30".into()],
            timeout_s: Some(0.2),
            ..Default::default()
        })
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        slow.fire(HookCall::new(HookEvent::Alarm), move |l| tx.send(l).unwrap());
        assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap().contains("killed"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
