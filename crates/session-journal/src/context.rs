//! `session/context.ts`: the model context of a branch path. The latest compaction replaces what came before it;
//! context edits (`custom` entries of type `context_edit`) omit or replace earlier entries; failed, aborted and
//! deferred assistant messages are left out.

use crate::legacy_v3::{branch_summary_message, compaction_summary_message};
use serde_json::{Map, Value, json};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

pub const CONTEXT_EDIT_CUSTOM_TYPE: &str = "context_edit";
pub const CONTEXT_OMITTED_CUSTOM_TYPE: &str = "context_edit:omitted";
pub const CONTEXT_OMITTED_TOOL_RESULT_TEXT: &str = "[Tool result removed from context.]";

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `buildContextEntries`. The path may be borrowed: only the entries from the newest compaction are copied.
pub fn build_context_entries<V: Borrow<Value>>(path: &[V]) -> Vec<Value> {
    let start = path
        .iter()
        .rposition(|e| str_of(e.borrow(), "type") == "compaction")
        .unwrap_or(0);
    path[start..].iter().map(|e| e.borrow().clone()).collect()
}

fn is_context_message(message: &Value) -> bool {
    str_of(message, "role") != "assistant" || !matches!(str_of(message, "stopReason"), "error" | "aborted" | "deferred")
}

/// `sessionEntryToContextMessages`.
pub fn entry_context_messages(entry: &Value) -> Vec<Value> {
    match str_of(entry, "type") {
        "message" => {
            let message = &entry["message"];
            if is_context_message(message) {
                vec![message.clone()]
            } else {
                vec![]
            }
        }
        "compaction" => {
            let mut out = vec![compaction_summary_message(
                &entry["summary"],
                &entry["tokensBefore"],
                entry["timestamp"].clone(),
            )];
            if let Some(tail) = entry["retainedTail"].as_array() {
                out.extend(tail.iter().filter(|m| is_context_message(m)).cloned());
            }
            out
        }
        "branch_summary" if jsrt::value::truthy(entry.get("summary")) => {
            vec![branch_summary_message(
                &entry["summary"],
                entry["fromId"].clone(),
                entry["timestamp"].clone(),
            )]
        }
        _ => vec![],
    }
}

/// One edit: `replacement` is `None` to omit the target, else `Some(content)` (a string or a content array).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEdit {
    pub target_id: String,
    pub replacement: Option<Value>,
}

fn parse_edit(value: &Value) -> Option<ContextEdit> {
    let target = value.get("targetId")?.as_str().filter(|t| !t.is_empty())?;
    let replacement = match value.get("replacement")? {
        Value::Null => None,
        Value::Object(r) => Some(r.get("content").filter(|c| c.is_string() || c.is_array())?.clone()),
        _ => return None,
    };
    Some(ContextEdit {
        target_id: target.into(),
        replacement,
    })
}

/// `contextEditsOf`: the edits one entry carries; none for any other entry or malformed data.
pub fn context_edits_of(entry: &Value) -> Vec<ContextEdit> {
    if str_of(entry, "type") != "custom" || str_of(entry, "customType") != CONTEXT_EDIT_CUSTOM_TYPE {
        return vec![];
    }
    let Some(data) = entry.get("data").filter(|d| d.is_object()) else {
        return vec![];
    };
    match data.get("edits").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(parse_edit).collect(),
        None => parse_edit(data).into_iter().collect(),
    }
}

/// The ids of the tool calls in a message's content.
pub fn content_call_ids(content: &Value) -> Vec<String> {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| str_of(p, "type") == "toolCall")
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `replaceContent`: `message` with `content` in place of its own (a string becomes one text part for an assistant
/// message or a tool result; `None` removes it). Messages of other roles are left as they are.
pub fn replace_content(message: &Value, content: Option<&Value>) -> Value {
    let role = str_of(message, "role");
    if !matches!(role, "user" | "assistant" | "toolResult" | "custom") {
        return message.clone();
    }
    let next = match (role, content) {
        ("assistant" | "toolResult", Some(Value::String(text))) => Some(json!([{"type": "text", "text": text}])),
        (_, other) => other.cloned(),
    };
    let mut out = message.clone();
    if let Value::Object(map) = &mut out {
        match next {
            Some(next) => {
                map.insert("content".into(), next);
            }
            None => {
                map.shift_remove("content");
            }
        }
    }
    out
}

fn omitted(entry: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "parentId", "seq", "timestamp"] {
        out.insert(key.into(), entry[key].clone());
    }
    out.insert("type".into(), "custom".into());
    out.insert("customType".into(), CONTEXT_OMITTED_CUSTOM_TYPE.into());
    Value::Object(out)
}

/// An entry as the model sees it, and whether an edit produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub entry: Value,
    pub changed: bool,
}

/// `applyContextEdits`: applies the edits found among `entries` to them. Tool calls and results stay paired:
/// results of an omitted assistant message (or of calls a replacement dropped) are omitted with it, and an omitted
/// result whose call stays visible becomes a stub result. Later edits win.
pub fn apply_context_edits<V: Borrow<Value>>(entries: &[V]) -> Vec<Value> {
    apply_context_edits_marked(entries)
        .into_iter()
        .map(|applied| applied.entry)
        .collect()
}

/// [`apply_context_edits`], each entry marked with whether an edit changed it.
pub fn apply_context_edits_marked<V: Borrow<Value>>(entries: &[V]) -> Vec<Applied> {
    let mut edits: HashMap<String, Option<Value>> = HashMap::new();
    for entry in entries {
        for edit in context_edits_of(entry.borrow()) {
            edits.insert(edit.target_id, edit.replacement);
        }
    }
    let unchanged = |entry: &Value| Applied {
        entry: entry.clone(),
        changed: false,
    };
    let changed = |entry: Value| Applied { entry, changed: true };
    if edits.is_empty() {
        return entries.iter().map(|e| unchanged(e.borrow())).collect();
    }
    let mut hidden = HashSet::new();
    for entry in entries.iter().map(Borrow::borrow) {
        if str_of(entry, "type") != "message" || str_of(&entry["message"], "role") != "assistant" {
            continue;
        }
        let Some(replacement) = edits.get(str_of(entry, "id")) else {
            continue;
        };
        let kept: HashSet<String> = replacement
            .as_ref()
            .map(content_call_ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for id in content_call_ids(&entry["message"]["content"]) {
            if !kept.contains(&id) {
                hidden.insert(id);
            }
        }
    }
    entries
        .iter()
        .map(Borrow::borrow)
        .map(|entry: &Value| {
            let id = str_of(entry, "id");
            match str_of(entry, "type") {
                "custom" => {
                    if matches!(edits.get(id), Some(None)) {
                        changed(omitted(entry))
                    } else {
                        unchanged(entry)
                    }
                }
                "message" => {
                    let message = &entry["message"];
                    if str_of(message, "role") == "toolResult" && hidden.contains(str_of(message, "toolCallId")) {
                        return changed(omitted(entry));
                    }
                    let Some(replacement) = edits.get(id) else {
                        return unchanged(entry);
                    };
                    let mut out = entry.clone();
                    match replacement {
                        None if str_of(message, "role") == "toolResult" => {
                            out["message"] = replace_content(message, Some(&CONTEXT_OMITTED_TOOL_RESULT_TEXT.into()));
                        }
                        None => return changed(omitted(entry)),
                        Some(content) => out["message"] = replace_content(message, Some(content)),
                    }
                    changed(out)
                }
                _ => unchanged(entry),
            }
        })
        .collect()
}

/// `buildSessionContext` without entry projectors (custom entries contribute nothing).
pub fn build_session_context<V: Borrow<Value>>(path: &[V]) -> Vec<Value> {
    build_session_context_with(path, &[])
}

/// `EntryProjector`: the model messages of a custom entry of the type it is registered for.
pub type EntryProjector = fn(&Value) -> Vec<Value>;

/// `buildSessionContext` with `entryProjectors` (custom type, projector): a custom entry whose type has a projector
/// contributes the projector's messages at its place in the branch. Edits apply first, so an omitted entry projects
/// nothing.
pub fn build_session_context_with<V: Borrow<Value>>(path: &[V], projectors: &[(&str, EntryProjector)]) -> Vec<Value> {
    project_context_entries(&apply_context_edits(&build_context_entries(path)), projectors)
}

/// The model messages of context entries (edits already applied), custom entries through `projectors`.
pub fn project_context_entries(entries: &[Value], projectors: &[(&str, EntryProjector)]) -> Vec<Value> {
    entries
        .iter()
        .flat_map(|entry| {
            let projector = (str_of(entry, "type") == "custom")
                .then(|| projectors.iter().find(|(kind, _)| *kind == str_of(entry, "customType")))
                .flatten();
            match projector {
                Some((_, project)) => project(entry),
                None => entry_context_messages(entry),
            }
        })
        .collect()
}

/// The `details` of each custom message of `custom_type` on the branch `entries` since its last compaction, oldest
/// first: what the context still shows of a feature that adds a message once (recalled memory, a playbook, the active
/// refinements).
pub fn custom_details_since_compaction<'a>(entries: &'a [Value], custom_type: &str) -> Vec<&'a Value> {
    let mut shown = Vec::new();
    for entry in entries {
        if entry["type"] == "compaction" {
            shown.clear();
        }
        let message = &entry["message"];
        if entry["type"] == "message" && message["role"] == "custom" && message["customType"] == custom_type {
            shown.push(&message["details"]);
        }
    }
    shown
}

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

fn text_user(text: String, timestamp: Option<&Value>) -> Value {
    let mut message = json!({"role": "user", "content": [{"type": "text", "text": text}]});
    if let Some(timestamp) = timestamp {
        message["timestamp"] = timestamp.clone();
    }
    message
}

/// `String(value)` as `Array.prototype.join` applies it: `null` and `undefined` become "".
pub(crate) fn join_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        value => jsrt::value::string(value),
    }
}

fn bash_execution_to_text(message: &Value) -> String {
    let output = join_string(message.get("output"));
    let mut text = format!("Ran `{}`\n", join_string(message.get("command")));
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    let exit_code = message.get("exitCode").filter(|c| !c.is_null());
    if jsrt::value::truthy(message.get("cancelled")) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code.filter(|c| c.as_f64() != Some(0.0)) {
        text.push_str(&format!("\n\nCommand exited with code {}", jsrt::json::stringify(code)));
    }
    if jsrt::value::truthy(message.get("truncated")) && jsrt::value::truthy(message.get("fullOutputPath")) {
        text.push_str(&format!(
            "\n\n[Output truncated. Full output: {}]",
            join_string(message.get("fullOutputPath"))
        ));
    }
    text
}

/// `convertToLlm`: agent messages as provider messages: custom messages become user messages, summaries user text,
/// `!!` bash executions are dropped.
pub fn convert_to_llm(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| match str_of(m, "role") {
            "bashExecution" if jsrt::value::truthy(m.get("excludeFromContext")) => None,
            "bashExecution" => Some(text_user(bash_execution_to_text(m), m.get("timestamp"))),
            "custom" => {
                let content = match m.get("content") {
                    Some(Value::String(text)) => json!([{"type": "text", "text": text}]),
                    other => other.cloned().unwrap_or(Value::Null),
                };
                let mut message = json!({"role": "user", "content": content});
                if let Some(timestamp) = m.get("timestamp") {
                    message["timestamp"] = timestamp.clone();
                }
                Some(message)
            }
            "branchSummary" => Some(text_user(
                format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    join_string(m.get("summary"))
                ),
                m.get("timestamp"),
            )),
            "compactionSummary" => Some(text_user(
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    join_string(m.get("summary"))
                ),
                m.get("timestamp"),
            )),
            "system" | "user" | "assistant" | "toolResult" => Some(m.clone()),
            _ => None,
        })
        .collect()
}
