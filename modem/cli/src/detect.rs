//! PTT candidates on this host, found by reading sysfs and `/dev/serial/by-id`
//! only: nothing is opened, nothing keyed (SPEC 13.6 guided setup).
//!
//! Serial ports are named by their by-id links (`usb-AIOC_All-In-One-Cable_…`)
//! and carry the USB vendor and product of the device behind them; HID nodes
//! (`/dev/hidrawN`) carry theirs from `uevent`'s `HID_ID`. Known interfaces
//! are recognized by USB ID ([`KNOWN`]) and the rig profiles' name matches.

use std::path::{Path, PathBuf};

/// A known USB interface.
#[derive(Clone, Copy, Debug)]
pub struct Known {
    /// Vendor ID.
    pub vid: u16,
    /// Product ID (None: any product of the vendor).
    pub pid: Option<u16>,
    /// What it is.
    pub name: &'static str,
    /// The rig profile to suggest.
    pub rig: &'static str,
}

/// USB IDs recognized. From the vendors' and pid.codes' registrations; a
/// device missing here is still listed, just without a name.
pub const KNOWN: &[Known] = &[
    Known { vid: 0x1209, pid: Some(0x7388), name: "AIOC (All-In-One-Cable)", rig: "aioc-ht" },
    Known { vid: 0x10c4, pid: Some(0xea60), name: "Silicon Labs CP210x serial (Digirig and others)", rig: "digirig" },
    Known { vid: 0x0d8c, pid: None, name: "C-Media CM108/CM119-class audio", rig: "cm108-gpio" },
    Known { vid: 0x08bb, pid: Some(0x2902), name: "TI PCM2902 codec (SignaLink and others)", rig: "signalink" },
    Known { vid: 0x08bb, pid: Some(0x2904), name: "TI PCM2904 codec (SignaLink and others)", rig: "signalink" },
];

/// The known interface for a vendor and product.
pub fn known(vid: u16, pid: u16) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.vid == vid && k.pid.is_none_or(|p| p == pid))
}

/// Kind of PTT candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttKind {
    /// A USB serial port (RTS/DTR keying).
    Serial,
    /// A HID node (CM108 GPIO keying).
    Hid,
}

/// A device that may key a radio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PttCandidate {
    /// Serial or HID.
    pub kind: PttKind,
    /// The path to give `--ptt` (by-id link or /dev/hidrawN).
    pub path: PathBuf,
    /// USB vendor and product, when found.
    pub usb: Option<(u16, u16)>,
    /// A readable name (the by-id name or the HID name).
    pub label: String,
}

impl PttCandidate {
    /// The known interface, when recognized.
    pub fn known(&self) -> Option<&'static Known> {
        self.usb.and_then(|(v, p)| known(v, p))
    }

    /// `vid:pid` in hex.
    pub fn usb_text(&self) -> Option<String> {
        self.usb.map(|(v, p)| format!("{v:04x}:{p:04x}"))
    }

    /// The `--ptt` spec for this device (serial keys on DTR by default;
    /// the profile or setup sets the line).
    pub fn spec(&self) -> String {
        match self.kind {
            PttKind::Serial => format!("serial:{}", self.path.display()),
            PttKind::Hid => format!("cm108:{}", self.path.display()),
        }
    }
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn hex16(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

/// The USB vendor and product of a sysfs device: the first ancestor with
/// `idVendor` and `idProduct`.
fn usb_ids_of(dev: &Path) -> Option<(u16, u16)> {
    let mut d = std::fs::canonicalize(dev).ok()?;
    for _ in 0..8 {
        if let (Some(v), Some(p)) = (read_trim(&d.join("idVendor")), read_trim(&d.join("idProduct"))) {
            return Some((hex16(&v)?, hex16(&p)?));
        }
        if !d.pop() {
            break;
        }
    }
    None
}

/// Serial ports under `root/dev/serial/by-id`, with the USB IDs from
/// `root/sys/class/tty/<tty>/device`.
pub fn serial_ports(root: &Path) -> Vec<PttCandidate> {
    let by_id = root.join("dev/serial/by-id");
    let mut out: Vec<PttCandidate> = std::fs::read_dir(&by_id)
        .map(|r| {
            r.flatten()
                .map(|e| {
                    let path = e.path();
                    let label = e.file_name().to_string_lossy().into_owned();
                    let tty = std::fs::read_link(&path)
                        .ok()
                        .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()));
                    let usb = tty.and_then(|t| usb_ids_of(&root.join("sys/class/tty").join(t).join("device")));
                    // The /dev path, also when the root is a test tree.
                    let path = PathBuf::from("/dev/serial/by-id").join(&label);
                    PttCandidate { kind: PttKind::Serial, path, usb, label }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// HID nodes under `root/sys/class/hidraw`, with `HID_ID` and `HID_NAME`
/// from their `device/uevent`.
pub fn hid_nodes(root: &Path) -> Vec<PttCandidate> {
    let class = root.join("sys/class/hidraw");
    let mut out: Vec<PttCandidate> = std::fs::read_dir(&class)
        .map(|r| {
            r.flatten()
                .filter_map(|e| {
                    let node = e.file_name().to_string_lossy().into_owned();
                    let uevent = std::fs::read_to_string(e.path().join("device/uevent")).ok()?;
                    let field = |k: &str| {
                        uevent.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('='))).map(str::trim)
                    };
                    // HID_ID=0003:00000D8C:0000013C (bus:vendor:product)
                    let usb = field("HID_ID").and_then(|id| {
                        let mut it = id.split(':');
                        let (_bus, v, p) = (it.next()?, it.next()?, it.next()?);
                        Some((u16::try_from(u32::from_str_radix(v, 16).ok()?).ok()?, {
                            u16::try_from(u32::from_str_radix(p, 16).ok()?).ok()?
                        }))
                    });
                    let label = field("HID_NAME").unwrap_or(&node).to_string();
                    Some(PttCandidate { kind: PttKind::Hid, path: PathBuf::from("/dev").join(&node), usb, label })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Every candidate: serial ports, then HID nodes of known interfaces (other
/// HID devices are keyboards and mice).
pub fn ptt_candidates(root: &Path) -> Vec<PttCandidate> {
    let mut v = serial_ports(root);
    v.extend(hid_nodes(root).into_iter().filter(|c| c.known().is_some()));
    v
}

/// `cm108:auto`: the one C-Media-class HID node (vendor 0x0d8c, or the
/// AIOC's HID interface), else an error naming what was found.
pub fn resolve_cm108_auto(root: &Path) -> Result<PathBuf, String> {
    let nodes: Vec<PttCandidate> =
        hid_nodes(root).into_iter().filter(|c| matches!(c.usb, Some((0x0d8c, _)) | Some((0x1209, 0x7388)))).collect();
    match nodes.as_slice() {
        [one] => Ok(one.path.clone()),
        [] => Err("cm108:auto: no CM108-class HID device found (is the interface plugged in?)".into()),
        many => Err(format!(
            "cm108:auto: {} CM108-class HID devices ({}); name one with cm108:/dev/hidrawN",
            many.len(),
            many.iter().map(|c| format!("{} {}", c.path.display(), c.label)).collect::<Vec<_>>().join(", ")
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake sysfs and /dev tree: an AIOC (serial and HID), a CP2102 serial
    /// port, a C-Media HID node and a keyboard.
    pub(crate) fn fake_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cairn-detect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |p: &str, text: &str| {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        // USB devices with their interfaces below them.
        mk("sys/devices/usb1/1-1/idVendor", "1209\n");
        mk("sys/devices/usb1/1-1/idProduct", "7388\n");
        mk("sys/devices/usb1/1-2/idVendor", "10c4\n");
        mk("sys/devices/usb1/1-2/idProduct", "ea60\n");
        std::fs::create_dir_all(root.join("sys/devices/usb1/1-1/1-1:1.0/tty/ttyACM0")).unwrap();
        std::fs::create_dir_all(root.join("sys/devices/usb1/1-2/1-2:1.0/ttyUSB0")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/tty/ttyACM0")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/tty/ttyUSB0")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(root.join("sys/devices/usb1/1-1/1-1:1.0"), root.join("sys/class/tty/ttyACM0/device")).unwrap();
            symlink(root.join("sys/devices/usb1/1-2/1-2:1.0"), root.join("sys/class/tty/ttyUSB0/device")).unwrap();
            std::fs::create_dir_all(root.join("dev/serial/by-id")).unwrap();
            symlink("../../ttyACM0", root.join("dev/serial/by-id/usb-AIOC_All-In-One-Cable_1234abcd-if00")).unwrap();
            symlink(
                "../../ttyUSB0",
                root.join("dev/serial/by-id/usb-Silicon_Labs_CP2102N_USB_to_UART_Bridge_Controller_99-if00-port0"),
            )
            .unwrap();
        }
        mk("sys/class/hidraw/hidraw0/device/uevent", "HID_ID=0003:0000046D:0000C52B\nHID_NAME=Logitech Keyboard\n");
        mk("sys/class/hidraw/hidraw1/device/uevent", "HID_ID=0003:00001209:00007388\nHID_NAME=AIOC All-In-One-Cable\n");
        mk(
            "sys/class/hidraw/hidraw2/device/uevent",
            "HID_ID=0003:00000D8C:0000013C\nHID_NAME=C-Media USB Audio Device\n",
        );
        root
    }

    #[cfg(unix)]
    #[test]
    fn finds_serial_and_hid_candidates_by_usb_id() {
        let root = fake_root("find");
        let c = ptt_candidates(&root);
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "usb-AIOC_All-In-One-Cable_1234abcd-if00",
                "usb-Silicon_Labs_CP2102N_USB_to_UART_Bridge_Controller_99-if00-port0",
                "AIOC All-In-One-Cable",
                "C-Media USB Audio Device"
            ],
            "the keyboard is left out"
        );
        assert_eq!(c[0].usb, Some((0x1209, 0x7388)));
        assert_eq!(c[0].known().unwrap().rig, "aioc-ht");
        assert_eq!(c[1].known().unwrap().rig, "digirig");
        assert_eq!(c[0].spec(), "serial:/dev/serial/by-id/usb-AIOC_All-In-One-Cable_1234abcd-if00");
        assert_eq!(c[3].spec(), "cm108:/dev/hidraw2");
        assert_eq!(c[3].usb_text().as_deref(), Some("0d8c:013c"));
        // Two CM108-class nodes (the AIOC's and the C-Media's): auto refuses to guess.
        assert!(resolve_cm108_auto(&root).unwrap_err().contains("2 CM108-class"));
        std::fs::remove_file(root.join("sys/class/hidraw/hidraw1/device/uevent")).unwrap();
        assert_eq!(resolve_cm108_auto(&root).unwrap(), PathBuf::from("/dev/hidraw2"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_host_has_no_candidates() {
        let root = std::env::temp_dir().join(format!("cairn-detect-empty-{}", std::process::id()));
        assert!(ptt_candidates(&root).is_empty());
        assert!(resolve_cm108_auto(&root).is_err());
    }
}
