//! CBOR values as JSON, as the TypeScript client sees them (`JSON.stringify` of the decoded number).

use client::runtime::to_json;
use protocol::Value;

#[test]
fn client_cbor_floats_become_json_numbers_like_javascript() {
    // `JSON.stringify(9e15)` is "9000000000000000": integral floats below 2^53 are integers (the bound was 9e15).
    assert_eq!(to_json(&Value::Float(9e15)).to_string(), "9000000000000000");
    assert_eq!(to_json(&Value::Float(-0.0)).to_string(), "0");
    assert_eq!(to_json(&Value::Float(1.5)).to_string(), "1.5");
    assert_eq!(to_json(&Value::Float(f64::NAN)).to_string(), "null");
}
