//! `JSON.stringify` of `serde_json` values, JavaScript property order, and numbers held as JavaScript holds them.
//!
//! `serde_json::to_string` differs from `JSON.stringify` on floats and key order, which this module fixes:
//! - integral floats keep a fraction (`1.0`, `-0.0`; JavaScript: `1`, `0`);
//! - floats in 1e16..1e21 and exactly 1e-6 use exponent form (`1e+16`, `1e-6`; JavaScript: `10000000000000000`,
//!   `0.000001`);
//! - integers beyond 2^53 print exactly (JavaScript rounds them to a double when parsing: `9007199254740993` is
//!   `9007199254740992`);
//! - object keys keep insertion order, where JavaScript lists array-index keys first, ascending.
//!
//! Strings escape identically, and so do all other numbers.

use serde_json::{Map, Number, Value};

/// `JSON.stringify(value)`.
pub fn stringify(value: &Value) -> String {
    stringify_indented(value, "")
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    stringify_indented(value, "  ")
}

/// `JSON.stringify(value, null, indent)`: `indent` (at most its first 10 characters, as JavaScript clamps the gap)
/// per level, `": "` between a key and its value, `[]` and `{}` for empty containers; an empty `indent` is compact.
pub fn stringify_indented(value: &Value, indent: &str) -> String {
    let gap: String = indent.chars().take(10).collect();
    let mut out = String::new();
    write_value(&mut out, value, &gap, 0);
    out
}

fn newline(out: &mut String, gap: &str, level: usize) {
    if !gap.is_empty() {
        out.push('\n');
        for _ in 0..level {
            out.push_str(gap);
        }
    }
}

fn write_value(out: &mut String, value: &Value, gap: &str, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&crate::number::json_to_string(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, gap, level + 1);
                write_value(out, item, gap, level + 1);
            }
            newline(out, gap, level);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in object_entries(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, gap, level + 1);
                write_string(out, key);
                out.push_str(if gap.is_empty() { ":" } else { ": " });
                write_value(out, item, gap, level + 1);
            }
            newline(out, gap, level);
            out.push('}');
        }
    }
}

/// A JSON string literal with the escapes `JSON.stringify` emits: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, other
/// controls as lowercase `\u00xx`; U+2028, U+2029 and DEL are written raw.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A canonical array index ("0" to "4294967294", no sign or leading zero): such keys enumerate first.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u32>().ok().filter(|n| *n != u32::MAX)
}

/// `Object.entries(object)` order: array-index keys ascending, then the other keys in insertion order. Needs
/// `serde_json`'s `preserve_order` (this crate enables it), else insertion order is lost.
pub fn object_entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut indexed: Vec<(u32, &String, &Value)> = map
        .iter()
        .filter_map(|(k, v)| array_index(k).map(|i| (i, k, v)))
        .collect();
    indexed.sort_by_key(|(i, _, _)| *i);
    let mut out: Vec<(&String, &Value)> = indexed.into_iter().map(|(_, k, v)| (k, v)).collect();
    out.extend(map.iter().filter(|(k, _)| array_index(k).is_none()));
    out
}

/// A JavaScript number as a JSON value: an integral value within ±2^53 becomes an integer (so `serde_json` writes
/// `3`, not `3.0`; `-0` becomes `0`), other finite values stay floats, and `NaN` / the infinities are `null`, as
/// `JSON.stringify` writes them.
pub fn number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(x as i64))
    } else {
        Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

/// Every integral float in `value` as an integer ([`number`]), as a `JSON.parse` / `JSON.stringify` round trip leaves
/// a value that `serde_json` then writes the same way (`-3e2` is `-300`, not `-300.0`).
pub fn normalize_numbers(value: Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => number(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, normalize_numbers(v))).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Literals printed by node 26.

    #[test]
    fn jsrt_json_numbers_serialize_like_json_stringify() {
        let input = "[0,-0,1,-1,0.1,0.30000000000000004,1e21,1e-7,1.5e-7,0.000001,123456789012345678901,9007199254740993,1.7976931348623157e308,5e-324,-2.5e-10,100,1234.5678,1e20,12345678901234567890,1e16,1.0]";
        let parsed: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            stringify(&parsed),
            "[0,0,1,-1,0.1,0.30000000000000004,1e+21,1e-7,1.5e-7,0.000001,123456789012345680000,9007199254740992,1.7976931348623157e+308,5e-324,-2.5e-10,100,1234.5678,100000000000000000000,12345678901234567000,10000000000000000,1]"
        );
    }

    #[test]
    fn jsrt_json_serde_differs_where_this_module_exists() {
        let value: Value =
            serde_json::from_str(r#"{"b":1,"2":2,"a":[1.0,-0.0,1e21,1e16,1e-6,1e-7,9007199254740993]}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"b":1,"2":2,"a":[1.0,-0.0,1e+21,1e+16,1e-6,1e-7,9007199254740993]}"#
        );
        assert_eq!(
            stringify(&value),
            r#"{"2":2,"b":1,"a":[1,0,1e+21,10000000000000000,0.000001,1e-7,9007199254740992]}"#
        );
    }

    #[test]
    fn jsrt_json_objects_and_strings_serialize_like_json_stringify() {
        let parsed: Value =
            serde_json::from_str(r#"{"b":1,"10":2,"2":3,"a":4,"01":5,"4294967295":6,"4294967294":7,"+1":8}"#).unwrap();
        assert_eq!(
            stringify(&parsed),
            r#"{"2":3,"10":2,"4294967294":7,"b":1,"a":4,"01":5,"4294967295":6,"+1":8}"#
        );
        let s = Value::from("\u{0}\u{8}\u{c}\n\r\t\u{1f}\u{7f}\"\\/\u{2028}é");
        let expected = "\"\\u0000\\b\\f\\n\\r\\t\\u001f\u{7f}\\\"\\\\/\u{2028}é\"";
        assert_eq!(stringify(&s), expected);
        assert_eq!(serde_json::to_string(&s).unwrap(), expected);
    }

    #[test]
    fn jsrt_json_indented_output_matches_json_stringify_with_a_gap() {
        let value = json!({"b": [], "c": {}, "d": [1, {"e": null}]});
        assert_eq!(
            stringify_indented(&value, "\t"),
            "{\n\t\"b\": [],\n\t\"c\": {},\n\t\"d\": [\n\t\t1,\n\t\t{\n\t\t\t\"e\": null\n\t\t}\n\t]\n}"
        );
        assert_eq!(stringify_pretty(&json!({"a": [1]})), "{\n  \"a\": [\n    1\n  ]\n}");
        assert_eq!(
            stringify_pretty(&json!({"a": [1]})),
            serde_json::to_string_pretty(&json!({"a": [1]})).unwrap()
        );
        assert_eq!(stringify_indented(&json!({"a": [1]}), ""), r#"{"a":[1]}"#);
        assert_eq!(stringify_indented(&json!([1]), "            "), "[\n          1\n]");
    }

    #[test]
    fn jsrt_json_number_keeps_integers_integral() {
        assert_eq!(number(3.0).to_string(), "3");
        assert_eq!(number(-0.0).to_string(), "0");
        assert_eq!(number(0.5).to_string(), "0.5");
        assert_eq!(number(f64::NAN), Value::Null);
        assert_eq!(number(f64::INFINITY), Value::Null);
        assert!(number(9_007_199_254_740_992.0).is_f64());
        let normalized = normalize_numbers(serde_json::from_str("[-3e2, 1.5, {\"a\": 2.0}]").unwrap());
        assert_eq!(normalized.to_string(), r#"[-300,1.5,{"a":2}]"#);
    }
}
