//! `ultron --mode rpc`: Pi's JSONL protocol on stdin/stdout (`modes/rpc/rpc-types.ts`) as the native command serves
//! it (`experimental/rpc-native.ts`). Commands are JSON lines with a `type` and an optional `id`; each gets one
//! `{id, type: "response", command, success, data | error}` line, and lane events stream as Pi session events.
//!
//! The command layer ([`handle`]) parses and validates commands and shapes responses; everything a command does
//! to a session goes through [`Remote`]. [`remote`] is the session as `rpc-native.ts` reaches it: through this
//! process's server generation, attached to the session worker that runs it.

pub mod client;
pub mod events;
pub mod jsonl;
pub mod remote;

use protocol::watch::QueueKind;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use remote::Remote;

/// The stdout line writer, shared by the command loop and the threads that answer later (bash).
#[derive(Clone)]
pub struct Output(Arc<Mutex<Box<dyn Write + Send>>>);

impl Output {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Output(Arc::new(Mutex::new(writer)))
    }

    pub fn emit(&self, value: &Value) {
        let mut writer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writer.write_all(jsonl::serialize_json_line(value).as_bytes());
        let _ = writer.flush();
    }
}

/// What the command loop reacts to: a stdin record, the end of stdin, or an event of the session's own work.
pub enum Input {
    Line(String),
    Eof,
    Event(remote::Event),
}

/// A command that answers after the handler returns (`bash`): it writes its response line itself.
pub struct Reply {
    id: Option<Value>,
    command: String,
    out: Output,
}

impl Reply {
    pub fn send(self, result: Result<Option<Value>, String>) {
        self.out.emit(&match result {
            Ok(data) => success(self.id.as_ref(), &self.command, data),
            Err(message) => failure(self.id.as_ref(), Some(&self.command), &message),
        });
    }
}

/// The arguments of a `/goal` prompt (`None`: not a `/goal` command).
fn goal_arguments(message: &str) -> Option<&str> {
    let rest = message.trim().strip_prefix("/goal")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

fn success(id: Option<&Value>, command: &str, data: Option<Value>) -> Value {
    let mut out = Map::new();
    if let Some(id) = id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), "response".into());
    out.insert("command".into(), command.into());
    out.insert("success".into(), true.into());
    if let Some(data) = data {
        out.insert("data".into(), data);
    }
    Value::Object(out)
}

/// The error response; `command` is omitted when the command had no string `type`.
fn failure(id: Option<&Value>, command: Option<&str>, message: &str) -> Value {
    let mut out = Map::new();
    if let Some(id) = id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), "response".into());
    if let Some(command) = command {
        out.insert("command".into(), command.into());
    }
    out.insert("success".into(), false.into());
    out.insert("error".into(), message.into());
    Value::Object(out)
}

/// `promptRequest`: the message text and any images.
fn prompt_request(command: &Value, kind: &str) -> Result<(String, Vec<Value>), String> {
    let message = command
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{kind} requires a message"))?;
    let images = command
        .get("images")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((message.to_string(), images))
}

fn queue_mode(mode: Option<&Value>) -> Result<&str, String> {
    mode.and_then(Value::as_str)
        .filter(|mode| config::keys::QUEUE_MODES.contains(mode))
        .ok_or_else(|| format!("Invalid queue mode: {}", jsrt::value::string(mode)))
}

/// One command: `Ok(None)` when the response comes later (or was already written).
fn run_command(
    session: &mut Remote,
    command: &Value,
    kind: &str,
    out: &Output,
) -> Result<Option<Option<Value>>, String> {
    let str_field = |key: &str| command.get(key).and_then(Value::as_str);
    let done = |data: Option<Value>| Ok(Some(data));
    match kind {
        "prompt" => {
            let (message, images) = prompt_request(command, kind)?;
            // `/goal ...` is the session goal's command, as in the TUI: answered with its text, never a turn.
            if let Some(rest) = message.trim().strip_prefix("/observer")
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                return done(Some(json!({"text": session.observer(rest.trim())?})));
            }
            if let Some(args) = goal_arguments(&message) {
                return done(Some(json!({"text": session.goal(args)?})));
            }
            // `/persona ...` and `/keepgoing ...` are the session's toggles, never a turn.
            if let Some((command, args)) =
                resources::keep_going::toggle_command(&message).filter(|_| message.trim_start().starts_with('/'))
            {
                return done(Some(session.persona(command, &args)?));
            }
            if session.is_running() {
                let queue = match str_field("streamingBehavior") {
                    Some("steer") => QueueKind::Steer,
                    Some("followUp") => QueueKind::FollowUp,
                    _ => {
                        return Err("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".into());
                    }
                };
                session.enqueue(queue, message, images)?;
                return done(None);
            }
            session.prompt(message, images)?;
            done(None)
        }
        "steer" | "follow_up" => {
            let (message, images) = prompt_request(command, kind)?;
            let queue = if kind == "steer" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            session.enqueue(queue, message, images)?;
            done(None)
        }
        "abort" => {
            session.abort();
            done(None)
        }
        "clear_queue" => done(Some(session.clear_queue())),
        "new_session" => {
            session.new_session()?;
            done(Some(json!({"cancelled": false})))
        }
        "get_state" => done(Some(session.state())),
        "set_model" => {
            let provider = jsrt::value::string(command.get("provider"));
            let model_id = jsrt::value::string(command.get("modelId"));
            done(Some(session.set_model(&provider, &model_id)?))
        }
        "cycle_model" => done(Some(session.cycle_model()?.unwrap_or(Value::Null))),
        "get_available_models" => done(Some(json!({"models": session.available_models()}))),
        "set_thinking_level" => {
            let level = jsrt::value::string(command.get("level"));
            session.set_thinking_level(&level)?;
            done(None)
        }
        "cycle_thinking_level" => match session.cycle_thinking_level()? {
            Some(level) => done(Some(json!({"level": level}))),
            None => done(Some(Value::Null)),
        },
        "get_available_thinking_levels" => done(Some(json!({"levels": session.thinking_levels()}))),
        "set_steering_mode" | "set_follow_up_mode" => {
            let mode = queue_mode(command.get("mode"))?;
            let queue = if kind == "set_steering_mode" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            session.set_queue_mode(queue, mode)?;
            done(None)
        }
        "compact" => {
            // Answered when the compaction ends: an `abort` or `get_state` sent meanwhile is served at once.
            let reply = Reply {
                id: command.get("id").cloned(),
                command: kind.into(),
                out: out.clone(),
            };
            session.compact(str_field("customInstructions"), reply)?;
            Ok(None)
        }
        "set_auto_compaction" => {
            session.set_auto_compaction(command.get("enabled") == Some(&Value::Bool(true)));
            done(None)
        }
        "set_auto_retry" => {
            session.set_auto_retry(command.get("enabled") == Some(&Value::Bool(true)));
            done(None)
        }
        "abort_retry" => {
            session.abort_retry();
            done(None)
        }
        "bash" => {
            let text = str_field("command").ok_or("bash requires a command")?;
            let reply = Reply {
                id: command.get("id").cloned(),
                command: kind.into(),
                out: out.clone(),
            };
            session.bash(
                text.to_string(),
                command.get("excludeFromContext") == Some(&Value::Bool(true)),
                reply,
            )?;
            Ok(None)
        }
        "abort_bash" => {
            session.abort_bash();
            done(None)
        }
        "get_session_stats" => done(Some(session.session_stats())),
        "switch_session" => {
            let selector = str_field("sessionPath").ok_or("switch_session requires sessionPath")?;
            session.switch_session(selector)?;
            done(Some(json!({"cancelled": false})))
        }
        "fork" => {
            let entry_id = str_field("entryId").ok_or("fork requires entryId")?;
            let text = session.fork(entry_id)?;
            done(Some(json!({"text": text, "cancelled": false})))
        }
        "clone" => {
            session.clone_session()?;
            done(Some(json!({"cancelled": false})))
        }
        "get_fork_messages" => done(Some(json!({"messages": session.fork_messages()}))),
        "get_entries" => {
            let (entries, leaf) = session.entries(str_field("since"))?;
            done(Some(json!({"entries": entries, "leafId": leaf})))
        }
        "get_tree" => {
            let (tree, leaf) = session.tree();
            done(Some(json!({"tree": tree, "leafId": leaf})))
        }
        "get_last_assistant_text" => done(Some(json!({"text": session.last_assistant_text()}))),
        "set_session_name" => {
            let name = str_field("name").map(jsrt::string::trim).unwrap_or_default();
            if name.is_empty() {
                return Err("Session name cannot be empty".into());
            }
            session.set_session_name(name)?;
            done(None)
        }
        "get_messages" => done(Some(json!({"messages": session.messages()}))),
        "get_commands" => done(Some(json!({"commands": session.commands()?}))),
        "observer" => done(Some(
            json!({"text": session.observer(str_field("args").unwrap_or_default())?}),
        )),
        "goal" => {
            let args = str_field("args").unwrap_or_default();
            done(Some(json!({"text": session.goal(args)?})))
        }
        // Ultron extension: the session's persona toggle and keep-going mode (`args`: on|off|auto, empty reports).
        "persona" => done(Some(session.persona("persona", str_field("args").unwrap_or_default())?)),
        "keep_going" => done(Some(
            session.persona("keepgoing", str_field("args").unwrap_or_default())?,
        )),
        // Ultron extension: the read-only inspector behind /agents, /memory why, /skills why, /experiments.
        "inspect" => {
            let request = str_field("request").ok_or("inspect requires a request type")?;
            let payload = command.get("payload").cloned().unwrap_or_else(|| json!({}));
            done(Some(session.inspect(request, payload)?))
        }
        "export_html" => {
            let path = session.export_html(str_field("outputPath"))?;
            done(Some(json!({ "path": path })))
        }
        other => Err(format!("Unknown command: {other}")),
    }
}

/// `handleLine`: parse one record and answer it. Extension UI responses go to the session's worker (unknown ids are
/// ignored there) and are not answered.
pub fn handle_line(session: &mut Remote, line: &str, out: &Output) {
    let command: Value = match serde_json::from_str(line) {
        Ok(command) => command,
        Err(error) => {
            out.emit(&failure(
                None,
                Some("parse"),
                &format!("Failed to parse command: {error}"),
            ));
            return;
        }
    };
    let kind = command.get("type").and_then(Value::as_str);
    if kind == Some("extension_ui_response") {
        // Fire-and-forget, as in Pi: unknown ids are ignored and nothing is written back.
        let answer = if command["cancelled"] == true {
            Some(json!({"cancelled": true}))
        } else if let Some(confirmed) = command["confirmed"].as_bool() {
            Some(json!({"confirmed": confirmed}))
        } else {
            command["value"].as_str().map(|value| json!({"value": value}))
        };
        if let (Some(id), Some(answer)) = (command["id"].as_str(), answer) {
            session.extension_ui_response(id, answer);
        }
        return;
    }
    let id = command.get("id");
    let Some(kind) = kind else {
        let shown = jsrt::value::string(command.get("type"));
        out.emit(&failure(id, None, &format!("Unknown command: {shown}")));
        return;
    };
    match run_command(session, &command, kind, out) {
        Ok(Some(data)) => out.emit(&success(id, kind, data)),
        Ok(None) => {}
        Err(message) => out.emit(&failure(id, Some(kind), &message)),
    }
}

/// Serve commands until stdin ends, then let in-flight work finish and release the session.
pub fn serve(session: &mut Remote, inputs: &Receiver<Input>, out: &Output) {
    let mut ended = false;
    while let Ok(input) = inputs.recv() {
        match input {
            Input::Line(line) => {
                if !ended {
                    handle_line(session, &line, out);
                }
            }
            Input::Eof => {
                ended = true;
                session.input_ended();
            }
            Input::Event(event) => session.deliver(event, out),
        }
        if ended && !session.busy() {
            break;
        }
    }
    session.close();
}
