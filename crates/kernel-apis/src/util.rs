//! The clock seam the modules share. The JavaScript semantics they use are in [`jsrt`].

use serde_json::Value;
use std::sync::Arc;

// ponytail: re-exports kept for the crates that still import them from here (acceptance, delegation,
// session-core); they move to `jsrt` directly.
pub use jsrt::date::iso_string as iso;
pub use jsrt::hash::sha256_hex;
pub use jsrt::json::stringify;
pub use jsrt::random::uuid_v4 as uuid;
pub use jsrt::string::trim as js_trim;
pub use jsrt::sync::lock;
pub use jsrt::utf16::head as utf16_prefix;
pub use jsrt::utf16::len as utf16_len;

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
