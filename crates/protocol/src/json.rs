//! Service payloads are JSON on the CBOR wire. Both directions are total: a CBOR value JSON cannot hold reads as
//! `JSON.stringify` writes the decoded JavaScript value (a byte string or a non-finite float is `null`, an integral
//! float below 2^53 an integer, `-0` is `0`).

use crate::cbor::Value;
use serde_json::Value as Json;

pub fn to_cbor(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN)), Value::Int),
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(to_cbor).collect()),
        Json::Object(map) => Value::Map(map.iter().map(|(k, v)| (k.clone(), to_cbor(v))).collect()),
    }
}

pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null | Value::Bytes(_) => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Float(f) => jsrt::json::number(*f),
        Value::Text(s) => Json::String(s.clone()),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Map(entries) => Json::Object(entries.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}
