//! The clock seam and small checks the modules share. The JavaScript semantics they use are in [`jsrt`].

use serde_json::Value;
use std::sync::Arc;

/// Milliseconds since the epoch (`Date.now()`); injected so fixtures can fix it.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(jsrt::date::now_ms)
}

pub(crate) fn object(entries: Vec<(&str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// A versioned definition reference, `<id>@<version>` (`/^[a-z][a-z0-9-]*@[0-9]+$/`).
pub(crate) fn valid_definition(text: &str) -> bool {
    let Some((id, version)) = text.split_once('@') else {
        return false;
    };
    id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
}
