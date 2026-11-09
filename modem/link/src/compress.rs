//! PDU content encoding (SPEC §12.2; ADR 0014 `zstd-frame-format`): zstd
//! bodies for encodings 1 (no dictionary) and 2 (with a dictionary), the
//! dictionary set, and the sender's choice of encoding.
//!
//! A body of encoding 1 or 2 is exactly one standard zstd frame (RFC 8878):
//!
//! | Frame element | Sent | Accepted |
//! | --- | --- | --- |
//! | Magic number `28 B5 2F FD` | always | required ([`CodecError::MissingMagic`]) |
//! | Content checksum | never (the PDU's CRC-32 covers the decoded body) | yes, checked by libzstd |
//! | In-frame Dictionary_ID | never (encoding 2 names the dictionary in the PDU) | yes, ignored: the PDU's Dictionary ID decides |
//! | Frame_Content_Size | never | yes; a size above the bound is refused before decoding |
//! | Bytes after the frame | never | refused ([`CodecError::TrailingBytes`], the ADR's MAY) |
//!
//! A receiver bounds every decoded body by [`DECODED_BODY_MAX`], the largest
//! body a raw PDU can carry, and discards one that exceeds it or fails the
//! PDU's CRC-32. The dictionary for encoding 2 is the published file, used
//! unchanged through libzstd's dictionary loader; its Dictionary ID is the
//! first four bytes of the file's SHA-256 (SPEC §12.1), and on the air in a
//! B.3 block the same four bytes read as a big-endian 32-bit number.
//!
//! Compression needs the `zstd` feature (libzstd through the `zstd` crate).
//! Without it [`Codec`] sends raw only and delivers compressed bodies
//! undecoded ([`CodecError::Unavailable`]); frame headers and dictionary IDs
//! work either way.

#[cfg(feature = "zstd")]
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::params::{PDU_CRC_BYTES, PDU_FIXED_BYTES, PDU_MAX_LENGTH};
use crate::pdu::{Pdu, crc32, encoding};

/// The zstd frame magic number, 0xFD2FB528 little-endian (RFC 8878 §3.1.1).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The largest decoded body a receiver accepts: the largest body a raw PDU
/// can carry, 65,535 - 3 - 4 = 65,528 bytes (SPEC §12.1: the 16-bit Length
/// counts Type, Stream, Encoding, the body and the CRC-32). A sender
/// compresses only what it could have sent raw, so a body that decodes to
/// more than this is discarded (ADR 0014: receivers MUST bound the
/// decompressed size).
pub const DECODED_BODY_MAX: usize = PDU_MAX_LENGTH - PDU_FIXED_BYTES - PDU_CRC_BYTES;

/// The compression level this modem sends short bodies at (the level is
/// the sender's choice and is not on the air, ADR 0014); the dictionary
/// evaluation measured at 19. A 300-byte message takes well under a
/// millisecond.
pub const DEFAULT_LEVEL: i32 = 19;

/// Bodies longer than this go at [`BULK_LEVEL`].
pub const BULK_ABOVE: usize = 1024;

/// The level for bodies above [`BULK_ABOVE`] (file chunks): about four
/// times faster than 19 on 2 kB chunks for about 5% more bytes, so a file
/// send does not hold the modem's event loop for long.
pub const BULK_LEVEL: i32 = 9;

/// A Dictionary ID: the first four bytes of the dictionary file's SHA-256,
/// in digest order (SPEC §12.1).
pub type DictionaryId = [u8; 4];

/// The B.3 form of a Dictionary ID (the four bytes, big-endian).
pub fn id_to_u32(id: DictionaryId) -> u32 {
    u32::from_be_bytes(id)
}

/// The Dictionary ID of a B.3 dictionary slot.
pub fn id_from_u32(v: u32) -> DictionaryId {
    v.to_be_bytes()
}

/// A Dictionary ID as 8 lower-case hex digits.
pub fn id_hex(id: DictionaryId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse 8 hex digits into a Dictionary ID.
pub fn id_parse(s: &str) -> Option<DictionaryId> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(id_from_u32(v))
}

/// Why a body could not be encoded or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Built without the `zstd` feature: encodings 1 and 2 are not available.
    Unavailable,
    /// An encoding this modem does not implement (3, schema-encoded, is v2).
    UnknownEncoding(u8),
    /// Encoding 2 names a dictionary this station does not have (or, on
    /// send, one the session did not negotiate).
    UnknownDictionary(DictionaryId),
    /// The body does not start with the zstd magic number.
    MissingMagic,
    /// The frame header or a block is malformed, or libzstd refused it.
    Malformed(String),
    /// Bytes follow the frame (skippable or concatenated frames are not used).
    TrailingBytes(usize),
    /// The decoded body would exceed the bound.
    TooLarge {
        /// The bound, bytes.
        bound: usize,
    },
    /// The decoded body fails the PDU's CRC-32 (a dictionary mismatch fails here).
    CrcMismatch,
    /// A compressed PDU without the CRC-32 of its decoded body.
    MissingCrc,
}

impl CodecError {
    /// A body that fails this way MUST be discarded (ADR 0014); the others
    /// are bodies this station cannot decode, which may still be delivered
    /// undecoded.
    pub fn must_discard(&self) -> bool {
        !matches!(self, CodecError::Unavailable | CodecError::UnknownEncoding(_) | CodecError::UnknownDictionary(_))
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Unavailable => write!(f, "built without zstd: encodings 1 and 2 are not available"),
            CodecError::UnknownEncoding(e) => write!(f, "encoding {e} is not implemented"),
            CodecError::UnknownDictionary(id) => write!(f, "dictionary {} is not available", id_hex(*id)),
            CodecError::MissingMagic => write!(f, "the body does not start with the zstd magic number"),
            CodecError::Malformed(m) => write!(f, "malformed zstd frame: {m}"),
            CodecError::TrailingBytes(n) => write!(f, "{n} bytes after the zstd frame"),
            CodecError::TooLarge { bound } => write!(f, "the decoded body exceeds {bound} bytes"),
            CodecError::CrcMismatch => write!(f, "the decoded body fails the PDU CRC-32"),
            CodecError::MissingCrc => write!(f, "a compressed PDU needs the CRC-32 of its decoded body"),
        }
    }
}

impl std::error::Error for CodecError {}

// --- Frame header (RFC 8878 §3.1.1.1) -------------------------------------------------

/// What a zstd frame header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Bytes of the header, magic number included.
    pub len: usize,
    /// Frame_Header_Descriptor byte.
    pub descriptor: u8,
    /// Single_Segment_flag (no Window_Descriptor; the content size is the window).
    pub single_segment: bool,
    /// Content_Checksum_flag.
    pub checksum: bool,
    /// Dictionary_ID, when the frame carries one (a zero ID counts as one too).
    pub dictionary_id: Option<u32>,
    /// Bytes of the Dictionary_ID field (0, 1, 2 or 4).
    pub dictionary_id_bytes: usize,
    /// Frame_Content_Size, when present.
    pub content_size: Option<u64>,
    /// Window size from the Window_Descriptor, when present.
    pub window_size: Option<u64>,
}

/// Parse the header of the zstd frame at the start of `body`.
pub fn frame_header(body: &[u8]) -> Result<FrameHeader, CodecError> {
    if body.len() < 4 || body[..4] != ZSTD_MAGIC {
        return Err(CodecError::MissingMagic);
    }
    let short = || CodecError::Malformed("frame header truncated".into());
    let fhd = *body.get(4).ok_or_else(short)?;
    if fhd & 0x08 != 0 {
        return Err(CodecError::Malformed("reserved bit set in the frame header descriptor".into()));
    }
    let fcs_flag = fhd >> 6;
    let single_segment = fhd & 0x20 != 0;
    let checksum = fhd & 0x04 != 0;
    let did_bytes = [0, 1, 2, 4][(fhd & 3) as usize];
    let fcs_bytes = match fcs_flag {
        0 => single_segment as usize,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut at = 5;
    let window_size = if single_segment {
        None
    } else {
        let wd = *body.get(at).ok_or_else(short)?;
        at += 1;
        let log = 10 + (wd >> 3) as u32;
        let base = 1u64 << log;
        Some(base + (base / 8) * (wd & 7) as u64)
    };
    let le = |b: &[u8]| b.iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64);
    let did = body.get(at..at + did_bytes).ok_or_else(short)?;
    let dictionary_id = (did_bytes > 0).then(|| le(did) as u32);
    at += did_bytes;
    let fcs = body.get(at..at + fcs_bytes).ok_or_else(short)?;
    let content_size = match fcs_bytes {
        0 => None,
        2 => Some(le(fcs) + 256),
        _ => Some(le(fcs)),
    };
    at += fcs_bytes;
    Ok(FrameHeader {
        len: at,
        descriptor: fhd,
        single_segment,
        checksum,
        dictionary_id,
        dictionary_id_bytes: did_bytes,
        content_size,
        window_size,
    })
}

/// The frame with any in-frame Dictionary_ID removed (flag cleared, field
/// dropped), so libzstd decodes with the dictionary the PDU names whatever
/// the frame says (ADR 0014: use the dictionary the PDU's Dictionary ID
/// names). The window, content size and checksum are untouched.
pub fn without_dictionary_id(body: &[u8], h: &FrameHeader) -> Vec<u8> {
    let did_at = 5 + (!h.single_segment) as usize;
    let mut v = Vec::with_capacity(body.len());
    v.extend_from_slice(&body[..4]);
    v.push(h.descriptor & !3);
    v.extend_from_slice(&body[5..did_at]);
    v.extend_from_slice(&body[did_at + h.dictionary_id_bytes..]);
    v
}

// --- Dictionaries ---------------------------------------------------------------------

/// A published dictionary file (SPEC §12.2), used unchanged.
#[derive(Clone)]
pub struct Dictionary {
    id: DictionaryId,
    sha256: [u8; 32],
    name: String,
    bytes: Arc<[u8]>,
}

impl fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dictionary")
            .field("id", &id_hex(self.id))
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Dictionary {
    /// A dictionary from its file's bytes; `name` is for people (the file name).
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Dictionary {
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        Dictionary { id: [sha256[0], sha256[1], sha256[2], sha256[3]], sha256, name: name.into(), bytes: bytes.into() }
    }

    /// Read a dictionary file.
    pub fn load(path: &Path) -> io::Result<Dictionary> {
        let bytes = std::fs::read(path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Dictionary::new(name, bytes))
    }

    /// Its Dictionary ID.
    pub fn id(&self) -> DictionaryId {
        self.id
    }

    /// Its SHA-256.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Its name (the file name it was loaded from).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The dictionaries a station has, in the order it prefers them.
#[derive(Clone, Debug, Default)]
pub struct Dictionaries {
    list: Vec<Dictionary>,
}

/// Dictionary file extension in a dictionary directory.
pub const DICTIONARY_EXTENSION: &str = "zdict";

impl Dictionaries {
    /// No dictionaries.
    pub fn new() -> Dictionaries {
        Dictionaries::default()
    }

    /// Every `*.zdict` file directly in `dir`, by file name. Returns the set
    /// and a note per file skipped (unreadable, or a duplicate of one
    /// already loaded).
    pub fn load_dir(dir: &Path) -> io::Result<(Dictionaries, Vec<String>)> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == DICTIONARY_EXTENSION))
            .collect();
        paths.sort();
        let mut set = Dictionaries::new();
        let mut notes = Vec::new();
        for p in paths {
            match Dictionary::load(&p) {
                Ok(d) => {
                    if let Some(prev) = set.get(d.id()) {
                        notes.push(format!("{}: same ID {} as {}; skipped", p.display(), id_hex(d.id()), prev.name()));
                    } else {
                        set.list.push(d);
                    }
                }
                Err(e) => notes.push(format!("{}: {e}", p.display())),
            }
        }
        Ok((set, notes))
    }

    /// Add a dictionary (replacing one with the same ID).
    pub fn insert(&mut self, d: Dictionary) {
        match self.list.iter_mut().find(|x| x.id == d.id) {
            Some(x) => *x = d,
            None => self.list.push(d),
        }
    }

    /// The dictionary with this ID.
    pub fn get(&self, id: DictionaryId) -> Option<&Dictionary> {
        self.list.iter().find(|d| d.id == id)
    }

    /// Every dictionary, in order.
    pub fn iter(&self) -> impl Iterator<Item = &Dictionary> {
        self.list.iter()
    }

    /// Their IDs in B.3 form, in order.
    pub fn ids_u32(&self) -> Vec<u32> {
        self.list.iter().map(|d| id_to_u32(d.id)).collect()
    }

    /// How many.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// None at all.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

// --- The codec ------------------------------------------------------------------------

/// The sender's encoding preference for one PDU (SPEC §13.2 `send`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preference {
    /// The smallest of raw, zstd and zstd with each usable dictionary.
    Auto,
    /// Encoding 0.
    Raw,
    /// Encoding 1, even when it is larger than raw.
    Zstd,
    /// Encoding 2 with this dictionary, or the best usable one.
    ZstdDict(Option<DictionaryId>),
}

#[cfg(feature = "zstd")]
mod lib {
    pub use zstd::zstd_safe::{CCtx, CParameter, DCtx, find_frame_compressed_size, get_error_name};
}

/// Encodes and decodes PDU bodies against a dictionary set, keeping one
/// libzstd context per dictionary (and level) so a dictionary is digested
/// once.
pub struct Codec {
    dicts: Dictionaries,
    level: i32,
    bulk_level: i32,
    #[cfg(feature = "zstd")]
    cctx: HashMap<(Option<DictionaryId>, i32), lib::CCtx<'static>>,
    #[cfg(feature = "zstd")]
    dctx: HashMap<Option<DictionaryId>, lib::DCtx<'static>>,
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codec")
            .field("dicts", &self.dicts)
            .field("level", &self.level)
            .field("bulk_level", &self.bulk_level)
            .finish()
    }
}

impl Clone for Codec {
    fn clone(&self) -> Codec {
        Codec { bulk_level: self.bulk_level, ..Codec::with_level(self.dicts.clone(), self.level) }
    }
}

impl Default for Codec {
    fn default() -> Codec {
        Codec::new(Dictionaries::new())
    }
}

impl Codec {
    /// Compression is built in (the `zstd` feature).
    pub const AVAILABLE: bool = cfg!(feature = "zstd");

    /// A codec over these dictionaries at [`DEFAULT_LEVEL`], and
    /// [`BULK_LEVEL`] above [`BULK_ABOVE`] bytes.
    pub fn new(dicts: Dictionaries) -> Codec {
        Codec { bulk_level: BULK_LEVEL, ..Codec::with_level(dicts, DEFAULT_LEVEL) }
    }

    /// A codec at one compression level (1-22) for every body.
    pub fn with_level(dicts: Dictionaries, level: i32) -> Codec {
        Codec {
            dicts,
            level,
            bulk_level: level,
            #[cfg(feature = "zstd")]
            cctx: HashMap::new(),
            #[cfg(feature = "zstd")]
            dctx: HashMap::new(),
        }
    }

    /// The dictionaries.
    pub fn dictionaries(&self) -> &Dictionaries {
        &self.dicts
    }

    /// The dictionary IDs to offer a peer in B.3: every dictionary when
    /// compression is built in, none otherwise.
    pub fn offered_ids(&self) -> Vec<u32> {
        if Codec::AVAILABLE { self.dicts.ids_u32() } else { Vec::new() }
    }

    /// Compress `data` into one zstd frame (encoding 1, or 2 with `dict`):
    /// magic number, no checksum, no in-frame dictionary ID, no content size.
    pub fn compress(&mut self, data: &[u8], dict: Option<DictionaryId>) -> Result<Vec<u8>, CodecError> {
        if let Some(id) = dict {
            if self.dicts.get(id).is_none() {
                return Err(CodecError::UnknownDictionary(id));
            }
        }
        #[cfg(feature = "zstd")]
        {
            use std::collections::hash_map::Entry;
            let level = if data.len() > BULK_ABOVE { self.bulk_level } else { self.level };
            let cctx = match self.cctx.entry((dict, level)) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let mut c = lib::CCtx::create();
                    let err = |c: usize| CodecError::Malformed(lib::get_error_name(c).to_string());
                    for p in [
                        lib::CParameter::CompressionLevel(level),
                        lib::CParameter::ChecksumFlag(false),
                        lib::CParameter::ContentSizeFlag(false),
                        lib::CParameter::DictIdFlag(false),
                    ] {
                        c.set_parameter(p).map_err(err)?;
                    }
                    if let Some(id) = dict {
                        let d = self.dicts.get(id).expect("checked above");
                        c.load_dictionary(d.bytes()).map_err(err)?;
                    }
                    e.insert(c)
                }
            };
            let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
            cctx.compress2(&mut out, data).map_err(|c| CodecError::Malformed(lib::get_error_name(c).to_string()))?;
            Ok(out)
        }
        #[cfg(not(feature = "zstd"))]
        {
            let _ = data;
            Err(CodecError::Unavailable)
        }
    }

    /// Decompress a body of encoding 1 (`dict` None) or 2, bounded by `bound`.
    pub fn decompress(&mut self, body: &[u8], dict: Option<DictionaryId>, bound: usize) -> Result<Vec<u8>, CodecError> {
        let h = frame_header(body)?;
        if let Some(id) = dict {
            if self.dicts.get(id).is_none() {
                return Err(CodecError::UnknownDictionary(id));
            }
        }
        if h.content_size.is_some_and(|n| n > bound as u64) {
            return Err(CodecError::TooLarge { bound });
        }
        #[cfg(feature = "zstd")]
        {
            use std::collections::hash_map::Entry;
            let frame_len = lib::find_frame_compressed_size(body)
                .map_err(|c| CodecError::Malformed(lib::get_error_name(c).to_string()))?;
            if frame_len < body.len() {
                return Err(CodecError::TrailingBytes(body.len() - frame_len));
            }
            let stripped;
            let frame = if h.dictionary_id.is_some() {
                stripped = without_dictionary_id(body, &h);
                &stripped[..]
            } else {
                body
            };
            let dctx = match self.dctx.entry(dict) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let mut c = lib::DCtx::create();
                    if let Some(id) = dict {
                        let d = self.dicts.get(id).expect("checked above");
                        c.load_dictionary(d.bytes())
                            .map_err(|c| CodecError::Malformed(lib::get_error_name(c).to_string()))?;
                    }
                    e.insert(c)
                }
            };
            // One byte over the bound tells "too large" from "fits exactly".
            let mut out = Vec::with_capacity(bound.saturating_add(1));
            match dctx.decompress(&mut out, frame) {
                Ok(n) if n > bound => Err(CodecError::TooLarge { bound }),
                Ok(_) => Ok(out),
                Err(c) => {
                    let name = lib::get_error_name(c);
                    if name.contains("too small") {
                        Err(CodecError::TooLarge { bound })
                    } else {
                        Err(CodecError::Malformed(name.to_string()))
                    }
                }
            }
        }
        #[cfg(not(feature = "zstd"))]
        {
            let _ = h;
            Err(CodecError::Unavailable)
        }
    }

    /// The decoded body of a received PDU, its CRC-32 checked. Raw bodies
    /// are checked too (the link layer has already done so).
    pub fn decode(&mut self, pdu: &Pdu) -> Result<Vec<u8>, CodecError> {
        let body = match pdu.encoding {
            encoding::RAW => pdu.body.clone(),
            encoding::ZSTD => self.decompress(&pdu.body, None, DECODED_BODY_MAX)?,
            encoding::ZSTD_DICT => {
                let id = pdu.dictionary_id.ok_or(CodecError::Malformed("encoding 2 without a Dictionary ID".into()))?;
                self.decompress(&pdu.body, Some(id), DECODED_BODY_MAX)?
            }
            e => return Err(CodecError::UnknownEncoding(e)),
        };
        let crc = pdu.crc.ok_or(CodecError::MissingCrc)?;
        if crc32(&body) != crc {
            return Err(CodecError::CrcMismatch);
        }
        Ok(body)
    }

    /// A PDU carrying `body` in the encoding `pref` asks for. `usable` are
    /// the dictionaries the receiver is known to have (a session's
    /// negotiated set; none for a broadcast): [`Preference::Auto`] tries
    /// those, [`Preference::ZstdDict`] must name one of them. `Auto` takes
    /// the smallest PDU, raw on a tie, and raw without the `zstd` feature.
    pub fn encode(
        &mut self,
        pdu_type: u8,
        stream: u8,
        body: Vec<u8>,
        pref: Preference,
        usable: &[DictionaryId],
    ) -> Result<Pdu, CodecError> {
        if body.len() > DECODED_BODY_MAX {
            return Err(CodecError::TooLarge { bound: DECODED_BODY_MAX });
        }
        let crc = crc32(&body);
        let make = |enc: u8, dict: Option<DictionaryId>, b: Vec<u8>| Pdu {
            pdu_type,
            stream,
            encoding: enc,
            dictionary_id: dict,
            body: b,
            crc: Some(crc),
        };
        let usable: Vec<DictionaryId> = usable.iter().copied().filter(|id| self.dicts.get(*id).is_some()).collect();
        match pref {
            Preference::Raw => Ok(make(encoding::RAW, None, body)),
            Preference::Zstd => {
                let z = self.compress(&body, None)?;
                Ok(make(encoding::ZSTD, None, z))
            }
            Preference::ZstdDict(Some(id)) => {
                if !usable.contains(&id) {
                    return Err(CodecError::UnknownDictionary(id));
                }
                let z = self.compress(&body, Some(id))?;
                Ok(make(encoding::ZSTD_DICT, Some(id), z))
            }
            Preference::ZstdDict(None) => {
                let mut best: Option<Pdu> = None;
                for id in usable {
                    let z = self.compress(&body, Some(id))?;
                    if best.as_ref().is_none_or(|b| z.len() < b.body.len()) {
                        best = Some(make(encoding::ZSTD_DICT, Some(id), z));
                    }
                }
                best.ok_or(CodecError::UnknownDictionary([0; 4]))
            }
            Preference::Auto => {
                let mut best = make(encoding::RAW, None, body.clone());
                if !Codec::AVAILABLE || body.is_empty() {
                    return Ok(best);
                }
                let mut consider = |p: Pdu| {
                    if p.wire_len() < best.wire_len() {
                        best = p;
                    }
                };
                consider(make(encoding::ZSTD, None, self.compress(&body, None)?));
                for id in usable {
                    consider(make(encoding::ZSTD_DICT, Some(id), self.compress(&body, Some(id))?));
                }
                Ok(best)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(fhd: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = ZSTD_MAGIC.to_vec();
        v.push(fhd);
        v.extend_from_slice(rest);
        v
    }

    #[test]
    fn header_fields() {
        // Window descriptor 0x00: 1 KiB window, no other field.
        let h = frame_header(&frame(0x00, &[0x00])).unwrap();
        assert_eq!(
            (h.len, h.window_size, h.checksum, h.dictionary_id, h.content_size),
            (6, Some(1024), false, None, None)
        );
        // Single segment with a 1-byte content size.
        let h = frame_header(&frame(0x20, &[200])).unwrap();
        assert_eq!((h.len, h.window_size, h.content_size), (6, None, Some(200)));
        // Checksum, 4-byte dictionary ID, 2-byte content size (+256).
        let h = frame_header(&frame(0x40 | 0x04 | 0x03, &[0x58, 0x44, 0x33, 0x22, 0x11, 0x10, 0x00])).unwrap();
        assert_eq!(h.window_size, Some(1 << 21));
        assert_eq!((h.checksum, h.dictionary_id, h.content_size, h.len), (true, Some(0x1122_3344), Some(256 + 16), 12));
        // Stripping the dictionary ID keeps the rest.
        let f = frame(0x40 | 0x03, &[0x58, 0x44, 0x33, 0x22, 0x11, 0x10, 0x00, 0xAA]);
        let s = without_dictionary_id(&f, &frame_header(&f).unwrap());
        assert_eq!(s, frame(0x40, &[0x58, 0x10, 0x00, 0xAA]));
    }

    #[test]
    fn header_rejections() {
        assert_eq!(frame_header(&[0x28, 0xB5, 0x2F]), Err(CodecError::MissingMagic));
        assert_eq!(frame_header(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]), Err(CodecError::MissingMagic)); // skippable
        assert!(matches!(frame_header(&frame(0x08, &[0])), Err(CodecError::Malformed(_))));
        assert!(matches!(frame_header(&frame(0xC0, &[0, 1, 2])), Err(CodecError::Malformed(_))));
    }

    #[test]
    fn ids() {
        let d = Dictionary::new("x", b"abc".to_vec());
        // SHA-256("abc") = ba7816bf...
        assert_eq!(d.id(), [0xba, 0x78, 0x16, 0xbf]);
        assert_eq!(id_to_u32(d.id()), 0xba78_16bf);
        assert_eq!(id_hex(d.id()), "ba7816bf");
        assert_eq!(id_parse("ba7816bf"), Some(d.id()));
        assert_eq!(id_parse("ba7816b"), None);
        assert_eq!(DECODED_BODY_MAX, 65_528);
    }

    #[test]
    fn raw_without_zstd_or_tiny() {
        let mut c = Codec::default();
        let p = c.encode(1, 0, Vec::new(), Preference::Auto, &[]).unwrap();
        assert_eq!((p.encoding, p.body.len()), (encoding::RAW, 0));
        assert_eq!(c.decode(&p).unwrap(), Vec::<u8>::new());
    }
}
