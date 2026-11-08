//! Reading Pi message JSON (`{role, content, ...}`), whose content is a string or a list of blocks.

use serde_json::Value;

/// The text blocks of a message joined (a string content as it is).
pub fn text(message: &Value) -> String {
    content_text(&message["content"], "")
}

/// A content's text blocks joined by `separator` (a string content as it is).
pub fn content_text(content: &Value, separator: &str) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(separator),
        _ => String::new(),
    }
}
