//! Format-4 JSONL codec (`harness/session/jsonl/{codec,io,types}.ts`): the header, the legacy v3 header, and one
//! transaction per line (a single write is written bare, several as an array).

use serde_json::{Map, Value};

pub const JSONL_FORMAT_VERSION: i64 = 4;
pub const JSONL_STORAGE_VERSION: i64 = 1;

/// `Number.isSafeInteger(value) && value >= minimum`.
pub(crate) fn safe_integer_at_least(value: Option<&Value>, minimum: i64) -> bool {
    value.and_then(jsrt::number::safe_integer).is_some_and(|n| n >= minimum)
}

fn optional_string(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).is_none_or(Value::is_string)
}

/// `isJsonlStorageHeader`.
pub fn is_v4_header(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.get("kind").and_then(Value::as_str) == Some("header")
        && map.get("v").and_then(Value::as_f64) == Some(JSONL_FORMAT_VERSION as f64)
        && map.get("id").is_some_and(Value::is_string)
        && map.get("cwd").is_some_and(Value::is_string)
        && safe_integer_at_least(map.get("storageVersion"), 1)
        && safe_integer_at_least(map.get("createdAt"), 0)
        && (map.get("nextSeq").is_none() || safe_integer_at_least(map.get("nextSeq"), 1))
        && optional_string(map, "parentSessionId")
        && optional_string(map, "legacyParentSessionPath")
}

/// `isLegacyV3SessionHeader`.
pub fn is_v3_header(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.get("type").and_then(Value::as_str) == Some("session")
        && map.get("version").and_then(Value::as_f64) == Some(3.0)
        && map.get("id").is_some_and(Value::is_string)
        && map.get("cwd").is_some_and(Value::is_string)
        && map
            .get("timestamp")
            .and_then(Value::as_str)
            .is_some_and(|t| jsrt::date::parse_date(t).is_some())
        && optional_string(map, "parentSession")
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParsedHeader {
    V4(Map<String, Value>),
    V3(Map<String, Value>),
}

/// `parseJsonlSessionHeader`.
pub fn parse_header(line: &str) -> Result<ParsedHeader, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| "Invalid JSONL session header: not valid JSON".to_string())?;
    if is_v4_header(&value) {
        return Ok(ParsedHeader::V4(into_object(value)));
    }
    if is_v3_header(&value) {
        return Ok(ParsedHeader::V3(into_object(value)));
    }
    Err("Unsupported JSONL session header".into())
}

pub(crate) fn into_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Files written before the rename name their reserved namespaces and durable kinds `pi.*`; they read as
/// `ultron.*`. The file itself is never rewritten.
pub fn rename_legacy(write: &mut Value) {
    fn rename(slot: Option<&mut Value>) {
        if let Some(Value::String(name)) = slot
            && let Some(rest) = name.strip_prefix("pi.")
        {
            *name = format!("ultron.{rest}");
        }
    }
    rename(write.get_mut("namespace"));
    if write["key"] == crate::ns::DURABLE_KEY
        && let Some(Value::Array(writes)) = write.pointer_mut("/value/writes")
    {
        for inner in writes {
            rename(inner.pointer_mut("/value/kind"));
            rename(inner.pointer_mut("/record/kind"));
        }
    }
}

fn parse_committed_write(mut value: Value) -> Result<Value, String> {
    rename_legacy(&mut value);
    let Some(map) = value.as_object() else {
        return Err("Invalid JSONL transaction write".into());
    };
    if !safe_integer_at_least(map.get("seq"), 1) {
        return Err("Invalid JSONL write seq".into());
    }
    let op = jsrt::value::string(map.get("op"));
    match map.get("kind").and_then(Value::as_str) {
        Some("entry") => {
            if !safe_integer_at_least(map.get("timestamp"), 0) {
                return Err("Invalid JSONL entry timestamp".into());
            }
        }
        Some("usage") => {}
        Some("value") if op == "set" || op == "delete" => {}
        Some("value") => return Err(format!("Invalid JSONL value operation: {op}")),
        Some("list") if op == "append" || op == "delete" => {}
        Some("list") => return Err(format!("Invalid JSONL list operation: {op}")),
        _ => {
            let kind = jsrt::value::string(map.get("kind"));
            return Err(format!("Invalid JSONL write kind: {kind}"));
        }
    }
    Ok(value)
}

/// `parseJsonlTransaction`: one write object or an array of them.
pub fn parse_transaction(line: &str) -> Result<Vec<Value>, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| "Invalid JSONL transaction: not valid JSON".to_string())?;
    match value {
        Value::Array(items) => items.into_iter().map(parse_committed_write).collect(),
        other => Ok(vec![parse_committed_write(other)?]),
    }
}

/// `serializeJsonlTransaction`.
pub fn serialize_transaction(writes: &[Value]) -> String {
    match writes {
        [one] => jsrt::json::stringify(one),
        // `JSON.stringify` of the array, without copying the writes into one.
        many => format!(
            "[{}]",
            many.iter().map(jsrt::json::stringify).collect::<Vec<_>>().join(",")
        ),
    }
}

pub fn header_str<'a>(header: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    header.get(key).and_then(Value::as_str)
}

pub fn header_i64(header: &Map<String, Value>, key: &str) -> Option<i64> {
    header
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}
