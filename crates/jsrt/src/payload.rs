//! Checks on the JSON payloads the kernel host APIs receive.

use serde_json::{Map, Value};

/// Refuses a payload key outside `allowed`, naming the first one in payload order:
/// "Unknown payload field: <key>".
pub fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn jsrt_fields_names_the_first_unknown_key_in_payload_order() {
        let payload: Map<String, Value> = serde_json::from_str(r#"{"name":1,"zeta":2,"alpha":3}"#).unwrap();
        assert_eq!(fields(&payload, &["name"]), Err("Unknown payload field: zeta".into()));
        assert_eq!(fields(&payload, &["name", "zeta", "alpha"]), Ok(()));
        assert_eq!(fields(&map(json!({})), &[]), Ok(()));
    }
}
