//! The inspection commands (`/agents`, `/memory`, `/skills`, `/experiments`, `/goals`, `/progress`;
//! experimental/services/inspection-commands.ts): each maps its argument text to one read-only host request, which
//! the session's inspector answers, and the answer is rendered as indented `key: value` lines.

use serde_json::{Value, json};

const MAX_LINES: usize = 40;

fn words(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

fn request(kind: &str, payload: Value) -> Result<(String, Value), String> {
    Ok((kind.to_string(), payload))
}

fn optional_id_request(args: &str, list_kind: &str, get_kind: &str) -> Result<(String, Value), String> {
    if args.is_empty() {
        request(list_kind, json!({}))
    } else {
        request(get_kind, json!({ "id": args }))
    }
}

fn why_or_list_request(
    args: &str,
    why_kind: &str,
    id_field: &str,
    list_kind: &str,
    usage: &str,
) -> Result<(String, Value), String> {
    let words = words(args);
    match (words.first().copied(), words.get(1).copied()) {
        (Some("why"), Some(target)) => request(why_kind, json!({ id_field: target })),
        (None | Some("list"), _) => request(list_kind, json!({})),
        _ => Err(usage.into()),
    }
}

fn progress_request(args: &str) -> Result<(String, Value), String> {
    if args.is_empty() {
        Err("Usage: /progress <task-id>".into())
    } else {
        request("progress.history", json!({ "task_id": args }))
    }
}

/// The host request of `/name args` (args trimmed), or the command's usage error; `None` when `name` is not an
/// inspection command.
pub fn inspection_request(name: &str, args: &str) -> Option<Result<(String, Value), String>> {
    Some(match name {
        "agents" => optional_id_request(args, "agents.status", "agents.inspect"),
        "memory" => why_or_list_request(
            args,
            "memory.why",
            "taskId",
            "memory.list",
            "Usage: /memory why <task-id> | /memory list",
        ),
        "skills" => why_or_list_request(
            args,
            "skills.why",
            "decision_id",
            "skills.list",
            "Usage: /skills why <decision-id> | /skills list",
        ),
        "experiments" => request("experiments.list", json!({})),
        "goals" => optional_id_request(args, "goals.list", "goals.get"),
        "progress" => progress_request(args),
        _ => return None,
    })
}

/// `JSON.stringify` of a scalar; strings as they are.
fn scalar(item: &Value) -> String {
    match item {
        Value::String(text) => text.clone(),
        other => jsrt::json::stringify(other),
    }
}

fn render_array(lines: &mut Vec<String>, items: &[Value], inner: &str) {
    for child in items {
        if child.is_array() || child.is_object() {
            lines.push(format!("{inner}-"));
            render(lines, child, &format!("{inner}  "), None);
        } else {
            lines.push(format!("{inner}- {}", scalar(child)));
        }
    }
}

fn render_container(lines: &mut Vec<String>, item: &Value, indent: &str, label: Option<&str>) {
    if let Some(label) = label {
        lines.push(format!("{indent}{label}:"));
    }
    let inner = if label.is_some() {
        format!("{indent}  ")
    } else {
        indent.to_string()
    };
    match item {
        Value::Array(items) => render_array(lines, items, &inner),
        Value::Object(map) => {
            for (key, child) in jsrt::json::object_entries(map) {
                render(lines, child, &inner, Some(key));
            }
        }
        _ => {}
    }
}

fn render(lines: &mut Vec<String>, item: &Value, indent: &str, label: Option<&str>) {
    let head = match label {
        None => indent.to_string(),
        Some(label) => format!("{indent}{label}: "),
    };
    let empty = match item {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => {
            lines.push(format!("{head}{}", scalar(item)));
            return;
        }
    };
    if empty {
        lines.push(format!("{head}(none)"));
        return;
    }
    render_container(lines, item, indent, label);
}

/// `formatInspection`: JSON as indented `key: value` lines under `title`, at most 40 lines and a count of the rest,
/// so a large record cannot flood the screen.
pub fn format_inspection(title: &str, value: &Value) -> String {
    let mut lines = vec![title.to_string()];
    render(&mut lines, value, "", None);
    if lines.len() <= MAX_LINES {
        return lines.join("\n");
    }
    let more = lines.len() - MAX_LINES;
    lines.truncate(MAX_LINES);
    lines.push(format!("… {more} more lines"));
    lines.join("\n")
}
