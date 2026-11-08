//! CBOR values as JSON, as the TypeScript side sees them (`JSON.stringify` of the decoded value), on both ends of a
//! connection.

use protocol::Value;
use protocol::json::{to_cbor, to_json};
use serde_json::json;

#[test]
fn protocol_json_floats_become_json_numbers_like_javascript() {
    // `JSON.stringify(9e15)` is "9000000000000000": integral floats below 2^53 are integers (the bound was 9e15).
    assert_eq!(to_json(&Value::Float(9e15)).to_string(), "9000000000000000");
    assert_eq!(to_json(&Value::Float(-0.0)).to_string(), "0");
    assert_eq!(to_json(&Value::Float(1.5)).to_string(), "1.5");
    assert_eq!(to_json(&Value::Float(f64::NAN)).to_string(), "null");
}

/// The server read a call holding a byte string as `null` as a whole (its own converter refused it); it now reads it
/// as the client does: only the byte string is `null`.
#[test]
fn protocol_json_byte_strings_are_null_in_place() {
    let call = Value::Map(vec![
        ("serviceId".into(), Value::Text("s".into())),
        (
            "args".into(),
            Value::Array(vec![Value::Bytes(vec![1, 2]), Value::Int(3)]),
        ),
    ]);
    assert_eq!(to_json(&call), json!({"serviceId": "s", "args": [null, 3]}));
}

#[test]
fn protocol_json_round_trips_payloads() {
    let payload = json!({"a": [1, -2, 1.5, "x", null, true], "b": {"c": 9_007_199_254_740_993_i64}});
    assert_eq!(to_json(&to_cbor(&payload)), payload);
}
