//! Received objects on disk (`cairn-modem run --rx-dir DIR`).
//!
//! FILE-CHUNK PDUs (SPEC 12.1, 12.4: a 32-bit object ID, a 32-bit byte
//! offset, then data) are written into `DIR/CALL_objN.bin.part` at their
//! offset as they arrive, keyed by sender and object ID; MESSAGE PDUs
//! (session messages and UI broadcasts) become `DIR/CALL_msgN.txt` and
//! `DIR/CALL_uiN.txt`. The protocol carries no object length, so this
//! modem's own convention marks the end (`control::HostState::file_commands`):
//! every chunk but the last carries [`FILE_CHUNK_DATA`] bytes, and an object
//! whose length is a multiple of that ends with an empty chunk at its
//! length. When every byte up to the end has arrived the file is renamed to
//! `CALL_objN.bin` and its path and SHA-256 are reported. Names never
//! overwrite: an existing name gets `-2`, `-3` and so on.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cairn_link::pdu::pdu_type;
use cairn_link::{Address, Pdu};

use crate::control::FILE_CHUNK_DATA;

/// An object being received.
struct Partial {
    part: PathBuf,
    done: PathBuf,
    file: File,
    /// Received byte ranges, sorted and merged.
    have: Vec<(u64, u64)>,
    end: Option<u64>,
}

/// The receive directory.
pub struct RxDir {
    dir: PathBuf,
    objects: HashMap<(Address, u32), Partial>,
    messages: u64,
}

/// A file finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completed {
    /// The sender.
    pub from: Address,
    /// Its object ID.
    pub object: u32,
    /// Where it is.
    pub path: PathBuf,
    /// Its length.
    pub bytes: u64,
    /// SHA-256, hex.
    pub sha256: String,
}

/// Where a delivered PDU was saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The file (a `.part` file while an object is incomplete).
    pub path: PathBuf,
    /// The object, when this PDU completed it.
    pub completed: Option<Completed>,
    /// A MESSAGE PDU saved whole.
    pub message: bool,
}

impl Saved {
    /// The lines to print: a completed file's path and SHA-256, a
    /// message's path (nothing for a chunk of an incomplete object).
    pub fn lines(&self, from: Address, pdu: &Pdu) -> Vec<String> {
        match self {
            Saved { completed: Some(c), .. } => vec![format!(
                "rx file from {} object {} complete: {} bytes -> {} sha256 {}",
                c.from,
                c.object,
                c.bytes,
                c.path.display(),
                c.sha256
            )],
            Saved { path, completed: None, message: true } => {
                vec![format!("rx message from {from} saved: {} ({} bytes)", path.display(), pdu.body.len())]
            }
            _ => Vec::new(),
        }
    }
}

/// The callsign as a file-name part: letters, digits and '-' only.
fn call_part(a: &Address) -> String {
    a.to_string().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// `stem.ext` in `dir`, or `stem-2.ext`, `stem-3.ext`, ... if taken (by the
/// file or its `.part`).
fn unique(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let taken = |p: &Path| p.exists() || p.with_extension(format!("{ext}.part")).exists();
    let first = dir.join(format!("{stem}.{ext}"));
    if !taken(&first) {
        return first;
    }
    (2..).map(|k| dir.join(format!("{stem}-{k}.{ext}"))).find(|p| !taken(p)).expect("a free name")
}

/// SHA-256 of a byte string, lower-case hex.
pub fn sha256_hex(data: &[u8]) -> String {
    cairn_phy::fields::sha256(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn add_range(have: &mut Vec<(u64, u64)>, r: (u64, u64)) {
    if r.0 >= r.1 {
        return;
    }
    have.push(r);
    have.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(have.len());
    for &(a, b) in have.iter() {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    *have = merged;
}

impl RxDir {
    /// Use (and create) `dir`.
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(RxDir { dir: dir.to_path_buf(), objects: HashMap::new(), messages: 0 })
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A delivered PDU (session, or broadcast when `broadcast`). Returns the
    /// lines to print: a completed file's path and SHA-256, a message's path.
    pub fn on_pdu(&mut self, from: Address, pdu: &Pdu, broadcast: bool) -> io::Result<Vec<String>> {
        Ok(self.save(from, pdu, broadcast)?.map_or_else(Vec::new, |s| s.lines(from, pdu)))
    }

    /// A delivered PDU: where it went. FILE-CHUNK PDUs of sessions go into
    /// their object's file (`completed` once it is whole), MESSAGE PDUs into
    /// a file of their own; other PDUs, and broadcast FILE-CHUNKs, are not
    /// saved.
    pub fn save(&mut self, from: Address, pdu: &Pdu, broadcast: bool) -> io::Result<Option<Saved>> {
        match pdu.pdu_type {
            pdu_type::FILE_CHUNK if !broadcast && pdu.body.len() >= 8 => {
                let obj = u32::from_be_bytes(pdu.body[0..4].try_into().expect("4 bytes"));
                let off = u32::from_be_bytes(pdu.body[4..8].try_into().expect("4 bytes"));
                Ok(Some(match self.chunk(from, obj, off as u64, &pdu.body[8..])? {
                    Some(c) => Saved { path: c.path.clone(), completed: Some(c), message: false },
                    None => {
                        let path = self.objects.get(&(from, obj)).map(|p| p.part.clone()).unwrap_or_default();
                        Saved { path, completed: None, message: false }
                    }
                }))
            }
            pdu_type::MESSAGE => {
                self.messages += 1;
                let kind = if broadcast { "ui" } else { "msg" };
                let path = unique(&self.dir, &format!("{}_{kind}{}", call_part(&from), self.messages), "txt");
                fs::write(&path, &pdu.body)?;
                Ok(Some(Saved { path, completed: None, message: true }))
            }
            _ => Ok(None),
        }
    }

    /// One FILE-CHUNK: write it, and finish the object when it is whole.
    pub fn chunk(&mut self, from: Address, obj: u32, off: u64, data: &[u8]) -> io::Result<Option<Completed>> {
        let key = (from, obj);
        if !self.objects.contains_key(&key) {
            let done = unique(&self.dir, &format!("{}_obj{obj}", call_part(&from)), "bin");
            let part = done.with_extension("bin.part");
            let file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&part)?;
            self.objects.insert(key, Partial { part, done, file, have: Vec::new(), end: None });
        }
        let p = self.objects.get_mut(&key).expect("object");
        if !data.is_empty() {
            p.file.seek(SeekFrom::Start(off))?;
            p.file.write_all(data)?;
            add_range(&mut p.have, (off, off + data.len() as u64));
        }
        if data.len() < FILE_CHUNK_DATA {
            p.end = Some(off + data.len() as u64);
        }
        let complete = match p.end {
            Some(0) => true,
            Some(end) => p.have.first().is_some_and(|&(a, b)| a == 0 && b >= end),
            None => false,
        };
        if !complete {
            return Ok(None);
        }
        let p = self.objects.remove(&key).expect("object");
        let end = p.end.expect("end");
        p.file.set_len(end)?;
        p.file.sync_all()?;
        drop(p.file);
        fs::rename(&p.part, &p.done)?;
        let bytes = fs::read(&p.done)?;
        Ok(Some(Completed { from, object: obj, path: p.done, bytes: end, sha256: sha256_hex(&bytes) }))
    }

    /// Objects still incomplete: (sender, object ID, bytes so far).
    pub fn pending(&self) -> Vec<(Address, u32, u64)> {
        let mut v: Vec<_> =
            self.objects.iter().map(|(k, p)| (k.0, k.1, p.have.iter().map(|r| r.1 - r.0).sum())).collect();
        v.sort_by_key(|x| (x.0.raw(), x.1));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::HostState;
    use cairn_link::HostCommand;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cairn-rxdir-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn deliver(rx: &mut RxDir, from: Address, data: &[u8], order: &[usize]) -> Vec<String> {
        let mut h = HostState::new();
        let (cmds, _, _) = h.file_commands(data);
        let pdus: Vec<Pdu> = cmds
            .into_iter()
            .map(|c| match c {
                HostCommand::Send { pdu, .. } => pdu,
                _ => unreachable!(),
            })
            .collect();
        let mut lines = Vec::new();
        for &i in order.iter().filter(|&&i| i < pdus.len()) {
            lines.extend(rx.on_pdu(from, &pdus[i], false).unwrap());
        }
        lines
    }

    #[test]
    fn files_reassemble_by_object_and_offset() {
        let dir = tmp("files");
        let mut rx = RxDir::new(&dir).unwrap();
        let a = crate::control::parse_call("N0CALL-1").unwrap();
        for (len, order) in [
            (5000usize, vec![0, 1, 2]),
            (2 * FILE_CHUNK_DATA, vec![0, 1, 2]),
            (5000, vec![2, 0, 1]),
            (0, vec![0]),
            (FILE_CHUNK_DATA * 3 + 7, vec![3, 1, 0, 2]),
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
            let lines = deliver(&mut rx, a, &data, &order);
            assert_eq!(lines.len(), 1, "{len}: {lines:?}");
            let path = lines[0].split(" -> ").nth(1).unwrap().split(" sha256 ").next().unwrap();
            assert_eq!(fs::read(path).unwrap(), data, "{len}");
            assert!(lines[0].ends_with(&sha256_hex(&data)));
            assert!(path.contains("N0CALL-1_obj1"), "{path}");
        }
        assert!(rx.pending().is_empty());
        // Same object ID again (a new run of the sender): a new name.
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 5, "{names:?}");
        // A message.
        let l = rx.on_pdu(a, &Pdu::message(b"hello de N0CALL".to_vec()), false).unwrap();
        assert!(l[0].contains("N0CALL-1_msg1.txt"), "{l:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_objects_stay_part_files() {
        let dir = tmp("partial");
        let mut rx = RxDir::new(&dir).unwrap();
        let a = crate::control::parse_call("N0CALL-2").unwrap();
        let data = vec![9u8; 5000];
        assert!(deliver(&mut rx, a, &data, &[0, 2]).is_empty());
        assert_eq!(rx.pending(), vec![(a, 1, 2048 + 904)]);
        assert!(dir.join("N0CALL-2_obj1.bin.part").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
