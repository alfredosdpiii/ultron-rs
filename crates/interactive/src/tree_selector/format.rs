use std::collections::HashMap;

use jsrt::{date, utf16};
use serde_json::Value;
use theme::{Fg, Theme};

use crate::components::{bold, italic};

use super::{Node, s};

/// `extractContent`: the first 200 UTF-16 units.
pub(super) fn content(content: &Value) -> String {
    let full = protocol::pi_message::content_text(content, "");
    let units: Vec<u16> = full.encode_utf16().take(200).collect();
    String::from_utf16_lossy(&units)
}

pub(super) fn has_text(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(|t| !t.trim().is_empty())),
        _ => false,
    }
}

pub(super) fn normalize(text: &str) -> String {
    text.replace(['\n', '\t'], " ").trim().to_string()
}

fn shorten_home(path: String, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path,
    }
}

fn arg_text(args: &Value, key: &str) -> Option<String> {
    match &args[key] {
        Value::String(v) if !v.is_empty() => Some(v.clone()),
        Value::Null | Value::Bool(false) | Value::String(_) => None,
        other => Some(other.to_string()),
    }
}

fn tool_path(args: &Value, home: &str) -> String {
    shorten_home(
        arg_text(args, "path")
            .or_else(|| arg_text(args, "file_path"))
            .unwrap_or_default(),
        home,
    )
}

fn format_read_tool(args: &Value, home: &str) -> String {
    let mut display = tool_path(args, home);
    let offset = args["offset"].as_i64();
    let limit = args["limit"].as_i64();
    if offset.is_some() || limit.is_some() {
        let start = offset.unwrap_or(1);
        let end = limit.map_or_default(|l| (start + l - 1).to_string());
        let range = if end.is_empty() || end == "0" {
            String::new()
        } else {
            format!("-{end}")
        };
        display += &format!(":{start}{range}");
    }
    format!("[read: {display}]")
}

fn format_bash_tool(args: &Value) -> String {
    let raw = arg_text(args, "command").unwrap_or_default();
    let cmd = utf16::head_lossy(normalize(&raw).as_str(), 50);
    let more = if utf16::len(&raw) > 50 { "..." } else { "" };
    format!("[bash: {cmd}{more}]")
}

fn search_path(args: &Value, home: &str) -> String {
    shorten_home(arg_text(args, "path").unwrap_or_else(|| ".".into()), home)
}

fn format_generic_tool(name: &str, args: &Value) -> String {
    let json = args.to_string();
    let more = if utf16::len(&json) > 40 { "..." } else { "" };
    format!("[{name}: {}{more}]", utf16::head_lossy(&json, 40))
}

/// `formatToolCall` (paths under `home` shortened to `~`).
pub(super) fn format_tool_call(name: &str, args: &Value, home: &str) -> String {
    match name {
        "read" => format_read_tool(args, home),
        "write" => format!("[write: {}]", tool_path(args, home)),
        "edit" => format!("[edit: {}]", tool_path(args, home)),
        "bash" => format_bash_tool(args),
        "grep" => format!(
            "[grep: /{}/ in {}]",
            arg_text(args, "pattern").unwrap_or_default(),
            search_path(args, home)
        ),
        "find" => format!(
            "[find: {} in {}]",
            arg_text(args, "pattern").unwrap_or_default(),
            search_path(args, home)
        ),
        "ls" => format!("[ls: {}]", search_path(args, home)),
        _ => format_generic_tool(name, args),
    }
}

fn searchable_message_parts(m: &Value, parts: &mut Vec<String>) {
    parts.push(s(m, "role").unwrap_or("").to_string());
    if !m["content"].is_null() {
        parts.push(content(&m["content"]));
    }
    if m["role"] == "bashExecution"
        && let Some(c) = s(m, "command").filter(|c| !c.is_empty())
    {
        parts.push(c.to_string());
    }
}

fn searchable_meta_parts(kind: &str, entry: &Value, parts: &mut Vec<String>) {
    match kind {
        "session_info" => {
            parts.push("title".into());
            if let Some(name) = s(entry, "name").filter(|n| !n.is_empty()) {
                parts.push(name.to_string());
            }
        }
        "model_change" => {
            parts.push("model".into());
            parts.push(s(entry, "modelId").unwrap_or("").to_string());
        }
        "thinking_level_change" => {
            parts.push("thinking".into());
            parts.push(s(entry, "thinkingLevel").unwrap_or("").to_string());
        }
        "custom" => {
            parts.push("custom".into());
            parts.push(s(entry, "customType").unwrap_or("").to_string());
        }
        "context_edit" => {
            parts.push("context edit".into());
            parts.push(
                if entry["replacement"].is_null() {
                    "omit"
                } else {
                    "replace"
                }
                .into(),
            );
            parts.push(s(entry, "targetId").unwrap_or("").to_string());
        }
        "label" => {
            parts.push("label".into());
            parts.push(s(entry, "label").unwrap_or("").to_string());
        }
        _ => {}
    }
}

pub(super) fn searchable_text(node: &Node) -> String {
    let entry = &node.entry;
    let mut parts: Vec<String> = Vec::new();
    if let Some(label) = &node.label {
        parts.push(label.clone());
    }
    match s(entry, "type").unwrap_or("") {
        "message" => searchable_message_parts(&entry["message"], &mut parts),
        "custom_message" => {
            parts.push(s(entry, "customType").unwrap_or("").to_string());
            parts.push(match &entry["content"] {
                Value::String(t) => t.clone(),
                other => content(other),
            });
        }
        "compaction" => parts.push("compaction".into()),
        "branch_summary" => {
            parts.push("branch summary".into());
            parts.push(s(entry, "summary").unwrap_or("").to_string());
        }
        kind => searchable_meta_parts(kind, entry, &mut parts),
    }
    parts.join(" ")
}

fn display_assistant_message(t: &Theme, m: &Value) -> String {
    let text = normalize(&content(&m["content"]));
    let head = t.fg(Fg::Success, "assistant: ");
    if !text.is_empty() {
        head + &text
    } else if m["stopReason"] == "aborted" {
        head + &t.fg(Fg::Muted, "(aborted)")
    } else if let Some(error) = s(m, "errorMessage").filter(|e| !e.is_empty()) {
        head + &t.fg(Fg::Error, &utf16::head_lossy(&normalize(error), 80))
    } else {
        head + &t.fg(Fg::Muted, "(no content)")
    }
}

fn display_message(t: &Theme, tool_calls: &HashMap<String, (String, Value)>, home: &str, m: &Value) -> String {
    match s(m, "role").unwrap_or("") {
        "user" => t.fg(Fg::Accent, "user: ") + &normalize(&content(&m["content"])),
        "assistant" => display_assistant_message(t, m),
        "toolResult" => {
            let call = s(m, "toolCallId").and_then(|id| tool_calls.get(id));
            match call {
                Some((name, args)) => t.fg(Fg::Muted, &format_tool_call(name, args, home)),
                None => t.fg(Fg::Muted, &format!("[{}]", s(m, "toolName").unwrap_or("tool"))),
            }
        }
        "bashExecution" => t.fg(
            Fg::Dim,
            &format!("[bash]: {}", normalize(s(m, "command").unwrap_or(""))),
        ),
        role => t.fg(Fg::Dim, &format!("[{role}]")),
    }
}

fn display_meta_entry(t: &Theme, kind: &str, entry: &Value) -> String {
    match kind {
        "model_change" => t.fg(Fg::Dim, &format!("[model: {}]", s(entry, "modelId").unwrap_or(""))),
        "thinking_level_change" => t.fg(
            Fg::Dim,
            &format!("[thinking: {}]", s(entry, "thinkingLevel").unwrap_or("")),
        ),
        "custom" => t.fg(Fg::Dim, &format!("[custom: {}]", s(entry, "customType").unwrap_or(""))),
        "context_edit" => t.fg(
            Fg::Dim,
            &format!(
                "[context {}: {}]",
                if entry["replacement"].is_null() {
                    "omit"
                } else {
                    "replace"
                },
                s(entry, "targetId").unwrap_or("")
            ),
        ),
        "label" => t.fg(
            Fg::Dim,
            &format!("[label: {}]", s(entry, "label").unwrap_or("(cleared)")),
        ),
        "session_info" => match s(entry, "name").filter(|n| !n.is_empty()) {
            Some(name) => t.fg(Fg::Dim, "[title: ") + &t.fg(Fg::Dim, name) + &t.fg(Fg::Dim, "]"),
            None => t.fg(Fg::Dim, "[title: ") + &italic(&t.fg(Fg::Dim, "empty")) + &t.fg(Fg::Dim, "]"),
        },
        _ => String::new(),
    }
}

pub(super) fn display_text(
    t: &Theme,
    tool_calls: &HashMap<String, (String, Value)>,
    home: &str,
    node: &Node,
    selected: bool,
) -> String {
    let entry = &node.entry;
    let result = match s(entry, "type").unwrap_or("") {
        "message" => display_message(t, tool_calls, home, &entry["message"]),
        "custom_message" => {
            let text = match &entry["content"] {
                Value::String(text) => text.clone(),
                other => protocol::pi_message::content_text(other, ""),
            };
            t.fg(
                Fg::CustomMessageLabel,
                &format!("[{}]: ", s(entry, "customType").unwrap_or("")),
            ) + &normalize(&text)
        }
        "compaction" => {
            let tokens = (entry["tokensBefore"].as_f64().unwrap_or(0.0) / 1000.0).round();
            t.fg(Fg::BorderAccent, &format!("[compaction: {tokens}k tokens]"))
        }
        "branch_summary" => t.fg(Fg::Warning, "[branch summary]: ") + &normalize(s(entry, "summary").unwrap_or("")),
        kind => display_meta_entry(t, kind, entry),
    };
    if selected { bold(&result) } else { result }
}

pub(super) fn copy_text(node: &Node) -> Option<String> {
    let entry = &node.entry;
    let text = match s(entry, "type").unwrap_or("") {
        "message" => {
            let m = &entry["message"];
            if m["role"] == "bashExecution" {
                s(m, "command").map(str::to_string)
            } else if m.get("content").is_some() {
                let text = protocol::pi_message::content_text(&m["content"], "");
                if text.is_empty() && m["role"] == "assistant" {
                    s(m, "errorMessage").map(str::to_string)
                } else {
                    Some(text)
                }
            } else {
                None
            }
        }
        "custom_message" => Some(protocol::pi_message::content_text(&entry["content"], "")),
        "compaction" | "branch_summary" => s(entry, "summary").map(str::to_string),
        _ => None,
    };
    text.filter(|t| !t.trim().is_empty())
}

/// `formatLabelTimestamp`: the label's local time (UTC here), with the date when not today.
pub(super) fn format_label_timestamp(ts: &str) -> String {
    let date = ts.get(..10).unwrap_or("");
    let time = ts.get(11..16).unwrap_or("");
    let today = date::iso_string(date::now_ms())[..10].to_string();
    if date == today {
        return time.to_string();
    }
    let year = date.get(..4).unwrap_or("");
    let month: u32 = date.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(0);
    let day: u32 = date.get(8..10).and_then(|d| d.parse().ok()).unwrap_or(0);
    if Some(year) == today.get(..4) {
        format!("{month}/{day} {time}")
    } else {
        format!("{}/{month}/{day} {time}", year.get(2..).unwrap_or(""))
    }
}
