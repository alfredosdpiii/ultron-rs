use super::{
    BranchView, CONTEXT_NOTE_CUSTOM_TYPE, ContextControl, TaskLine, collapsed_cell_text, message_text, note_text,
    select_prune_candidates, trimmed_result_text,
};
use serde_json::{Map, Value, json};
use session_journal::context::{CONTEXT_EDIT_CUSTOM_TYPE, context_edits_of};
use std::collections::HashSet;

const COLLAPSE_MIN_BYTES: usize = 600;
const MAX_PENDING_CELLS: usize = 64;
const TRIM_MIN_CHARS: usize = 8192;
const TRIM_TARGET: f64 = 0.9;
const KEPT_ITEM_CHARS: usize = 4000;
const KEPT_TOTAL_CHARS: i64 = 16000;
pub const CONTEXT_KEPT_MESSAGE_TYPE: &str = "ultron-context-kept";

fn set_cell(cells: &mut Vec<(String, Vec<TaskLine>)>, id: &str, lines: Vec<TaskLine>) {
    match cells.iter_mut().find(|(known, _)| known == id) {
        Some(slot) => slot.1 = lines,
        None => cells.push((id.to_string(), lines)),
    }
}

fn collect_trim_edits(view: &BranchView) -> (Vec<Value>, i64) {
    let mut edits = Vec::new();
    let mut saved = 0i64;
    let mut answered_after = false;
    for entry in view.window().iter().rev() {
        if entry["type"] != "message" {
            continue;
        }
        let message = &entry["message"];
        if message["role"] == "assistant" {
            answered_after = true;
            continue;
        }
        if message["role"] != "toolResult" || !answered_after {
            continue;
        }
        if let Some((edit, delta)) = trim_tool_result(view, entry, message) {
            saved += delta;
            edits.push(edit);
        }
    }
    (edits, saved)
}

fn trim_tool_result(view: &BranchView, entry: &Value, message: &Value) -> Option<(Value, i64)> {
    let id = entry["id"].as_str().unwrap_or_default();
    if view.pinned(id) || view.edit(id).is_some() || view.protected.contains(id) {
        return None;
    }
    if !message["content"]
        .as_array()
        .is_some_and(|parts| parts.iter().all(|part| part["type"] == "text"))
    {
        return None;
    }
    let original = message_text(message);
    if jsrt::utf16::len(&original) <= TRIM_MIN_CHARS {
        return None;
    }
    let content = trimmed_result_text(id, &original);
    let delta = (jsrt::utf16::len(&original) as i64 - jsrt::utf16::len(&content) as i64).div_euclid(4);
    Some((json!({"targetId": id, "replacement": {"content": content}}), delta))
}

fn format_kept_entry(
    view: &BranchView,
    entry: &Value,
    omitted: &HashSet<String>,
    retained: &HashSet<String>,
) -> Option<String> {
    let id = entry["id"].as_str().unwrap_or_default();
    if entry["type"] == "custom" && entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE && !omitted.contains(id) {
        return Some(format!("- note {id}: {}", note_text(entry)));
    }
    if view.pinned(id) && entry["type"] == "message" && !retained.contains(&jsrt::json::stringify(&entry["message"])) {
        let text = message_text(&entry["message"]);
        let text = if jsrt::utf16::len(&text) > KEPT_ITEM_CHARS {
            format!("{}…", jsrt::utf16::head(&text, KEPT_ITEM_CHARS - 1))
        } else {
            text
        };
        return Some(format!(
            "- pinned {id} ({}): {text}",
            entry["message"]["role"].as_str().unwrap_or("undefined")
        ));
    }
    None
}

fn append_bounded_line(lines: &mut Vec<String>, budget: &mut i64, line: String) {
    if *budget <= 0 {
        return;
    }
    let bounded = if jsrt::utf16::len(&line) as i64 > *budget {
        format!("{}…", jsrt::utf16::head(&line, *budget as usize - 1))
    } else {
        line
    };
    *budget -= jsrt::utf16::len(&bounded) as i64;
    lines.push(bounded);
}

impl ContextControl {
    /// Harness `tool_start`: a root `rlm` cell starts; finished tasks not yet attributed belong to it.
    pub fn tool_start(&self, lane: &str, tool_name: &str, tool_call_id: &str) {
        if lane != kernel::ROOT_LANE || tool_name != "rlm" {
            return;
        }
        let mut tracking = jsrt::sync::lock(&self.tracking);
        tracking.current_cell = Some(tool_call_id.to_string());
        if !tracking.unattributed.is_empty() {
            let lines = std::mem::take(&mut tracking.unattributed);
            set_cell(&mut tracking.cells, tool_call_id, lines);
        }
    }

    pub fn tool_end(&self, lane: &str, tool_call_id: &str) {
        let mut tracking = jsrt::sync::lock(&self.tracking);
        if lane == kernel::ROOT_LANE && tracking.current_cell.as_deref() == Some(tool_call_id) {
            tracking.current_cell = None;
        }
    }

    /// Harness `message_end`: once the root model answered after a cell's output, collapse it.
    pub fn message_end(&self, lane: &str, role: &str) -> Result<(), String> {
        if lane == kernel::ROOT_LANE && role == "assistant" && !jsrt::sync::lock(&self.tracking).cells.is_empty() {
            self.collapse()?;
        }
        Ok(())
    }

    pub fn run_end(&self, lane: &str) -> Result<(), String> {
        if lane == kernel::ROOT_LANE && !jsrt::sync::lock(&self.tracking).cells.is_empty() {
            self.collapse()?;
        }
        Ok(())
    }

    /// A task's terminal result is durable (`NativeTask` JSON). Root-level tasks are attributed to the root cell
    /// that returns them.
    pub fn task_ended(&self, task: &Value, cost: Option<f64>) {
        if task.get("parentId").is_some() {
            return;
        }
        let text = |key: &str| task[key].as_str().unwrap_or_default().to_string();
        let status = task["result"]["status"]
            .as_str()
            .map_or_else(|| text("state"), str::to_string);
        let line = TaskLine {
            id: text("id"),
            definition: text("definition"),
            key: text("key"),
            status,
            cost,
        };
        let mut tracking = jsrt::sync::lock(&self.tracking);
        match tracking.current_cell.clone() {
            Some(cell) => {
                let mut lines = tracking
                    .cells
                    .iter()
                    .find(|(id, _)| *id == cell)
                    .map_or_default(|(_, lines)| lines.clone());
                lines.push(line);
                set_cell(&mut tracking.cells, &cell, lines);
            }
            None => {
                tracking.unattributed.push(line);
                if tracking.unattributed.len() > MAX_PENDING_CELLS {
                    tracking.unattributed.remove(0);
                }
            }
        }
        while tracking.cells.len() > MAX_PENDING_CELLS {
            tracking.cells.remove(0);
        }
    }

    /// Harness `entry_added`: drops a pending copy of the entry; for a context edit, the event observers receive.
    pub fn entry_added(&self, lane: &str, entry: &Value) -> Option<Value> {
        if entry["type"] != "custom" {
            return None;
        }
        jsrt::sync::lock(&self.pending).retain(|(_, pending)| pending["id"] != entry["id"]);
        if entry["customType"] != CONTEXT_EDIT_CUSTOM_TYPE {
            return None;
        }
        let data = if entry["data"].is_object() {
            &entry["data"]
        } else {
            &Value::Null
        };
        let mut event = Map::new();
        event.insert("lane".into(), lane.into());
        event.insert("editId".into(), entry["id"].clone());
        event.insert("source".into(), data["source"].as_str().unwrap_or("unknown").into());
        if let Some(reason) = data["reason"].as_str() {
            event.insert("reason".into(), reason.into());
        }
        let edits: Vec<Value> = context_edits_of(entry)
            .into_iter()
            .map(|edit| {
                let action = if edit.replacement.is_none() { "omit" } else { "replace" };
                json!({"targetId": edit.target_id, "action": action})
            })
            .collect();
        event.insert("edits".into(), edits.into());
        if let Some(tasks) = data["tasks"].as_array() {
            let tasks: Vec<Value> = tasks
                .iter()
                .map(|task| match task {
                    Value::String(text) => Value::String(text.clone()),
                    other => Value::String(jsrt::json::stringify(other)),
                })
                .collect();
            event.insert("tasks".into(), tasks.into());
        }
        Some(Value::Object(event))
    }

    fn try_collapse_tool_result(
        &self,
        view: &BranchView,
        entry: &Value,
        message: &Value,
    ) -> Option<(Value, Vec<String>)> {
        let call = message["toolCallId"].as_str().unwrap_or_default();
        let lines = {
            let mut tracking = jsrt::sync::lock(&self.tracking);
            let index = tracking.cells.iter().position(|(id, _)| id == call)?;
            tracking.cells.remove(index).1
        };
        let id = entry["id"].as_str().unwrap_or_default();
        if view.pinned(id) || view.edit(id).is_some() {
            return None;
        }
        let original = message_text(message);
        if original.len() <= COLLAPSE_MIN_BYTES {
            return None;
        }
        let content = collapsed_cell_text(id, &original, &lines);
        if jsrt::utf16::len(&content) >= jsrt::utf16::len(&original) {
            return None;
        }
        let task_ids = lines.into_iter().map(|line| line.id).collect();
        Some((json!({"targetId": id, "replacement": {"content": content}}), task_ids))
    }

    fn collect_collapse_edits(&self, view: &BranchView) -> (Vec<Value>, Vec<String>) {
        let mut edits = Vec::new();
        let mut tasks = Vec::new();
        let mut answered_after = false;
        for entry in view.window().iter().rev() {
            if entry["type"] != "message" {
                continue;
            }
            let message = &entry["message"];
            if message["role"] == "assistant" {
                answered_after = true;
                continue;
            }
            if message["role"] == "toolResult"
                && answered_after
                && let Some((edit, task_ids)) = self.try_collapse_tool_result(view, entry, message)
            {
                edits.push(edit);
                tasks.extend(task_ids);
            }
        }
        (edits, tasks)
    }

    /// Collapse cell outputs the model has already answered after.
    fn collapse(&self) -> Result<(), String> {
        if jsrt::sync::lock(&self.tracking).cells.is_empty() {
            return Ok(());
        }
        let view = self.view(kernel::ROOT_LANE)?;
        let (mut edits, tasks) = self.collect_collapse_edits(&view);
        if edits.is_empty() {
            return Ok(());
        }
        edits.reverse();
        self.append(
            kernel::ROOT_LANE,
            CONTEXT_EDIT_CUSTOM_TYPE,
            json!({"edits": edits, "source": "collapse", "reason": "task results returned", "tasks": tasks}),
        )?;
        Ok(())
    }

    /// Harness `before_compaction`: for a threshold compaction, trims tool results the model answered after when
    /// that brings the estimate under 90% of the threshold; true declines the compaction.
    pub fn before_compaction(
        &self,
        lane: &str,
        reason: &str,
        tokens_before: i64,
        context_window: Option<i64>,
        reserve_tokens: i64,
    ) -> Result<bool, String> {
        if !self.trim || reason != "threshold" {
            return Ok(false);
        }
        self.known(lane);
        let Some(window) = context_window else {
            return Ok(false);
        };
        self.trim_instead(lane, tokens_before, window - reserve_tokens)
    }

    fn trim_instead(&self, lane: &str, tokens: i64, threshold: i64) -> Result<bool, String> {
        let view = self.view(lane)?;
        let target = threshold as f64 * TRIM_TARGET;
        let last_assistant = view
            .window()
            .iter()
            .rev()
            .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
            .and_then(|entry| entry["id"].as_str().map(str::to_string));
        let credit = jsrt::sync::lock(&self.tracking)
            .trimmed
            .iter()
            .find(|(name, assistant, _)| name == lane && Some(assistant) == last_assistant.as_ref())
            .map_or(0, |(_, _, saved)| *saved);
        if (tokens - credit) as f64 <= target {
            return Ok(true);
        }
        let (mut edits, saved) = collect_trim_edits(&view);
        let Some(last_assistant) = last_assistant else {
            return Ok(false);
        };
        if edits.is_empty() || (tokens - credit - saved) as f64 > target {
            if !self.cache_prune {
                return Ok(false);
            }
            let needed_tokens = ((tokens - credit) as f64 - target).ceil().max(1.0) as usize;
            let freed_bytes = self.prune_before_compaction(lane, needed_tokens.saturating_mul(4))?;
            let pruned_tokens = (freed_bytes / 4) as i64;
            if pruned_tokens <= 0 || (tokens - credit - pruned_tokens) as f64 > target {
                return Ok(false);
            }
            let mut tracking = jsrt::sync::lock(&self.tracking);
            tracking.trimmed.retain(|(name, _, _)| name != lane);
            tracking
                .trimmed
                .push((lane.to_string(), last_assistant, credit + pruned_tokens));
            return Ok(true);
        }
        edits.reverse();
        self.append(
            lane,
            CONTEXT_EDIT_CUSTOM_TYPE,
            json!({"edits": edits, "source": "trim", "reason": "trimmed old tool results instead of compacting"}),
        )?;
        let mut tracking = jsrt::sync::lock(&self.tracking);
        tracking.trimmed.retain(|(name, _, _)| name != lane);
        tracking
            .trimmed
            .push((lane.to_string(), last_assistant, credit + saved));
        Ok(true)
    }

    /// Cache-aware soft prune before full compaction: forgets editable tool results ranked by
    /// `bytes / (cache_impact_bytes + 1)` until `target_bytes` is reclaimed. Returns bytes freed.
    pub fn prune_before_compaction(&self, lane: &str, target_bytes: usize) -> Result<usize, String> {
        if target_bytes == 0 {
            return Ok(0);
        }
        self.known(lane);
        let view = self.view(lane)?;
        let kinds = HashSet::from(["tool"]);
        let chosen = select_prune_candidates(&view, &kinds, Some(target_bytes));
        if chosen.is_empty() {
            return Ok(0);
        }
        let before = Self::visible_bytes(&view);
        let edits: Vec<Value> = chosen
            .iter()
            .map(|c| json!({"targetId": c.id, "replacement": null}))
            .collect();
        self.append(
            lane,
            CONTEXT_EDIT_CUSTOM_TYPE,
            json!({"edits": edits, "source": "ctx.prune", "reason": "cache-aware pre-compaction prune"}),
        )?;
        let after = Self::visible_bytes(&self.view(lane)?);
        Ok(before.saturating_sub(after))
    }

    /// Notes and pinned items that the newest compaction cut off, as one bounded text block.
    pub fn kept_across_compaction(&self, lane: &str) -> Result<Option<String>, String> {
        let view = self.view(lane)?;
        let Some(index) = view.compaction_index else {
            return Ok(None);
        };
        let retained: HashSet<String> = view.path[index]["retainedTail"]
            .as_array()
            .map_or_default(|tail| tail.iter().map(jsrt::json::stringify).collect());
        let before = &view.path[..index];
        let omitted: HashSet<String> = before
            .iter()
            .flat_map(context_edits_of)
            .filter(|edit| edit.replacement.is_none())
            .map(|edit| edit.target_id)
            .collect();
        let mut lines: Vec<String> = Vec::new();
        let mut budget = KEPT_TOTAL_CHARS;
        for entry in before {
            if let Some(line) = format_kept_entry(&view, entry, &omitted, &retained) {
                append_bounded_line(&mut lines, &mut budget, line);
            }
        }
        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "Notes and pinned items kept across compaction (ctx.note / ctx.pin):\n{}",
            lines.join("\n")
        )))
    }

    /// Harness `transform_context`: after a compaction summary, shows again the notes and pinned items it cut off
    /// (only for lanes that used ctx, and the root). `None` leaves the messages unchanged.
    pub fn transform_context(&self, lane: &str, messages: &[Value]) -> Result<Option<Vec<Value>>, String> {
        let Some(index) = messages
            .iter()
            .position(|message| message["role"] == "compactionSummary")
        else {
            return Ok(None);
        };
        if !jsrt::sync::lock(&self.tracking).lanes.contains(lane) {
            return Ok(None);
        }
        let Some(kept) = self.kept_across_compaction(lane)? else {
            return Ok(None);
        };
        let mut out = messages.to_vec();
        out.insert(
            index + 1,
            json!({"role": "custom", "customType": CONTEXT_KEPT_MESSAGE_TYPE, "content": kept, "display": false,
                "timestamp": (self.clock)()}),
        );
        Ok(Some(out))
    }
}
