//! Tolerant JSON parsing, Go-compatible canonical rendering, and redaction.
//!
//! The Go implementation parses every number as `float64` and renders through
//! `encoding/json`, so `1` and `1.0` are the same value and print as `1`.
//! [`parse`] and the renderers here reproduce that: equality, LCS hashing, and
//! keyed-array identity all depend on it.

use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

/// Errors from [`parse`].
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    /// The document was empty after trimming the BOM and whitespace.
    #[error("empty document")]
    Empty,
    /// The document is not valid JSON.
    #[error("parse: {0}")]
    Parse(#[source] serde_json::Error),
}

/// Parses JSON tolerantly: strips a UTF-8 BOM and surrounding whitespace, and
/// normalizes every number to the `f64` semantics Go uses (see [`normalize`]).
///
/// # Errors
/// [`JsonError::Empty`] for a blank document, [`JsonError::Parse`] otherwise.
pub fn parse(bytes: &[u8]) -> Result<Value, JsonError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let bytes = trim_ascii(bytes);
    if bytes.is_empty() {
        return Err(JsonError::Empty);
    }
    let mut v: Value = serde_json::from_slice(bytes).map_err(JsonError::Parse)?;
    normalize(&mut v);
    Ok(v)
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &b[start..end]
}

/// Rewrites every number in place so that values Go would consider equal
/// compare equal here: each number goes through `f64`, and an integral value
/// that fits exactly is stored as an integer (`1.0` → `1`).
pub fn normalize(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                *n = go_number(f);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        Value::Object(m) => m.values_mut().for_each(normalize),
        _ => {}
    }
}

/// The `Number` Go's `float64` round-trips to: an `i64` when the value is
/// integral and exactly representable, else the float itself.
fn go_number(f: f64) -> Number {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if f.fract() == 0.0 && f.abs() < EXACT && !(f == 0.0 && f.is_sign_negative()) {
        // Truncation is exact: `f` is integral and below 2^53.
        Number::from(f as i64)
    } else {
        Number::from_f64(f).unwrap_or_else(|| Number::from(0))
    }
}

/// Formats a number the way Go's `encoding/json` does: shortest round-trip
/// digits, plain notation for `1e-6 <= |x| < 1e21`, otherwise exponent form
/// with an explicit sign; positive exponents are zero-padded to two digits
/// (`1e+21`) and negative ones are not (`1e-7`).
pub fn fmt_number(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{f:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        // encoding/json keeps strconv's two-digit positive exponents (`e+21`)
        // but strips the leading zero from small negative ones (`e-7`, not
        // `e-07`).
        return match exp.strip_prefix('-') {
            Some(d) => format!("{mantissa}e-{d}"),
            None => format!("{mantissa}e+{exp:0>2}"),
        };
    }
    format!("{f}")
}

/// Deterministic one-line serialization with sorted keys and Go-style
/// numbers; the hash for array elements and identity values.
pub fn canonical(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, None, 0);
    out
}

/// One-line rendering for diff rows and table cells (same as [`canonical`]).
pub fn compact(v: &Value) -> String {
    canonical(v)
}

/// Two-space indented rendering for display and export.
pub fn pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, Some(2), 0);
    out
}

fn write_value(out: &mut String, v: &Value, indent: Option<usize>, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&fmt_number(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            // serde_json's Map is a BTreeMap (no `preserve_order`), so this
            // iteration is already key-sorted.
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(w) = indent {
        out.push('\n');
        let _ = write!(out, "{:width$}", "", width = w * depth);
    }
}

fn write_string(out: &mut String, s: &str) {
    // serde_json escapes exactly the characters JSON requires.
    match serde_json::to_string(s) {
        Ok(q) => out.push_str(&q),
        Err(_) => out.push_str("\"\""),
    }
}

/// Returns a copy with the value of every field whose name is in `fields`
/// (case-insensitive) replaced by `«redacted»`, recursively.
pub fn redacted(v: &Value, fields: &[&str]) -> Value {
    fn redact(v: &Value, fields: &[String]) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, val)| {
                        let hit = fields.iter().any(|f| f.eq_ignore_ascii_case(k));
                        let out = if hit {
                            Value::String("«redacted»".into())
                        } else {
                            redact(val, fields)
                        };
                        (k.clone(), out)
                    })
                    .collect::<Map<String, Value>>(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|i| redact(i, fields)).collect()),
            other => other.clone(),
        }
    }
    let fields: Vec<String> = fields.iter().map(|f| f.to_ascii_lowercase()).collect();
    redact(v, &fields)
}

/// The JSON type name Go's diff engine uses: `null`, `bool`, `number`,
/// `string`, `object`, or `array`.
pub fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integral_floats_parse_equal_to_integers() {
        assert_eq!(parse(b"1").unwrap(), parse(b"1.0").unwrap());
        assert_eq!(parse(b"[1, 2.0]").unwrap(), parse(b"[1.0, 2]").unwrap());
        assert_eq!(
            parse(br#"{"a": 1e2}"#).unwrap(),
            parse(br#"{"a": 100}"#).unwrap()
        );
    }

    #[test]
    fn parse_strips_bom_and_whitespace() {
        let with_bom = [0xEF, 0xBB, 0xBF, b' ', b'{', b'}', b'\n'];
        assert_eq!(parse(&with_bom).unwrap(), json!({}));
    }

    #[test]
    fn parse_rejects_empty_and_invalid_documents() {
        assert!(matches!(parse(b"   \n"), Err(JsonError::Empty)));
        assert!(matches!(parse(b"{nope"), Err(JsonError::Parse(_))));
    }

    #[test]
    fn canonical_formats_numbers_like_go_encoding_json() {
        assert_eq!(canonical(&json!(1.0)), "1");
        assert_eq!(canonical(&json!(100)), "100");
        assert_eq!(canonical(&json!(0.123456)), "0.123456");
        assert_eq!(canonical(&json!(-0.0)), "-0");
        assert_eq!(canonical(&json!(0.000001)), "0.000001");
        // encoding/json strips the zero-pad strconv adds to small negative
        // exponents: 1e-07 → 1e-7. Positive exponents keep two digits.
        assert_eq!(canonical(&json!(1e-7)), "1e-7");
        assert_eq!(canonical(&json!(2.5e-8)), "2.5e-8");
        assert_eq!(canonical(&json!(-1e-7)), "-1e-7");
        assert_eq!(canonical(&json!(1e-10)), "1e-10");
        assert_eq!(canonical(&json!(1e-100)), "1e-100");
        assert_eq!(canonical(&json!(1e21)), "1e+21");
        assert_eq!(
            canonical(&json!(123456789012345680000.0)),
            "123456789012345680000"
        );
        assert_eq!(canonical(&json!(1.5e300)), "1.5e+300");
    }

    #[test]
    fn canonical_sorts_object_keys_recursively_and_is_compact() {
        let v = json!({"b": 1, "a": [2, {"d": 3, "c": 4}], "s": "x\"y"});
        assert_eq!(canonical(&v), r#"{"a":[2,{"c":4,"d":3}],"b":1,"s":"x\"y"}"#);
        assert_eq!(canonical(&json!(null)), "null");
        assert_eq!(canonical(&json!([])), "[]");
        assert_eq!(canonical(&json!({})), "{}");
    }

    #[test]
    fn compact_is_canonical_and_pretty_uses_two_space_indent() {
        let v = parse(br#"{"z": [1, 2.0], "a": {"k": true}}"#).unwrap();
        assert_eq!(compact(&v), r#"{"a":{"k":true},"z":[1,2]}"#);
        assert_eq!(
            pretty(&v),
            "{\n  \"a\": {\n    \"k\": true\n  },\n  \"z\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(pretty(&json!([])), "[]");
        assert_eq!(pretty(&json!({})), "{}");
    }

    #[test]
    fn redacted_replaces_named_fields_case_insensitively_and_recursively() {
        let v = json!({"Email": "a@b", "nested": [{"email": "c@d", "keep": 1}], "keep": "ok"});
        let r = redacted(&v, &["email"]);
        assert_eq!(
            r,
            json!({"Email": "«redacted»", "nested": [{"email": "«redacted»", "keep": 1}], "keep": "ok"})
        );
    }

    #[test]
    fn json_type_names_match_go() {
        assert_eq!(json_type(&json!(null)), "null");
        assert_eq!(json_type(&json!(true)), "bool");
        assert_eq!(json_type(&json!(1)), "number");
        assert_eq!(json_type(&json!("s")), "string");
        assert_eq!(json_type(&json!({})), "object");
        assert_eq!(json_type(&json!([])), "array");
    }
}
