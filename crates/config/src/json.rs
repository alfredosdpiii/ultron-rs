//! `JSON.parse` of a config file. `JSON.stringify` and property order are in [`jsrt::json`].

use serde_json::Value;

/// `JSON.parse(stripBom(text))`.
pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}
