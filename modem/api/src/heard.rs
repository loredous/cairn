//! The heard list (SPEC 13.6): every decoded header updates its source's
//! entry; queried by `heard`, pushed as `heard` events, and optionally kept
//! in a file across restarts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::proto::{parse_utc, round, utc_string};

/// One decoded header, as the heard list needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    /// The source as printed (`N0CALL-1`, `#0123456789` for a hash).
    pub address: String,
    /// Callsign text (the address for a hash).
    pub call: String,
    /// SSID.
    pub ssid: u8,
    /// Destination as printed (`*` for broadcast).
    pub dst: String,
    /// Frame type name (`DATA`, `CONNECT-ACK`, ...).
    pub frame_type: String,
    /// Waveform (`cairn`).
    pub mode: String,
    /// Rung.
    pub mcs: u8,
    /// Width, tones.
    pub width: u8,
    /// Channel SNR, dB.
    pub snr_db: f64,
    /// When, Unix microseconds.
    pub unix_us: i64,
    /// When, modem seconds.
    pub modem_time: f64,
}

/// A station's entry.
#[derive(Clone, Debug, PartialEq)]
pub struct HeardEntry {
    /// The station as printed.
    pub address: String,
    /// Callsign text.
    pub call: String,
    /// SSID.
    pub ssid: u8,
    /// First header, Unix microseconds.
    pub first_us: i64,
    /// Last header, Unix microseconds.
    pub last_us: i64,
    /// Last header, modem seconds (None: loaded from a file, not heard since).
    pub last_modem_time: Option<f64>,
    /// Headers decoded.
    pub count: u64,
    /// SNR of the last header.
    pub last_snr_db: f64,
    /// Best SNR.
    pub best_snr_db: f64,
    /// Width of the last frame.
    pub last_width: u8,
    /// Type of the last frame.
    pub last_frame_type: String,
    /// Rung of the last frame.
    pub last_mcs: u8,
    /// Destination of the last frame.
    pub last_dst: String,
    /// Headers per frame type.
    pub frame_types: BTreeMap<String, u64>,
    /// Modes heard in.
    pub modes: BTreeSet<String>,
}

impl HeardEntry {
    /// The `heard` result entry (`docs/api.md`).
    pub fn to_json(&self) -> Value {
        json!({
            "address": self.address,
            "call": self.call,
            "ssid": self.ssid,
            "first_heard": utc_string(self.first_us),
            "last_heard": utc_string(self.last_us),
            "last_modem_time": self.last_modem_time.map(|t| round(t, 3)),
            "count": self.count,
            "last_snr_db": round(self.last_snr_db, 2),
            "best_snr_db": round(self.best_snr_db, 2),
            "last_width": self.last_width,
            "last_frame_type": self.last_frame_type,
            "last_mcs": self.last_mcs,
            "last_dst": self.last_dst,
            "frame_types": self.frame_types,
            "modes": self.modes,
        })
    }

    fn from_json(v: &Value) -> Option<HeardEntry> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(Value::as_f64);
        let u = |k: &str| v.get(k).and_then(Value::as_u64);
        Some(HeardEntry {
            address: s("address")?,
            call: s("call")?,
            ssid: u("ssid")? as u8,
            first_us: parse_utc(&s("first_heard")?)?,
            last_us: parse_utc(&s("last_heard")?)?,
            last_modem_time: None,
            count: u("count")?.max(1),
            last_snr_db: n("last_snr_db")?,
            best_snr_db: n("best_snr_db").or(n("last_snr_db"))?,
            last_width: u("last_width")? as u8,
            last_frame_type: s("last_frame_type")?,
            last_mcs: u("last_mcs")? as u8,
            last_dst: s("last_dst").unwrap_or_else(|| "*".into()),
            frame_types: v
                .get("frame_types")
                .and_then(Value::as_object)
                .map(|m| m.iter().filter_map(|(k, c)| Some((k.clone(), c.as_u64()?))).collect())
                .unwrap_or_default(),
            modes: v
                .get("modes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        })
    }
}

/// The heard list.
#[derive(Clone, Debug, Default)]
pub struct HeardList {
    entries: HashMap<String, HeardEntry>,
    dirty: bool,
}

/// File format tag.
pub const HEARD_FORMAT: &str = "cairn-heard/1";

impl HeardList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// A decoded header: update (or start) its source's entry.
    pub fn update(&mut self, h: &Heard) -> &HeardEntry {
        self.dirty = true;
        let e = self.entries.entry(h.address.clone()).or_insert_with(|| HeardEntry {
            address: h.address.clone(),
            call: h.call.clone(),
            ssid: h.ssid,
            first_us: h.unix_us,
            last_us: h.unix_us,
            last_modem_time: None,
            count: 0,
            last_snr_db: h.snr_db,
            best_snr_db: h.snr_db,
            last_width: h.width,
            last_frame_type: h.frame_type.clone(),
            last_mcs: h.mcs,
            last_dst: h.dst.clone(),
            frame_types: BTreeMap::new(),
            modes: BTreeSet::new(),
        });
        e.count += 1;
        e.last_us = h.unix_us.max(e.last_us);
        e.first_us = e.first_us.min(h.unix_us);
        e.last_modem_time = Some(h.modem_time);
        e.last_snr_db = h.snr_db;
        e.best_snr_db = if e.count == 1 { h.snr_db } else { e.best_snr_db.max(h.snr_db) };
        e.last_width = h.width;
        e.last_frame_type = h.frame_type.clone();
        e.last_mcs = h.mcs;
        e.last_dst = h.dst.clone();
        *e.frame_types.entry(h.frame_type.clone()).or_default() += 1;
        e.modes.insert(h.mode.clone());
        e
    }

    /// Entries last heard at or after `since` (Unix microseconds), most
    /// recent first, at most `limit`.
    pub fn list(&self, since: Option<i64>, limit: Option<usize>) -> Vec<&HeardEntry> {
        let mut v: Vec<&HeardEntry> = self.entries.values().filter(|e| since.is_none_or(|s| e.last_us >= s)).collect();
        v.sort_by(|a, b| b.last_us.cmp(&a.last_us).then_with(|| a.address.cmp(&b.address)));
        v.truncate(limit.unwrap_or(usize::MAX));
        v
    }

    /// One station's entry.
    pub fn get(&self, address: &str) -> Option<&HeardEntry> {
        self.entries.get(address)
    }

    /// Stations in the list.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No stations.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Changed since the last save or load.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Write the list to `path` (a temporary file renamed over it).
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        let mut m = Map::new();
        m.insert("format".into(), json!(HEARD_FORMAT));
        m.insert("stations".into(), Value::Array(self.list(None, None).iter().map(|e| e.to_json()).collect()));
        let text = serde_json::to_string_pretty(&Value::Object(m)).map_err(io::Error::other)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

    /// Read a list written by [`HeardList::save`]; a missing file is an
    /// empty list.
    pub fn load(path: &Path) -> io::Result<HeardList> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HeardList::new()),
            Err(e) => return Err(e),
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if v.get("format").and_then(Value::as_str) != Some(HEARD_FORMAT) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("not a {HEARD_FORMAT} file")));
        }
        let mut list = HeardList::new();
        for s in v.get("stations").and_then(Value::as_array).into_iter().flatten() {
            if let Some(e) = HeardEntry::from_json(s) {
                list.entries.insert(e.address.clone(), e);
            }
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(addr: &str, ftype: &str, snr: f64, t: i64) -> Heard {
        let (call, ssid) = addr.split_once('-').map_or((addr, 0), |(c, s)| (c, s.parse().unwrap()));
        Heard {
            address: addr.into(),
            call: call.into(),
            ssid,
            dst: "*".into(),
            frame_type: ftype.into(),
            mode: "cairn".into(),
            mcs: 2,
            width: 24,
            snr_db: snr,
            unix_us: t,
            modem_time: t as f64 / 1e6,
        }
    }

    #[test]
    fn every_header_updates_its_entry() {
        let mut l = HeardList::new();
        l.update(&h("N0AAA-1", "BEACON", 12.0, 1_000_000));
        l.update(&h("N0BBB", "UI", 20.0, 2_000_000));
        let e = l.update(&h("N0AAA-1", "UI", 9.5, 3_000_000)).clone();
        assert_eq!(e.count, 2);
        assert_eq!((e.first_us, e.last_us), (1_000_000, 3_000_000));
        assert_eq!((e.last_snr_db, e.best_snr_db), (9.5, 12.0));
        assert_eq!(e.frame_types.get("BEACON"), Some(&1));
        assert_eq!(e.frame_types.get("UI"), Some(&1));
        assert_eq!(e.last_frame_type, "UI");
        let all = l.list(None, None);
        assert_eq!(all.iter().map(|e| e.address.as_str()).collect::<Vec<_>>(), ["N0AAA-1", "N0BBB"]);
        assert_eq!(l.list(Some(2_500_000), None).len(), 1);
        assert_eq!(l.list(None, Some(1)).len(), 1);
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("cairn-heard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("heard.json");
        let mut l = HeardList::new();
        l.update(&h("N0AAA-1", "BEACON", 12.0, 1_000_000));
        l.update(&h("N0AAA-1", "DATA", 14.25, 2_000_000));
        assert!(l.dirty());
        l.save(&path).unwrap();
        assert!(!l.dirty());
        let back = HeardList::load(&path).unwrap();
        let e = back.get("N0AAA-1").unwrap();
        assert_eq!(e.count, 2);
        assert_eq!(e.last_modem_time, None);
        assert_eq!(e.best_snr_db, 14.25);
        assert_eq!(e.frame_types.len(), 2);
        assert!(HeardList::load(&dir.join("missing.json")).unwrap().is_empty());
        std::fs::write(&path, "{}").unwrap();
        assert!(HeardList::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
