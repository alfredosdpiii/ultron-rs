//! `JSON.parse` of a config file. `JSON.stringify` and property order are in [`jsrt::json`].

use serde_json::{Map, Value};

// ponytail: kept for the crates that still import them from here (ai, cli, harness, session-core); they move to
// `jsrt` directly.
pub use jsrt::json::stringify_pretty;
pub use jsrt::number::json_to_string as js_number;

/// Keys in the order a JavaScript object enumerates them: array indices ascending, then insertion order.
pub fn js_key_order(map: &Map<String, Value>) -> Vec<&String> {
    jsrt::json::object_entries(map)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// `JSON.parse(stripBom(text))`.
pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}
