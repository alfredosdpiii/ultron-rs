//! The clock seam the modules share. The JavaScript semantics they use are in [`jsrt`].

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
