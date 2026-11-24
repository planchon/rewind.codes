//! Canonical JSON: RFC 8785, the JSON Canonicalization Scheme (JCS).
//!
//! A payload's digest is the SHA-256 of its canonical form, and digests are compared during
//! replays for as long as a run lives. So the canonical form is part of the stored format and
//! leaves nothing to a serializer:
//!
//! - object keys are sorted by their UTF-16 code units;
//! - numbers are IEEE 754 doubles, written as ECMAScript's `Number.prototype.toString` writes
//!   them;
//! - strings use the minimal escaping of RFC 8785 §3.2.2.2;
//! - there is no whitespace.

use std::cmp::Ordering;
use std::fmt::Write as _;

use serde_json::{Number, Value};

/// The canonical form of `value` (RFC 8785).
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(number, out),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| utf16_order(a, b));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(value, out);
            }
            out.push('}');
        }
    }
}

/// The value with the keys of every object in canonical order: the value its canonical JSON
/// parses back to.
///
/// What a code observes of a value must not depend on where it comes from. A value
/// delivered live (as an action or the harness built it) and the same value loaded back from
/// storage (written in canonical form) must have their keys in the same order, or
/// `Object.keys` would differ between a run and its replay.
pub fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| utf16_order(a, b));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonicalize(value)))
                    .collect(),
            )
        }
        scalar => scalar.clone(),
    }
}

/// RFC 8785 §3.2.3: keys are compared as arrays of UTF-16 code units.
fn utf16_order(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// RFC 8785 §3.2.2.2: only `"`, `\` and control characters are escaped.
fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// RFC 8785 §3.2.2.3: every number is an IEEE 754 double. Integers that a double cannot
/// represent exactly are rounded, as a JavaScript `JSON.parse` would round them.
fn write_number(number: &Number, out: &mut String) {
    // `as_f64` rounds integers to the nearest double, ties to even, as JavaScript does.
    write_ecmascript_number(number.as_f64().unwrap_or(f64::NAN), out);
}

/// ECMAScript `Number::toString(x)` (ECMA-262 §6.1.6.1.20) for a finite `x`: the shortest
/// digits that round-trip to `x`, closest to `x`, ties to even, laid out as ECMAScript does
/// (`1e+21`, `0.000001`, `1e-7`).
///
/// `serde_json` never holds a non-finite number; should one reach this function, it is written
/// as `null`, like `JSON.stringify` does.
pub(crate) fn write_ecmascript_number(x: f64, out: &mut String) {
    if x.is_finite() {
        out.push_str(ryu_js::Buffer::new().format_finite(x));
    } else {
        out.push_str("null");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn number(bits: u64) -> String {
        let mut out = String::new();
        write_ecmascript_number(f64::from_bits(bits), &mut out);
        out
    }

    /// RFC 8785, Appendix B.
    #[test]
    fn numbers_follow_the_rfc_8785_vectors() {
        let vectors: &[(u64, &str)] = &[
            (0x0000000000000000, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x8000000000000001, "-5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0xffefffffffffffff, "-1.7976931348623157e+308"),
            (0x4340000000000000, "9007199254740992"),
            (0xc340000000000000, "-9007199254740992"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af5, "9.999999999999997e+22"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
            (0x444b1ae4d6e2ef4e, "999999999999999700000"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x41b3de4355555553, "333333333.3333332"),
            (0x41b3de4355555554, "333333333.33333325"),
            (0x41b3de4355555555, "333333333.3333333"),
            (0x41b3de4355555556, "333333333.3333334"),
            (0x41b3de4355555557, "333333333.33333343"),
            (0xbecbf647612f3696, "-0.0000033333333333333333"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ];
        for (bits, expected) in vectors {
            assert_eq!(number(*bits), *expected, "bits {bits:#018x}");
        }
    }

    #[test]
    fn integers_are_doubles() {
        assert_eq!(canonical_json(&json!(42)), "42");
        assert_eq!(canonical_json(&json!(-7)), "-7");
        assert_eq!(canonical_json(&json!(1.0)), "1");
        assert_eq!(canonical_json(&json!(4.50)), "4.5");
        // Above 2^53, integers are rounded like JSON.parse rounds them.
        assert_eq!(
            canonical_json(&json!(9_007_199_254_740_993_u64)),
            "9007199254740992"
        );
        assert_eq!(canonical_json(&json!(u64::MAX)), "18446744073709552000");
        assert_eq!(canonical_json(&json!(i64::MIN)), "-9223372036854776000");
    }

    /// RFC 8785 §3.2.2: the sample of the specification.
    #[test]
    fn canonicalizes_the_rfc_sample() {
        let input = r#"{
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#;
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            canonical_json(&value),
            r#"{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\\\"/"}"#
        );
    }

    /// RFC 8785 §3.2.3: keys are sorted by UTF-16 code units, which is not the order of
    /// UTF-8 bytes once characters outside the BMP are involved.
    #[test]
    fn sorts_keys_by_utf16_code_units() {
        let input = r#"{
            "€": "Euro Sign",
            "\r": "Carriage Return",
            "דּ": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "😀": "Emoji: Grinning Face",
            "\u0080": "Control",
            "ö": "Latin Small Letter O With Diaeresis"
        }"#;
        let value: Value = serde_json::from_str(input).unwrap();
        let canonical = canonical_json(&value);
        let order: Vec<&str> = [
            "Carriage Return",
            "One",
            "Control",
            "Latin Small Letter O With Diaeresis",
            "Euro Sign",
            "Emoji: Grinning Face",
            "Hebrew Letter Dalet With Dagesh",
        ]
        .into_iter()
        .collect();
        let positions: Vec<usize> = order
            .iter()
            .map(|name| canonical.find(name).unwrap())
            .collect();
        assert!(positions.is_sorted(), "{canonical}");
    }

    #[test]
    fn escapes_only_what_rfc_8785_escapes() {
        let value = json!("\u{0}\u{1f}\u{7f}\u{2028}é/<>\"\\\u{8}\u{c}");
        assert_eq!(
            canonical_json(&value),
            "\"\\u0000\\u001f\u{7f}\u{2028}é/<>\\\"\\\\\\b\\f\""
        );
    }

    #[test]
    fn canonicalized_values_have_the_key_order_of_their_canonical_json() {
        let value: Value =
            serde_json::from_str(r#"{"z":1,"a":{"y":[{"d":1,"c":2}],"b":true}}"#).unwrap();
        let canonical = canonicalize(&value);
        assert_eq!(canonical, value, "the same value");
        let reparsed: Value = serde_json::from_str(&canonical_json(&value)).unwrap();
        assert_eq!(
            serde_json::to_string(&canonical).unwrap(),
            serde_json::to_string(&reparsed).unwrap(),
            "the same key order as the value parsed back from its canonical JSON"
        );
        assert_eq!(canonical_json(&canonical), canonical_json(&value));
    }

    #[test]
    fn nests_without_whitespace() {
        let value = json!({ "b": [1, { "d": null, "c": true }], "a": {} });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{},"b":[1,{"c":true,"d":null}]}"#
        );
    }
}
