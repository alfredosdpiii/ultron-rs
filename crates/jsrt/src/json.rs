//! `JSON.stringify` of `serde_json` values, JavaScript property order, numbers held as JavaScript holds them, and
//! V8's `JSON.parse` with its error messages ([`parse`]).
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

/// `preview(value, limit)`: a string as is, anything else as `JSON.stringify`, whitespace collapsed
/// ([`crate::string::flatten`]), cut to `limit` UTF-16 units with an ellipsis.
pub fn preview(value: &Value, limit: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => stringify(other),
    };
    let flat = crate::string::flatten(&text);
    if crate::utf16::len(&flat) > limit {
        format!("{}…", crate::utf16::head(&flat, limit.saturating_sub(1)))
    } else {
        flat
    }
}

// ------------------------------------------------------------------------------------------------- JSON.parse

/// Sources at least this long show the unexpected token in context (V8 `kMinOriginalSourceLengthForContext`).
const MIN_LENGTH_FOR_CONTEXT: usize = 21;
const CONTEXT_CHARS: usize = 10;

/// `JSON.parse(text)` as V8 implements it, or V8's `SyntaxError` message (`Unexpected token 'x', "x" is not valid
/// JSON`, `... in JSON at position N (line L column C)`). Positions, lines and columns count UTF-16 units; only `\n`
/// starts a line. Numbers become what JavaScript holds (doubles), written as integers when integral.
pub fn parse(text: &str) -> Result<Value, String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut parser = Parser { units: &units, pos: 0 };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos < units.len() {
        return Err(parser.located("Unexpected non-whitespace character after JSON", parser.pos));
    }
    Ok(value)
}

/// What a token at an unexpected place is.
enum Token {
    End,
    Number,
    Other,
}

struct Parser<'a> {
    units: &'a [u16],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0a | 0x0d)) {
            self.pos += 1;
        }
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf16_lossy(&self.units[from.min(self.units.len())..to.min(self.units.len())])
    }

    /// `<message> in JSON at position P (line L column C)`.
    fn at(&self, message: &str, pos: usize) -> String {
        self.located(&format!("{message} in JSON"), pos)
    }

    /// `<message> at position P (line L column C)`.
    fn located(&self, message: &str, pos: usize) -> String {
        let before = &self.units[..pos.min(self.units.len())];
        let line = before.iter().filter(|&&u| u == 0x0a).count() + 1;
        let line_start = before.iter().rposition(|&u| u == 0x0a).map_or(0, |i| i + 1);
        format!(
            "{message} at position {pos} (line {line} column {})",
            pos - line_start + 1
        )
    }

    /// `ReportUnexpectedToken` without a specific message.
    fn unexpected(&self, token: Token) -> String {
        match token {
            Token::End => "Unexpected end of JSON input".into(),
            Token::Number => self.at("Unexpected number", self.pos),
            Token::Other => {
                let source = self.text(0, self.units.len());
                if ["NaN", "Infinity", "undefined", "[object Object]"].contains(&source.as_str()) {
                    return format!("\"{source}\" is not valid JSON");
                }
                let len = self.units.len();
                let pos = self.pos;
                let token = self.text(pos, pos + 1);
                let context = if len < MIN_LENGTH_FOR_CONTEXT {
                    format!("\"{source}\"")
                } else if pos < CONTEXT_CHARS {
                    format!("\"{}\"...", self.text(0, pos + CONTEXT_CHARS))
                } else if pos + CONTEXT_CHARS >= len {
                    format!("...\"{}\"", self.text(pos - CONTEXT_CHARS, len))
                } else {
                    format!("...\"{}\"...", self.text(pos - CONTEXT_CHARS, pos + CONTEXT_CHARS))
                };
                format!("Unexpected token '{token}', {context} is not valid JSON")
            }
        }
    }

    /// An error with V8's specific message for this place (also at the end of the input).
    fn expected(&self, message: &str) -> String {
        self.at(message, self.pos)
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            None => Err(self.unexpected(Token::End)),
            Some(0x22) => self.string().map(Value::String),
            Some(0x7b) => self.object(),
            Some(0x5b) => self.array(),
            Some(0x2d | 0x30..=0x39) => self.number(),
            Some(0x74) => self.literal("true", Value::Bool(true)),
            Some(0x66) => self.literal("false", Value::Bool(false)),
            Some(0x6e) => self.literal("null", Value::Null),
            Some(_) => Err(self.unexpected(Token::Other)),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        for expected in word.encode_utf16() {
            match self.peek() {
                None => return Err(self.unexpected(Token::End)),
                Some(unit) if unit == expected => self.pos += 1,
                Some(_) => return Err(self.unexpected(Token::Other)),
            }
        }
        Ok(value)
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(0x30..=0x39)) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(0x2d) {
            self.pos += 1;
            if !matches!(self.peek(), Some(0x30..=0x39)) {
                return Err(self.expected("No number after minus sign"));
            }
        }
        if self.peek() == Some(0x30) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x30..=0x39)) {
                return Err(self.unexpected(Token::Number));
            }
        } else {
            self.digits();
        }
        if self.peek() == Some(0x2e) {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.expected("Unterminated fractional number"));
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x2b | 0x2d)) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.expected("Exponent part is missing a number"));
            }
        }
        let value: f64 = self.text(start, self.pos).parse().unwrap_or(f64::NAN);
        Ok(crate::json::number(value))
    }

    fn hex(&mut self) -> Result<u16, String> {
        let mut code = 0u16;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|u| char::from_u32(u32::from(u)))
                .and_then(|c| c.to_digit(16));
            let Some(digit) = digit else {
                return Err(self.expected("Bad Unicode escape"));
            };
            code = code * 16 + digit as u16;
            self.pos += 1;
        }
        Ok(code)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out: Vec<u16> = Vec::new();
        loop {
            let Some(unit) = self.peek() else {
                return Err(self.expected("Unterminated string"));
            };
            match unit {
                0x22 => {
                    self.pos += 1;
                    return Ok(String::from_utf16_lossy(&out));
                }
                0x5c => {
                    self.pos += 1;
                    let Some(escaped) = self.peek() else {
                        return Err(self.unexpected(Token::End));
                    };
                    let decoded = match escaped {
                        0x22 | 0x5c | 0x2f => escaped,
                        0x62 => 0x08,
                        0x66 => 0x0c,
                        0x6e => 0x0a,
                        0x72 => 0x0d,
                        0x74 => 0x09,
                        0x75 => {
                            self.pos += 1;
                            out.push(self.hex()?);
                            continue;
                        }
                        _ => return Err(self.expected("Bad escaped character")),
                    };
                    out.push(decoded);
                    self.pos += 1;
                }
                0..0x20 => return Err(self.expected("Bad control character in string literal")),
                _ => {
                    out.push(unit);
                    self.pos += 1;
                }
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut map = Map::new();
        self.skip_whitespace();
        if self.peek() == Some(0x7d) {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        if self.peek() != Some(0x22) {
            return Err(self.expected("Expected property name or '}'"));
        }
        loop {
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(0x3a) {
                return Err(self.expected("Expected ':' after property name"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value()?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(0x2c) => {
                    self.pos += 1;
                    self.skip_whitespace();
                    if self.peek() != Some(0x22) {
                        return Err(self.expected("Expected double-quoted property name"));
                    }
                }
                Some(0x7d) => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.expected("Expected ',' or '}' after property value")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(0x5d) {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(0x2c) => self.pos += 1,
                Some(0x5d) => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.expected("Expected ',' or ']' after array element")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Literals printed by node 26.

    #[test]
    fn jsrt_json_preview_counts_utf16_units() {
        // `"😀😀ab".length` is 6: four units of emoji, then two letters.
        assert_eq!(preview(&json!("😀😀ab"), 6), "😀😀ab");
        assert_eq!(preview(&json!("😀😀abc"), 6), "😀😀a…");
        assert_eq!(preview(&json!({"a": " x\n\ty "}), 100), r#"{"a":" x\n\ty "}"#);
        assert_eq!(preview(&json!(" a \n b "), 100), "a b");
    }

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
