//! Serial RTS/DTR keying (Unix).
//!
//! The port is opened with raw `open(2)` and driven only with modem-control
//! ioctls: no termios changes (no baud rate, no flow control), so nothing
//! else touches the lines. Both lines are moved together with one
//! `TIOCMSET`, so the pair never passes through an intermediate state; with
//! the AIOC (keyed by DTR high *and* RTS low) a separate "DTR down, then RTS
//! down" sequence in the wrong order would key it for an instant.
//!
//! On Linux the tty layer raises DTR and RTS together inside `open(2)` when
//! the port's saved baud rate is non-zero; we drop both in the very next
//! call. With an active-high line that glitch (both high, well under a
//! millisecond) does not key the AIOC, which needs RTS low; a radio keyed by
//! DTR alone would see it, but no radio keys that fast.
//!
//! On close (including a crash or `kill -9`), HUPCL makes the kernel drop
//! DTR and RTS, which unkeys an active-high line; we make sure HUPCL is set
//! for active-high lines. An inverted (active-low) line would key on close,
//! so we leave HUPCL alone there and release explicitly.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::Ptt;

/// Serial modem-control line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialLine {
    /// Data Terminal Ready.
    Dtr,
    /// Request To Send.
    Rts,
}

impl SerialLine {
    fn bit(self) -> libc::c_int {
        match self {
            SerialLine::Dtr => libc::TIOCM_DTR,
            SerialLine::Rts => libc::TIOCM_RTS,
        }
    }
    fn other(self) -> SerialLine {
        match self {
            SerialLine::Dtr => SerialLine::Rts,
            SerialLine::Rts => SerialLine::Dtr,
        }
    }
    fn name(self) -> &'static str {
        match self {
            SerialLine::Dtr => "DTR",
            SerialLine::Rts => "RTS",
        }
    }
}

/// Keying by one serial line; the other line is held low.
pub struct SerialPtt {
    fd: OwnedFd,
    path: PathBuf,
    line: SerialLine,
    active_high: bool,
}

fn get_bits(fd: libc::c_int) -> io::Result<libc::c_int> {
    let mut bits: libc::c_int = 0;
    // SAFETY: TIOCMGET writes one c_int through the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCMGET, &mut bits) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bits)
}

fn set_bits(fd: libc::c_int, bits: libc::c_int) -> io::Result<()> {
    // SAFETY: TIOCMSET reads one c_int through the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCMSET, &bits) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl SerialPtt {
    /// Open `path` and put the lines in the unkeyed state at once.
    /// `line` keys; `active_high` false keys with the line low.
    pub fn open(path: &Path, line: SerialLine, active_high: bool) -> io::Result<Self> {
        let c = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in serial path"))?;
        // SAFETY: plain open(2) with a valid C string.
        let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
        // SAFETY: raw is a freshly opened descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut p = SerialPtt { fd, path: path.to_path_buf(), line, active_high };
        // First thing: unkeyed (both lines moved in one ioctl).
        p.apply(false)?;
        // Keep other programs from opening the port and fighting over the lines.
        // SAFETY: TIOCEXCL takes no argument.
        unsafe { libc::ioctl(p.fd.as_raw_fd(), libc::TIOCEXCL) };
        if active_high {
            p.ensure_hupcl()?;
        }
        Ok(p)
    }

    fn ensure_hupcl(&mut self) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        // SAFETY: termios is plain data, filled by tcgetattr.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if t.c_cflag & libc::HUPCL == 0 {
            t.c_cflag |= libc::HUPCL;
            // Only HUPCL changes; the baud rate stays, so the driver leaves the lines alone.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn target_bits(&self, current: libc::c_int, keyed: bool) -> libc::c_int {
        let mut bits = current & !(libc::TIOCM_DTR | libc::TIOCM_RTS);
        if keyed == self.active_high {
            bits |= self.line.bit();
        }
        bits
    }

    fn apply(&mut self, keyed: bool) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        let cur = get_bits(fd)?;
        set_bits(fd, self.target_bits(cur, keyed))
    }

    /// The device path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Ptt for SerialPtt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        self.apply(keyed)
    }

    fn read_back(&mut self) -> io::Result<Option<bool>> {
        let bits = get_bits(self.fd.as_raw_fd())?;
        let line_high = bits & self.line.bit() != 0;
        let other_high = bits & self.line.other().bit() != 0;
        Ok(Some(line_high == self.active_high && !other_high))
    }

    fn describe(&self) -> String {
        let (on, off) = if self.active_high { ("high", "low") } else { ("low", "high") };
        format!(
            "serial {} (keyed: {} {on}, {} low; idle: {} {off})",
            self.path.display(),
            self.line.name(),
            self.line.other().name(),
            self.line.name()
        )
    }
}

impl Drop for SerialPtt {
    fn drop(&mut self) {
        let _ = self.apply(false);
    }
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// `port` as a path if it exists, else the single `/dev/serial/by-id/` entry
/// whose name contains it (ignoring case and punctuation).
pub fn resolve_port(port: &str) -> io::Result<PathBuf> {
    let p = Path::new(port);
    if p.exists() {
        return Ok(p.to_path_buf());
    }
    let want = normalize(port);
    let mut found = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/serial/by-id") {
        for e in rd.flatten() {
            if normalize(&e.file_name().to_string_lossy()).contains(&want) {
                found.push(e.path());
            }
        }
    }
    found.sort();
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no serial port {port:?} (not a path, and no /dev/serial/by-id/ name contains it)"),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "serial port {port:?} is ambiguous: {}",
                found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(line: SerialLine, active_high: bool) -> SerialPtt {
        // A descriptor that is never used for ioctls in these tests.
        let f = std::fs::File::open("/dev/null").unwrap();
        SerialPtt { fd: f.into(), path: "/dev/null".into(), line, active_high }
    }

    #[test]
    fn line_bits() {
        let (dtr, rts) = (libc::TIOCM_DTR, libc::TIOCM_RTS);
        let p = fake(SerialLine::Dtr, true);
        assert_eq!(p.target_bits(dtr | rts, true), dtr);
        assert_eq!(p.target_bits(dtr | rts, false), 0);
        let p2 = fake(SerialLine::Rts, false);
        assert_eq!(p2.target_bits(0, true), 0);
        assert_eq!(p2.target_bits(dtr, false), rts);
        // Other modem bits are preserved.
        assert_eq!(p.target_bits(libc::TIOCM_CTS | rts, true), libc::TIOCM_CTS | dtr);
        // Dropping tries a TIOCMSET on /dev/null, which fails harmlessly.
    }
}
