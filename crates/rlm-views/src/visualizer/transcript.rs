use std::collections::HashMap;

use serde_json::Value;

use super::{CellStatus, RlmClock, RlmRootCell, RlmTurn, one_line};

/// A running tool of the open operation (`LaneSnapshot.operation.runningTools`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunningTool {
    /// `running` while it runs.
    pub status: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub result: Value,
    pub is_error: bool,
}

/// What the cell extractors read: the transcript entries (`{message, timestamp?}`, or bare messages), borrowed, and
/// the open operation's running tools (`None`: no operation). The TUI builds one every frame, so it copies nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptView<'a> {
    entries: &'a [Value],
    bare: bool,
    pub running_tools: Option<Vec<RunningTool>>,
}

impl<'a> TranscriptView<'a> {
    /// From transcript entries (`{message, timestamp?}`); `running` is the open operation's tools.
    pub fn from_entries(entries: &'a [Value], running: Option<Vec<RunningTool>>) -> TranscriptView<'a> {
        TranscriptView {
            entries,
            bare: false,
            running_tools: running,
        }
    }

    /// From the TUI's Pi messages; `running` is the open operation's tools.
    pub fn from_messages(messages: &'a [Value], running: Option<Vec<RunningTool>>) -> TranscriptView<'a> {
        TranscriptView {
            entries: messages,
            bare: true,
            running_tools: running,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn message(&self, index: usize) -> &'a Value {
        let entry = &self.entries[index];
        if self.bare { entry } else { &entry["message"] }
    }

    /// The entry's own timestamp (a bare message has none).
    fn timestamp(&self, index: usize) -> &'a Value {
        if self.bare {
            &Value::Null
        } else {
            &self.entries[index]["timestamp"]
        }
    }
}

fn code_of(args: &Value) -> String {
    args["code"].as_str().unwrap_or("").to_string()
}

fn text_of(result: &Value) -> Option<String> {
    let content = result["content"].as_array()?;
    let text: String = content
        .iter()
        .map(|p| {
            if p["type"] == "text" {
                p["text"].as_str().unwrap_or("")
            } else {
                ""
            }
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

fn tool_cell_status(active: bool, is_error: bool) -> CellStatus {
    if active {
        CellStatus::Running
    } else if is_error {
        CellStatus::Error
    } else {
        CellStatus::Ok
    }
}

fn find_tool_call_code(view: &TranscriptView<'_>, before: usize, id: &str) -> (String, Option<f64>) {
    for back in (0..before).rev() {
        let assistant = view.message(back);
        if assistant["role"] != "assistant" {
            continue;
        }
        let Some(content) = assistant["content"].as_array() else {
            continue;
        };
        let Some(call) = content.iter().find(|p| p["type"] == "toolCall" && p["id"] == id) else {
            continue;
        };
        return (code_of(&call["arguments"]), assistant["timestamp"].as_f64());
    }
    (String::new(), None)
}

/// The current or last root `rlm` cell.
pub fn extract_root_cell(view: Option<&TranscriptView<'_>>, clock: &mut RlmClock, now: f64) -> Option<RlmRootCell> {
    let view = view?;
    let running = view
        .running_tools
        .as_ref()
        .and_then(|tools| tools.iter().rev().find(|t| t.tool_name == "rlm"));
    if let Some(tool) = running {
        let active = tool.status == "running";
        let timing = clock.mark(&format!("cell:{}", tool.tool_call_id), active, now, None);
        return Some(RlmRootCell {
            tool_call_id: tool.tool_call_id.clone(),
            code: code_of(&tool.args),
            status: tool_cell_status(active, tool.is_error),
            started_at: timing.started_at,
            ended_at: timing.ended_at,
            output: if active { None } else { text_of(&tool.result) },
        });
    }
    for index in (0..view.len()).rev() {
        let message = view.message(index);
        if message["role"] != "toolResult" || message["toolName"] != "rlm" {
            continue;
        }
        let Some(id) = message["toolCallId"].as_str() else {
            continue;
        };
        let (code, started) = find_tool_call_code(view, index, id);
        let observed = clock.mark(&format!("cell:{id}"), false, now, None);
        return Some(RlmRootCell {
            tool_call_id: id.to_string(),
            code,
            status: tool_cell_status(false, message["isError"] == true),
            started_at: observed.started_at.or(started),
            ended_at: observed.ended_at.or(message["timestamp"].as_f64()),
            output: text_of(message),
        });
    }
    None
}

fn user_turn_at(view: &TranscriptView<'_>, start: usize) -> RlmTurn {
    let message = view.message(start);
    let text = match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| {
                if p["type"] == "text" {
                    p["text"].as_str().unwrap_or("")
                } else {
                    ""
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let at = if view.timestamp(start).is_number() {
        view.timestamp(start).as_f64()
    } else {
        message["timestamp"].as_f64()
    };
    RlmTurn {
        started_at: at,
        prompt: Some(one_line(&text)),
    }
}

fn put_cell(order: &mut Vec<String>, cells: &mut HashMap<String, RlmRootCell>, cell: RlmRootCell) {
    if !cells.contains_key(&cell.tool_call_id) {
        order.push(cell.tool_call_id.clone());
    }
    cells.insert(cell.tool_call_id.clone(), cell);
}

fn collect_message_cells(
    message: &Value,
    clock: &mut RlmClock,
    now: f64,
    order: &mut Vec<String>,
    cells: &mut HashMap<String, RlmRootCell>,
) {
    if message["role"] == "assistant"
        && let Some(content) = message["content"].as_array()
    {
        for part in content {
            if part["type"] != "toolCall" || part["name"] != "rlm" {
                continue;
            }
            let Some(id) = part["id"].as_str() else { continue };
            let observed = clock.mark(&format!("cell:{id}"), false, now, None);
            put_cell(
                order,
                cells,
                RlmRootCell {
                    tool_call_id: id.to_string(),
                    code: code_of(&part["arguments"]),
                    status: CellStatus::Running,
                    started_at: observed.started_at.or(message["timestamp"].as_f64()),
                    ended_at: None,
                    output: None,
                },
            );
        }
    } else if message["role"] == "toolResult"
        && message["toolName"] == "rlm"
        && let Some(id) = message["toolCallId"].as_str()
    {
        let known = cells.get(id).cloned();
        let observed = clock.mark(&format!("cell:{id}"), false, now, None);
        put_cell(
            order,
            cells,
            RlmRootCell {
                tool_call_id: id.to_string(),
                code: known.as_ref().map_or_default(|c| c.code.clone()),
                status: tool_cell_status(false, message["isError"] == true),
                started_at: known.and_then(|c| c.started_at),
                ended_at: observed.ended_at.or(message["timestamp"].as_f64()),
                output: text_of(message),
            },
        );
    }
}

fn collect_running_tool_cell(
    tool: &RunningTool,
    clock: &mut RlmClock,
    now: f64,
    order: &mut Vec<String>,
    cells: &mut HashMap<String, RlmRootCell>,
) {
    if tool.tool_name != "rlm" {
        return;
    }
    let active = tool.status == "running";
    let timing = clock.mark(&format!("cell:{}", tool.tool_call_id), active, now, None);
    let known = cells.get(&tool.tool_call_id).cloned();
    let code = code_of(&tool.args);
    put_cell(
        order,
        cells,
        RlmRootCell {
            tool_call_id: tool.tool_call_id.clone(),
            code: if code.is_empty() {
                known.as_ref().map_or_default(|c| c.code.clone())
            } else {
                code
            },
            status: tool_cell_status(active, tool.is_error),
            started_at: timing.started_at.or(known.and_then(|c| c.started_at)),
            ended_at: timing.ended_at,
            output: if active { None } else { text_of(&tool.result) },
        },
    );
}

/// The last user message (time and preview) and the root `rlm` cells after it, oldest first.
pub fn extract_turn(
    view: Option<&TranscriptView<'_>>,
    clock: &mut RlmClock,
    now: f64,
    max_cells: usize,
) -> (Option<RlmTurn>, Vec<RlmRootCell>) {
    let Some(view) = view else { return (None, Vec::new()) };
    let start = (0..view.len()).rposition(|i| view.message(i)["role"] == "user");
    let turn = start.map(|start| user_turn_at(view, start));
    let mut order: Vec<String> = Vec::new();
    let mut cells: HashMap<String, RlmRootCell> = HashMap::new();
    for index in start.map_or(0, |s| s + 1)..view.len() {
        collect_message_cells(view.message(index), clock, now, &mut order, &mut cells);
    }
    for tool in view.running_tools.iter().flatten() {
        collect_running_tool_cell(tool, clock, now, &mut order, &mut cells);
    }
    let open = view.running_tools.is_some();
    let list: Vec<RlmRootCell> = order
        .iter()
        .map(|id| {
            let mut cell = cells[id].clone();
            if cell.status == CellStatus::Running && !open {
                cell.status = CellStatus::Error;
                cell.output = Some(cell.output.unwrap_or_else(|| "interrupted".into()));
            }
            cell
        })
        .collect();
    let skip = list.len().saturating_sub(max_cells);
    (turn, list[skip..].to_vec())
}
