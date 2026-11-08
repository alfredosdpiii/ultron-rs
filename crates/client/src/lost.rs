//! Lost-server messages (experimental/lost-server.ts): a call that failed because the server or the session worker
//! went away; the session file is durable, so the message says how to resume.

use crate::runtime::CallError;

pub const RESUME_HINT: &str = "your session is saved — run `ultron -c` to resume";

/// `isLostServerError`: the server answered that the session's worker went away (its code), or the connection
/// failed in a way that means the server is gone. `Disconnected` also carries local failures that are not a lost
/// server (an offline Radius relay, a session that is not attached), so for it the message decides, as TypeScript
/// matches its transport errors' messages.
pub fn is_lost(error: &CallError) -> bool {
    match error {
        CallError::Disconnected(message) => reads_as_lost(message),
        CallError::Server(error) => protocol::names::is_lost_code(&error.code),
    }
}

/// `lostServerMessage`.
pub fn lost_server_message(reason: Option<&str>) -> String {
    let detail = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map_or(String::new(), |r| format!(" ({r})"));
    format!("The Ultron server stopped{detail}; {RESUME_HINT}")
}

/// `describeLostServer`: the clear message for a lost server or worker; `None` for any other error.
pub fn describe(error: &CallError) -> Option<String> {
    if !is_lost(error) {
        return None;
    }
    let text = error.to_string();
    Some(if text.contains(RESUME_HINT) {
        text
    } else {
        lost_server_message(Some(&text))
    })
}

/// `clientErrorMessage`: the error's message, the clear lost-server message when the server or worker went away.
pub fn message(error: impl std::borrow::Borrow<CallError>) -> String {
    let error = error.borrow();
    describe(error).unwrap_or_else(|| error.to_string())
}

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        boundary(before) && boundary(after)
    })
}

/// `LOST_SERVER_PATTERNS` on a transport failure's or a status line's text, which carry no error code. Rust's own
/// I/O wording for a reset, broken or refused socket counts like Node's `ECONNRESET`/`EPIPE`/`ECONNREFUSED`.
fn reads_as_lost(message: &str) -> bool {
    let lower = message.to_lowercase();
    let worker = lower
        .find("session worker ")
        .map(|i| &lower[i + "session worker ".len()..])
        .is_some_and(|rest| {
            ["disconnected unexpectedly", "exited unexpectedly"]
                .iter()
                .any(|tail| rest.contains(tail))
        });
    worker
        || [
            "binding is closed",
            "byte transport closed",
            "unix connection is closed",
            "session worker disconnected during",
            "lost its coordinator",
            "experimental server was replaced",
            "experimental server is shutting down",
            "experimental server detached",
            "broken pipe",
            "connection reset by peer",
            "connection refused",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        || ["ECONNRESET", "EPIPE", "ECONNREFUSED"]
            .iter()
            .any(|w| contains_word(message, w))
}

/// `friendlyStatus`: a status line's `Error: …` text, made clear when it reports a lost server.
pub fn friendly_status(text: &str) -> String {
    if text.contains(RESUME_HINT) {
        return text.to_string();
    }
    for prefix in ["Error: ", "Reconnect error: "] {
        if let Some(rest) = text.strip_prefix(prefix)
            && reads_as_lost(rest)
        {
            return format!("Error: {}", lost_server_message(Some(rest)));
        }
    }
    text.to_string()
}
