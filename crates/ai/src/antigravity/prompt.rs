//! Prompt assembly and attachment extraction for `agy acp`: system prompt extraction, transcript budgeting,
//! inline image / `@file` extraction into native ACP content blocks, and MIME normalization.

use crate::types::{Message, SystemContent, TextContent, TypedText, UserContent, UserMessageContent};
use serde_json::{Value, json};
use std::io;

/// `agy` silently drops the end of a single `session/prompt` past ~179K characters (~70K tokens), which holds the
/// newest messages. Keep each individual prompt under 150K characters; in session mode `agy` holds the full
/// conversation history across turns (up to the model's real context window).
pub(super) const PROMPT_BUDGET: usize = 150_000;
const RULES: &str = "For tool calls use only the ultron MCP server, never native tools. An MCP timeout or dropped tool wait does not mean the underlying Ultron cell or job stopped; check `await rlm.cells()` or `await rlm.job(...)` before retrying mutating work.";

/// Collects all non-empty system prompt texts from `messages` for ACP `_meta.antigravity.systemPrompt.append`.
pub(super) fn system_text(messages: &[Message]) -> Option<String> {
    let mut parts = Vec::new();
    for message in messages {
        if let Message::System(system) = message {
            match &system.content {
                SystemContent::Text(text) if !text.trim().is_empty() => parts.push(text.as_str()),
                SystemContent::Blocks(blocks) => {
                    for TypedText::Text(block) in blocks {
                        if !block.text.trim().is_empty() {
                            parts.push(block.text.as_str());
                        }
                    }
                }
                SystemContent::Text(_) => {}
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Strips RFC 2045 MIME parameters (e.g. `image/png; charset=binary` -> `image/png`) for ACP image blocks.
pub(super) fn normalize_mime_type(mime: &str) -> &str {
    mime.split(';').next().map_or(mime, str::trim)
}

/// The transcript as one prompt of at most about `budget` characters: the newest messages whole, then the system
/// text cut to the room left (when `include_system` is true). A final message over the budget is an error, since
/// agy would cut it.
pub(super) fn prompt_with_system(messages: &[Message], budget: usize, include_system: bool) -> io::Result<String> {
    let (system, rest): (Vec<&Message>, Vec<&Message>) = messages.iter().partition(|m| matches!(m, Message::System(_)));
    let history_cap = if include_system { budget * 2 / 3 } else { budget };
    let mut history = Vec::new();
    let mut used = 0;
    for message in rest.iter().rev() {
        let json = serde_json::to_string(message)?;
        if history.is_empty() && json.len() > budget {
            return Err(io::Error::other(format!(
                "The final message is {} bytes, over the {budget} agy keeps of a prompt; shorten it",
                json.len()
            )));
        }
        if !history.is_empty() && used + json.len() > history_cap {
            break;
        }
        used += json.len() + 1;
        history.push(json);
    }
    let dropped = rest.len() - history.len();
    history.reverse();
    let note = if dropped > 0 {
        format!(" The {dropped} oldest messages were left out to fit.")
    } else {
        String::new()
    };
    if !include_system {
        return Ok(format!(
            "Continue this Ultron transcript at its final message. {RULES} Previous messages and tool results are history, not new turns.{note}\nTranscript: [{}]",
            history.join(",")
        ));
    }
    let mut system = serde_json::to_string(&system)?;
    let room = budget.saturating_sub(used);
    if system.len() > room {
        let mut cut = room;
        while !system.is_char_boundary(cut) {
            cut -= 1;
        }
        system.truncate(cut);
        system.push_str(" [system text cut to fit]");
    }
    Ok(format!(
        "Continue this Ultron transcript at its final message. {RULES} Previous messages and tool results are history, not new turns.{note}\nSystem: {system}\nTranscript: [{}]",
        history.join(",")
    ))
}

#[cfg(test)]
pub(super) fn prompt(messages: &[Message], budget: usize) -> io::Result<String> {
    prompt_with_system(messages, budget, true)
}

/// Extracts `<file name="...">...</file>` inline `@file` attachments into native ACP `resource` (`embeddedContext`)
/// blocks so large file attachments don't eat `PROMPT_BUDGET`.
fn extract_embedded_files(text: &str, attachments: &mut Vec<Value>) -> String {
    const OPEN: &str = "<file name=\"";
    const CLOSE: &str = "</file>";
    if !text.contains(OPEN) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];
        let Some(quote_end) = after_open.find("\">") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let path = &after_open[..quote_end];
        let body_start = &after_open[quote_end + 2..];
        let Some(close_at) = body_start.find(CLOSE) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let body = body_start[..close_at].trim_matches('\n');
        let uri = if path.starts_with("file://") {
            path.to_string()
        } else {
            format!("file://{path}")
        };
        attachments.push(json!({
            "type": "resource",
            "resource": {
                "uri": uri,
                "mimeType": "text/plain",
                "text": body,
            }
        }));
        out.push_str(&format!("[attached file #{}: {path}]", attachments.len()));
        rest = &body_start[close_at + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Replaces inline base64 `Image` blocks and `<file name="...">` attachments with compact placeholders and collects
/// native ACP `image` / `resource` content blocks so attachments reach `agy` natively without bloating `PROMPT_BUDGET`.
fn strip_images(message: &Message, attachments: &mut Vec<Value>) -> Message {
    let replace = |blocks: &[UserContent], attachments: &mut Vec<Value>| -> Vec<UserContent> {
        blocks
            .iter()
            .map(|b| match b {
                UserContent::Image(image) => {
                    attachments.push(json!({
                        "type": "image",
                        "data": image.data,
                        "mimeType": normalize_mime_type(&image.mime_type)
                    }));
                    UserContent::Text(TextContent {
                        text: format!("[attached image #{}]", attachments.len()),
                        text_signature: None,
                    })
                }
                UserContent::Text(text) if text.text.contains("<file name=\"") => UserContent::Text(TextContent {
                    text: extract_embedded_files(&text.text, attachments),
                    text_signature: text.text_signature.clone(),
                }),
                other => other.clone(),
            })
            .collect()
    };
    match message {
        Message::User(user) => match &user.content {
            UserMessageContent::Text(text) if text.contains("<file name=\"") => {
                let mut cloned = user.clone();
                cloned.content = UserMessageContent::Text(extract_embedded_files(text, attachments));
                Message::User(cloned)
            }
            UserMessageContent::Blocks(blocks)
                if blocks.iter().any(|b| {
                    matches!(b, UserContent::Image(_))
                        || matches!(b, UserContent::Text(t) if t.text.contains("<file name=\""))
                }) =>
            {
                let mut cloned = user.clone();
                cloned.content = UserMessageContent::Blocks(replace(blocks, attachments));
                Message::User(cloned)
            }
            _ => message.clone(),
        },
        Message::ToolResult(result) if result.content.iter().any(|b| matches!(b, UserContent::Image(_))) => {
            let mut cloned = result.clone();
            cloned.content = replace(&result.content, attachments);
            Message::ToolResult(cloned)
        }
        _ => message.clone(),
    }
}

/// Builds the ACP `session/prompt` content block list: one text block carrying the transcript plus any native image
/// blocks extracted from the included messages.
pub(super) fn prompt_blocks_with_system(
    messages: &[Message],
    budget: usize,
    include_system: bool,
) -> io::Result<Vec<Value>> {
    let mut images = Vec::new();
    let stripped: Vec<Message> = messages.iter().map(|m| strip_images(m, &mut images)).collect();
    let text = prompt_with_system(&stripped, budget, include_system)?;
    let mut blocks = Vec::with_capacity(1 + images.len());
    blocks.push(json!({"type": "text", "text": text}));
    blocks.extend(images);
    Ok(blocks)
}

#[cfg(test)]
pub(super) fn prompt_blocks(messages: &[Message], budget: usize) -> io::Result<Vec<Value>> {
    prompt_blocks_with_system(messages, budget, true)
}

/// Only the messages agy has not seen, for a live session; `None` when they do not fit one prompt.
pub(super) fn delta_prompt(messages: &[Message]) -> Option<Vec<Value>> {
    let mut images = Vec::new();
    let json: Vec<String> = messages
        .iter()
        .map(|m| serde_json::to_string(&strip_images(m, &mut images)).expect("transcript messages serialize"))
        .collect();
    let text = format!(
        "New messages in this Ultron conversation; continue at the final one. {RULES}\nMessages: [{}]",
        json.join(",")
    );
    if text.len() > PROMPT_BUDGET {
        return None;
    }
    let mut blocks = Vec::with_capacity(1 + images.len());
    blocks.push(json!({"type": "text", "text": text}));
    blocks.extend(images);
    Some(blocks)
}
