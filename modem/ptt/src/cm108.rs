//! CM108/CM119-class HID GPIO keying through Linux hidraw (Digirig, DRA-xx,
//! the AIOC's HID interface): a 5-byte output report sets the GPIO pin's
//! direction to output and its level (the layout Direwolf and others use).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};

use crate::Ptt;

/// CM108 GPIO keying.
pub struct Cm108Ptt {
    file: File,
    path: String,
    gpio: u8,
}

/// The output report for `gpio` (1-8) at `keyed`.
pub fn report(gpio: u8, keyed: bool) -> [u8; 5] {
    let mask = 1u8 << (gpio - 1);
    // [report id 0, reserved, GPIO data, GPIO direction (1 = output), reserved]
    [0, 0, if keyed { mask } else { 0 }, mask, 0]
}

impl Cm108Ptt {
    /// Open `path` (a hidraw node) and unkey.
    pub fn open(path: &str, gpio: u8) -> io::Result<Self> {
        if !(1..=8).contains(&gpio) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "CM108 GPIO must be 1-8"));
        }
        let file = OpenOptions::new().write(true).open(path).map_err(|e| {
            let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                " (hidraw nodes usually need the plugdev group or a udev rule)"
            } else {
                ""
            };
            io::Error::new(e.kind(), format!("{path}: {e}{hint}"))
        })?;
        let mut p = Cm108Ptt { file, path: path.to_string(), gpio };
        p.set(false)?;
        Ok(p)
    }
}

impl Ptt for Cm108Ptt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        let r = report(self.gpio, keyed);
        let n = self.file.write(&r)?;
        if n != r.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short HID report write"));
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("CM108 HID {} GPIO {}", self.path, self.gpio)
    }
}

impl Drop for Cm108Ptt {
    fn drop(&mut self) {
        let _ = self.set(false);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports() {
        assert_eq!(super::report(3, true), [0, 0, 4, 4, 0]);
        assert_eq!(super::report(3, false), [0, 0, 0, 4, 0]);
        assert_eq!(super::report(1, true), [0, 0, 1, 1, 0]);
    }
}
