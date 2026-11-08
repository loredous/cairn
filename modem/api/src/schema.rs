//! A small JSON Schema (draft 2020-12) validator, enough for
//! `docs/api.schema.json`: `$ref` to `#/$defs/...`, `type`, `enum`,
//! `const`, `properties`, `required`, `additionalProperties`, `items`,
//! `minItems`, `maxItems`, `minProperties`, `minimum`, `maximum`,
//! `exclusiveMinimum`, `minLength`, `maxLength`, `pattern`, `oneOf`,
//! `anyOf`, `allOf` and `not`. Annotations (`description`, `default`,
//! `title`) are ignored; any other keyword is an error, so the schema
//! cannot quietly use something this validator skips. The repository's
//! Python tests check the same schema with the `jsonschema` package.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use regex_lite::Regex;
use serde_json::Value;

/// A loaded schema.
pub struct Schema {
    root: Value,
    patterns: Mutex<HashMap<String, Regex>>,
}

const ANNOTATIONS: [&str; 6] = ["description", "default", "title", "$schema", "$id", "$defs"];

impl Schema {
    /// From a parsed document.
    pub fn new(root: Value) -> Self {
        Schema { root, patterns: Mutex::new(HashMap::new()) }
    }

    /// From a file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Schema::new(v))
    }

    /// The repository's `docs/api.schema.json`, found from this crate's
    /// directory (for tests).
    pub fn api() -> Self {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api.schema.json");
        Schema::load(&p).expect("docs/api.schema.json")
    }

    /// Validate against the root schema.
    pub fn validate(&self, v: &Value) -> Result<(), String> {
        self.check(&self.root, v, "$")
    }

    /// Validate against `#/$defs/<name>`.
    pub fn validate_def(&self, name: &str, v: &Value) -> Result<(), String> {
        let s = self.root.get("$defs").and_then(|d| d.get(name)).ok_or_else(|| format!("no $defs/{name}"))?;
        self.check(s, v, "$")
    }

    /// Validate one line of JSON (request, response or event).
    pub fn validate_line(&self, line: &str) -> Result<(), String> {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}: {line}"))?;
        self.validate(&v).map_err(|e| format!("{e}\n  in: {line}"))
    }

    fn resolve<'a>(&'a self, r: &str) -> Result<&'a Value, String> {
        let name = r.strip_prefix("#/$defs/").ok_or_else(|| format!("unsupported $ref {r}"))?;
        self.root.get("$defs").and_then(|d| d.get(name)).ok_or_else(|| format!("unresolved $ref {r}"))
    }

    fn pattern_ok(&self, pat: &str, s: &str) -> Result<bool, String> {
        let mut cache = self.patterns.lock().expect("patterns");
        if !cache.contains_key(pat) {
            let re = Regex::new(pat).map_err(|e| format!("bad pattern {pat}: {e}"))?;
            cache.insert(pat.to_string(), re);
        }
        Ok(cache[pat].is_match(s))
    }

    fn check(&self, schema: &Value, v: &Value, path: &str) -> Result<(), String> {
        let obj = match schema {
            Value::Bool(true) => return Ok(()),
            Value::Bool(false) => return Err(format!("{path}: no value allowed")),
            Value::Object(o) => o,
            _ => return Err(format!("{path}: schema is not an object")),
        };
        for (k, s) in obj {
            if ANNOTATIONS.contains(&k.as_str()) {
                continue;
            }
            match k.as_str() {
                "$ref" => self.check(self.resolve(s.as_str().unwrap_or(""))?, v, path)?,
                "type" => {
                    let ok = match s {
                        Value::String(t) => type_ok(t, v),
                        Value::Array(ts) => ts.iter().any(|t| t.as_str().is_some_and(|t| type_ok(t, v))),
                        _ => false,
                    };
                    if !ok {
                        return Err(format!("{path}: {v} is not of type {s}"));
                    }
                }
                "enum" => {
                    if !s.as_array().is_some_and(|a| a.iter().any(|e| json_eq(e, v))) {
                        return Err(format!("{path}: {v} is not one of {s}"));
                    }
                }
                "const" => {
                    if !json_eq(s, v) {
                        return Err(format!("{path}: {v} is not {s}"));
                    }
                }
                "properties" => {
                    if let (Some(props), Some(o)) = (s.as_object(), v.as_object()) {
                        for (name, ps) in props {
                            if let Some(x) = o.get(name) {
                                self.check(ps, x, &format!("{path}.{name}"))?;
                            }
                        }
                    }
                }
                "required" => {
                    if let Some(o) = v.as_object() {
                        for r in s.as_array().into_iter().flatten().filter_map(Value::as_str) {
                            if !o.contains_key(r) {
                                return Err(format!("{path}: missing member {r}"));
                            }
                        }
                    }
                }
                "additionalProperties" => {
                    if let Some(o) = v.as_object() {
                        let known = obj.get("properties").and_then(Value::as_object);
                        for (name, x) in o {
                            if known.is_some_and(|p| p.contains_key(name)) {
                                continue;
                            }
                            self.check(s, x, &format!("{path}.{name}"))?;
                        }
                    }
                }
                "items" => {
                    if let Some(a) = v.as_array() {
                        for (i, x) in a.iter().enumerate() {
                            self.check(s, x, &format!("{path}[{i}]"))?;
                        }
                    }
                }
                "minItems" | "maxItems" => {
                    if let Some(a) = v.as_array() {
                        let n = s.as_u64().unwrap_or(0) as usize;
                        if (k == "minItems" && a.len() < n) || (k == "maxItems" && a.len() > n) {
                            return Err(format!("{path}: {} items ({k} {n})", a.len()));
                        }
                    }
                }
                "minProperties" => {
                    if let Some(o) = v.as_object() {
                        if (o.len() as u64) < s.as_u64().unwrap_or(0) {
                            return Err(format!("{path}: fewer than {s} members"));
                        }
                    }
                }
                "minimum" | "maximum" | "exclusiveMinimum" => {
                    if let (Some(x), Some(lim)) = (v.as_f64(), s.as_f64()) {
                        let bad = match k.as_str() {
                            "minimum" => x < lim,
                            "maximum" => x > lim,
                            _ => x <= lim,
                        };
                        if bad {
                            return Err(format!("{path}: {x} violates {k} {lim}"));
                        }
                    }
                }
                "minLength" | "maxLength" => {
                    if let Some(t) = v.as_str() {
                        let n = s.as_u64().unwrap_or(0) as usize;
                        let len = t.chars().count();
                        if (k == "minLength" && len < n) || (k == "maxLength" && len > n) {
                            return Err(format!("{path}: length {len} violates {k} {n}"));
                        }
                    }
                }
                "pattern" => {
                    if let (Some(t), Some(p)) = (v.as_str(), s.as_str()) {
                        if !self.pattern_ok(p, t)? {
                            return Err(format!("{path}: {t:?} does not match {p}"));
                        }
                    }
                }
                "oneOf" | "anyOf" => {
                    let subs = s.as_array().ok_or_else(|| format!("{path}: {k} is not an array"))?;
                    let mut errs = Vec::new();
                    let mut n = 0;
                    for sub in subs {
                        match self.check(sub, v, path) {
                            Ok(()) => n += 1,
                            Err(e) => errs.push(e),
                        }
                    }
                    if n == 0 {
                        return Err(format!("{path}: matches none of {k}: [{}]", errs.join(" | ")));
                    }
                    if k == "oneOf" && n > 1 {
                        return Err(format!("{path}: matches {n} branches of oneOf"));
                    }
                }
                "allOf" => {
                    for sub in s.as_array().into_iter().flatten() {
                        self.check(sub, v, path)?;
                    }
                }
                "not" => {
                    if self.check(s, v, path).is_ok() {
                        return Err(format!("{path}: matches a schema it must not ({s})"));
                    }
                }
                other => return Err(format!("{path}: unsupported schema keyword {other}")),
            }
        }
        Ok(())
    }
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|x| x.fract() == 0.0),
        _ => false,
    }
}

/// JSON equality with numbers compared by value (1 == 1.0).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| json_eq(p, q)))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_api_schema_loads_and_checks() {
        let s = Schema::api();
        s.validate(&json!({"id": 1, "cmd": "status"})).unwrap();
        s.validate(&json!({"cmd": "send", "text": "hi", "priority": 3})).unwrap();
        assert!(s.validate(&json!({"cmd": "send", "text": "hi", "data": "aGk="})).is_err());
        assert!(s.validate(&json!({"cmd": "connect"})).is_err());
        s.validate(&json!({"id": null, "ok": false, "cmd": null, "error": {"code": "bad_json", "message": "x"}}))
            .unwrap();
        assert!(
            s.validate(&json!({"id": 1, "ok": false, "cmd": "x", "error": {"code": "nope", "message": ""}})).is_err()
        );
        assert!(
            s.validate(&json!({"event": "dcd", "time": "2026-11-07T18:00:00Z", "modem_time": 1.0, "raw": true,
                "detectors": {"quieting": true, "preamble": false, "afsk": false, "cos": false}, "at": 1.0}))
                .is_err(),
            "time without milliseconds"
        );
    }

    #[test]
    fn unknown_keywords_are_errors() {
        let s = Schema::new(json!({"type": "object", "if": {}}));
        assert!(s.validate(&json!({})).is_err());
    }
}
