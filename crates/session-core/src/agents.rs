//! What the session adds to the typed-agent host's `agents.status` (session-worker.ts `statusExtras`): the shell
//! jobs and the REPL tool calls the calling lane may see (the root lane all of them, another lane its own), bounded,
//! for the TUI's graph view (its job and tool nodes).

use serde_json::{Map, Value, json};

/// `statusExtras(caller)`: `{jobs, toolCalls}` for `lane`.
pub fn status_extras(host: &host::Host, tool_calls: &kernel_apis::ToolCalls, lane: &str) -> Map<String, Value> {
    let jobs: Vec<Value> = host
        .job_records(lane)
        .into_iter()
        .take(20)
        .map(|job| {
            json!({
                "id": job.id,
                "lane": job.lane,
                "command": jsrt::utf16::head(&job.command, 200),
                "status": job.status.as_str(),
                "exitCode": job.exit_code,
                "startedAt": job.started_at,
                "endedAt": job.ended_at,
                "outputBytes": job.output_bytes,
                // Last output, bounded, for the TUI graph's job details.
                "tail": jsrt::utf16::tail(&job.tail, 160),
            })
        })
        .collect();
    let calls: Vec<Value> = tool_calls
        .list()
        .into_iter()
        .filter(|call| lane == session_journal::run::MAIN_LANE || call["lane"] == lane)
        .take(40)
        .map(|call| {
            let mut out = Map::new();
            for key in [
                "id",
                "lane",
                "source",
                "name",
                "label",
                "status",
                "startedAt",
                "endedAt",
                "input",
            ] {
                out.insert(key.into(), call.get(key).cloned().unwrap_or(Value::Null));
            }
            for key in ["preview", "error"] {
                if let Some(value) = call.get(key).filter(|value| !value.is_null()) {
                    out.insert(key.into(), value.clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    let mut extras = Map::new();
    extras.insert("jobs".into(), Value::Array(jobs));
    extras.insert("toolCalls".into(), Value::Array(calls));
    extras
}
