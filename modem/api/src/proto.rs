//! Protocol constants and message builders (`docs/api.md`).

use std::fmt;

use serde_json::{Map, Value, json};

/// Schema version (`hello` `api_version`).
pub const API_VERSION: &str = "0.1.0";

/// Every command, in the order `hello` lists them.
pub const COMMANDS: [&str; 11] = [
    "connect",
    "disconnect",
    "send",
    "beacon",
    "probe",
    "heard",
    "status",
    "config",
    "calibrate",
    "capture",
    "subscribe",
];

/// Every event name.
pub const EVENTS: [&str; 9] = ["hello", "rx", "session", "link", "heard", "channel", "dcd", "tx", "error"];

/// Longest request line, LF excluded.
pub const MAX_LINE: usize = 1 << 20;

/// Largest body of one PDU: the 16-bit Length counts type, stream,
/// encoding, the body and the CRC-32 (SPEC 12.1; 65,535 - 8).
pub const MAX_PDU_BODY: usize = 65_527;

/// Error codes (`docs/api.md` "Error codes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Code {
    BadJson,
    BadRequest,
    UnknownCommand,
    Unsupported,
    NoSession,
    Busy,
    InvalidState,
    NotFound,
    IoError,
    TooLarge,
    LineTooLong,
    ReadOnly,
    OutOfRange,
    Internal,
    SendRejected,
    BroadcastRejected,
    PduError,
    TxFailed,
    ReceiveClipping,
    Distortion,
    CaptureFailed,
    PttFault,
}

impl Code {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::BadJson => "bad_json",
            Code::BadRequest => "bad_request",
            Code::UnknownCommand => "unknown_command",
            Code::Unsupported => "unsupported",
            Code::NoSession => "no_session",
            Code::Busy => "busy",
            Code::InvalidState => "invalid_state",
            Code::NotFound => "not_found",
            Code::IoError => "io_error",
            Code::TooLarge => "too_large",
            Code::LineTooLong => "line_too_long",
            Code::ReadOnly => "read_only",
            Code::OutOfRange => "out_of_range",
            Code::Internal => "internal",
            Code::SendRejected => "send_rejected",
            Code::BroadcastRejected => "broadcast_rejected",
            Code::PduError => "pdu_error",
            Code::TxFailed => "tx_failed",
            Code::ReceiveClipping => "receive_clipping",
            Code::Distortion => "distortion",
            Code::CaptureFailed => "capture_failed",
            Code::PttFault => "ptt_fault",
        }
    }
}

/// An error answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// The code.
    pub code: Code,
    /// For people.
    pub message: String,
}

impl ApiError {
    /// An error.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        ApiError { code, message: message.into() }
    }

    /// `bad_request`.
    pub fn bad(message: impl Into<String>) -> Self {
        ApiError::new(Code::BadRequest, message)
    }

    /// The `error` object.
    pub fn to_json(&self) -> Value {
        json!({"code": self.code.as_str(), "message": self.message})
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

/// A parsed request.
#[derive(Clone, Debug)]
pub struct Request {
    /// `id` as sent (null when absent).
    pub id: Value,
    /// `cmd`.
    pub cmd: String,
    /// The whole object.
    pub args: Map<String, Value>,
}

/// Why a line is not a request: its `id` and `cmd` as far as they could
/// be read, and the error.
#[derive(Clone, Debug)]
pub struct Rejected {
    /// `id`, or null.
    pub id: Value,
    /// `cmd`, or null.
    pub cmd: Value,
    /// The error.
    pub error: ApiError,
}

impl Rejected {
    /// The error response.
    pub fn response(&self) -> Value {
        json!({"id": self.id, "ok": false, "cmd": self.cmd, "error": self.error.to_json()})
    }
}

/// Parse one request line (CR and surrounding white space already allowed).
pub fn parse_request(line: &str) -> Result<Request, Box<Rejected>> {
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| {
        Box::new(Rejected { id: Value::Null, cmd: Value::Null, error: ApiError::new(Code::BadJson, e.to_string()) })
    })?;
    let Value::Object(args) = v else {
        return Err(Box::new(Rejected {
            id: Value::Null,
            cmd: Value::Null,
            error: ApiError::new(Code::BadJson, "a request is a JSON object"),
        }));
    };
    let id = match args.get("id") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) if s.chars().count() <= 64 => Value::String(s.clone()),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Value::Number(n.clone()),
        Some(_) => {
            return Err(Box::new(Rejected {
                id: Value::Null,
                cmd: args.get("cmd").filter(|c| c.is_string()).cloned().unwrap_or(Value::Null),
                error: ApiError::bad("id must be a string of at most 64 characters, an integer or null"),
            }));
        }
    };
    let cmd = match args.get("cmd") {
        Some(Value::String(c)) => c.clone(),
        _ => {
            return Err(Box::new(Rejected {
                id,
                cmd: Value::Null,
                error: ApiError::bad("cmd (a string) is required"),
            }));
        }
    };
    Ok(Request { id, cmd, args })
}

impl Request {
    /// A response to this request.
    pub fn ok(&self, members: Map<String, Value>) -> Value {
        ok_response(&self.id, &self.cmd, members)
    }

    /// An error response to this request.
    pub fn err(&self, e: &ApiError) -> Value {
        json!({"id": self.id, "ok": false, "cmd": self.cmd, "error": e.to_json()})
    }

    /// A member, when present and not null.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.args.get(name).filter(|v| !v.is_null())
    }

    /// An optional boolean.
    pub fn opt_bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ApiError::bad(format!("{name} must be a boolean"))),
        }
    }

    /// An optional string.
    pub fn opt_str(&self, name: &str) -> Result<Option<&str>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(ApiError::bad(format!("{name} must be a string"))),
        }
    }

    /// A required string.
    pub fn req_str(&self, name: &str) -> Result<&str, ApiError> {
        self.opt_str(name)?.ok_or_else(|| ApiError::bad(format!("{name} is required")))
    }

    /// An optional integer within `lo..=hi`.
    pub fn opt_int(&self, name: &str, lo: i64, hi: i64) -> Result<Option<i64>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => int_in(v, name, lo, hi).map(Some),
        }
    }

    /// An optional number above zero.
    pub fn opt_positive(&self, name: &str) -> Result<Option<f64>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => match v.as_f64() {
                Some(x) if x > 0.0 && x.is_finite() => Ok(Some(x)),
                _ => Err(ApiError::bad(format!("{name} must be a number above 0"))),
            },
        }
    }
}

/// An integer member's value within `lo..=hi` (`bad_request` otherwise).
pub fn int_in(v: &Value, name: &str, lo: i64, hi: i64) -> Result<i64, ApiError> {
    match v.as_i64() {
        Some(x) if (lo..=hi).contains(&x) => Ok(x),
        _ => Err(ApiError::bad(format!("{name} must be an integer {lo}-{hi}"))),
    }
}

/// A successful response.
pub fn ok_response(id: &Value, cmd: &str, members: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.clone());
    m.insert("ok".into(), Value::Bool(true));
    m.insert("cmd".into(), Value::String(cmd.into()));
    m.extend(members);
    Value::Object(m)
}

/// An event: `event`, `time` (from Unix microseconds), `modem_time`, then
/// the members.
pub fn event(name: &str, unix_us: i64, modem_time: f64, members: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert("event".into(), Value::String(name.into()));
    m.insert("time".into(), Value::String(utc_string(unix_us)));
    m.insert("modem_time".into(), json!(round(modem_time.max(0.0), 3)));
    m.extend(members);
    Value::Object(m)
}

/// The members of a `json!` object (empty for anything else).
pub fn members(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `x` rounded to `digits` decimals (finite; NaN and infinities become 0).
pub fn round(x: f64, digits: i32) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let k = 10f64.powi(digits);
    (x * k).round() / k
}

// --- Time -----------------------------------------------------------------------

/// Microseconds since the Unix epoch, now.
pub fn now_unix_us() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 3339 UTC with milliseconds: `2026-11-07T18:04:05.123Z`.
pub fn utc_string(unix_us: i64) -> String {
    let ms = unix_us.div_euclid(1000);
    let secs = ms.div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", s / 3600, s / 60 % 60, s % 60, ms.rem_euclid(1000))
}

/// A compact UTC stamp for file names: `20261107T180405Z`.
pub fn utc_stamp(unix_us: i64) -> String {
    let t = utc_string(unix_us);
    format!("{}{}{}T{}{}{}Z", &t[0..4], &t[5..7], &t[8..10], &t[11..13], &t[14..16], &t[17..19])
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff]Z` to Unix microseconds.
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let rest = &s[19..];
    let (frac, z) = match rest.strip_prefix('.') {
        Some(r) => {
            let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
            (digits.clone(), &r[digits.len()..])
        }
        None => (String::new(), rest),
    };
    if z != "Z" {
        return None;
    }
    let us_frac = if frac.is_empty() {
        0
    } else {
        let f: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
        f.parse::<i64>().ok()?
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1_000_000) + us_frac)
}

// --- Base64 (RFC 4648 section 4) ------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Decode standard base64 (padding required, no white space).
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (i, q) in b.chunks(4).enumerate() {
        let last = i == b.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_encode(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
        assert!(base64_decode("Zg=").is_none());
        assert!(base64_decode("Zg==Zg==").is_none());
        assert!(base64_decode("Z!==").is_none());
    }

    #[test]
    fn utc_formats_and_parses() {
        assert_eq!(utc_string(0), "1970-01-01T00:00:00.000Z");
        // 2026-11-07T18:04:05.123Z
        let t = parse_utc("2026-11-07T18:04:05.123Z").unwrap();
        assert_eq!(utc_string(t), "2026-11-07T18:04:05.123Z");
        assert_eq!(utc_stamp(t), "20261107T180405Z");
        assert_eq!(parse_utc("2024-02-29T00:00:00Z").map(utc_string).unwrap(), "2024-02-29T00:00:00.000Z");
        assert!(parse_utc("2026-11-07 18:04:05Z").is_none());
        assert!(parse_utc("2026-13-07T18:04:05Z").is_none());
    }

    #[test]
    fn requests_parse_or_say_why_not() {
        let r = parse_request(r#"{"id":"a","cmd":"status"}"#).unwrap();
        assert_eq!(r.cmd, "status");
        assert_eq!(r.id, json!("a"));
        let e = parse_request("{nope").unwrap_err();
        assert_eq!(e.error.code, Code::BadJson);
        assert_eq!(parse_request("[1,2]").unwrap_err().error.code, Code::BadJson);
        let e = parse_request(r#"{"id":7}"#).unwrap_err();
        assert_eq!((e.error.code, e.id.clone()), (Code::BadRequest, json!(7)));
        let e = parse_request(r#"{"id":[1],"cmd":"x"}"#).unwrap_err();
        assert_eq!((e.error.code, e.cmd.clone()), (Code::BadRequest, json!("x")));
        let r = parse_request(r#"{"cmd":"send","stream":64,"graceful":"yes"}"#).unwrap();
        assert!(r.opt_int("stream", 0, 63).is_err());
        assert!(r.opt_bool("graceful").is_err());
        assert_eq!(r.opt_int("priority", 0, 3).unwrap(), None);
    }
}
