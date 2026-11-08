//! `/continue` (`Op::ContinueFrom`): a new session that continues an old one by searching its log instead of carrying
//! a summary of it. Its first message says which session it continues and how to read it (`sessions.search` /
//! `sessions.read` scoped to it); nothing of the old conversation is copied.

use serde_json::{Value, json};
use session_journal::repo::Metadata;

/// The custom message type of the note.
pub const CONTINUATION: &str = "continuation";

/// The note that opens a session continuing `old` (named `name`).
pub fn message(old: &Metadata, name: Option<&str>, timestamp: i64) -> Value {
    let id = &old.id;
    let named = name.map_or(String::new(), |name| format!(" (\"{name}\")"));
    let content = format!(
        "[continuation] This session continues session {id}{named}, last active {}. Its conversation is not in your \
         context and was not summarized: when the user refers to earlier work, or you need a decision, path or result \
         from it, look it up instead of guessing: `await sessions.search(\"regex\", sessions=[\"{id}\"])` (or \
         `terms=[...]`) finds its turns, each hit with a `span`; `await sessions.read(span, around=5)` reads one; \
         `await sessions.read(session=\"{id}\", limit=50)` reads it from the start.",
        jsrt::date::iso_string(old.modified_at as i64)
    );
    json!({
        "role": "custom",
        "customType": CONTINUATION,
        "content": content,
        "display": true,
        "details": {"session": id},
        "timestamp": timestamp,
    })
}
