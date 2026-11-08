//! Durable module documents: the format version a stored document is read at.

use serde_json::Value;

/// `readVersioned`: a document without its version field reads as version 1; another version is refused.
pub fn read_versioned(namespace: &str, saved: Option<Value>, field: &str) -> Result<Option<Value>, String> {
    let Some(Value::Object(mut document)) = saved else {
        return Ok(saved);
    };
    match document.get(field) {
        None => {
            document.insert(field.to_string(), Value::from(1));
            Ok(Some(Value::Object(document)))
        }
        Some(Value::Number(number)) if number.as_f64() == Some(1.0) => Ok(Some(Value::Object(document))),
        Some(version) => Err(format!(
            "Cannot resume: session value {namespace} has format version {version}, but this Ultron reads only format \
             version 1. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade."
        )),
    }
}
