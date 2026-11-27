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

type CommandResult = Result<Option<Option<Value>>, String>;
type CommandHandler = fn(&mut Remote, &Value, &str, &Output) -> Option<CommandResult>;

fn str_field<'a>(command: &'a Value, key: &str) -> Option<&'a str> {
    command.get(key).and_then(Value::as_str)
}

fn done(data: Option<Value>) -> CommandResult {
    Ok(Some(data))
}

fn make_reply(command: &Value, kind: &str, out: &Output) -> Reply {
    Reply {
        id: command.get("id").cloned(),
        command: kind.into(),
        out: out.clone(),
    }
}

fn try_slash_prompt(session: &mut Remote, message: &str) -> Option<CommandResult> {
    if let Some(rest) = message.trim().strip_prefix("/observer")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(
            session
                .observer(rest.trim())
                .map(|text| Some(Some(json!({"text": text})))),
        );
    }
    if let Some(args) = goal_arguments(message) {
        return Some(session.goal(args).map(|text| Some(Some(json!({"text": text})))));
    }
    if let Some((command, args)) =
        resources::keep_going::toggle_command(message).filter(|_| message.trim_start().starts_with('/'))
    {
        return Some(session.persona(command, &args).map(|value| Some(Some(value))));
    }
    None
}

fn handle_prompt(session: &mut Remote, command: &Value, kind: &str) -> CommandResult {
    let (message, images) = prompt_request(command, kind)?;
    if let Some(result) = try_slash_prompt(session, &message) {
        return result;
    }
    if session.is_running() {
        let queue = match str_field(command, "streamingBehavior") {
            Some("steer") => QueueKind::Steer,
            Some("followUp") => QueueKind::FollowUp,
            _ => {
                return Err(
                    "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                        .into(),
                );
            }
        };
        session.enqueue(queue, message, images)?;
        return done(None);
    }
    session.prompt(message, images)?;
    done(None)
}

fn run_prompt_command(session: &mut Remote, command: &Value, kind: &str, _out: &Output) -> Option<CommandResult> {
    Some(match kind {
        "prompt" => handle_prompt(session, command, kind),
        "steer" | "follow_up" => {
            let queue = if kind == "steer" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            prompt_request(command, kind)
                .and_then(|(message, images)| session.enqueue(queue, message, images))
                .and_then(|()| done(None))
        }
        "abort" => {
            session.abort();
            done(None)
        }
        "clear_queue" => done(Some(session.clear_queue())),
        _ => return None,
    })
}

fn run_model_command(session: &mut Remote, command: &Value, kind: &str, _out: &Output) -> Option<CommandResult> {
    Some(match kind {
        "set_model" => {
            let provider = jsrt::value::string(command.get("provider"));
            let model_id = jsrt::value::string(command.get("modelId"));
            session.set_model(&provider, &model_id).map(|v| Some(Some(v)))
        }
        "cycle_model" => session.cycle_model().map(|v| Some(Some(v.unwrap_or(Value::Null)))),
        "get_available_models" => done(Some(json!({"models": session.available_models()}))),
        "set_thinking_level" => {
            let level = jsrt::value::string(command.get("level"));
            session.set_thinking_level(&level).and_then(|()| done(None))
        }
        "cycle_thinking_level" => session
            .cycle_thinking_level()
            .map(|level| Some(Some(level.map_or(Value::Null, |l| json!({"level": l}))))),
        "get_available_thinking_levels" => done(Some(json!({"levels": session.thinking_levels()}))),
        _ => return None,
    })
}

fn run_queue_and_retry_command(
    session: &mut Remote,
    command: &Value,
    kind: &str,
    _out: &Output,
) -> Option<CommandResult> {
    Some(match kind {
        "set_steering_mode" | "set_follow_up_mode" => {
            let queue = if kind == "set_steering_mode" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            queue_mode(command.get("mode"))
                .and_then(|mode| session.set_queue_mode(queue, mode))
                .and_then(|()| done(None))
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
        _ => return None,
    })
}

fn run_exec_command(session: &mut Remote, command: &Value, kind: &str, out: &Output) -> Option<CommandResult> {
    Some(match kind {
        "compact" => {
            let reply = make_reply(command, kind, out);
            session
                .compact(str_field(command, "customInstructions"), reply)
                .map(|()| None)
        }
        "bash" => {
            let reply = make_reply(command, kind, out);
            let exclude = command.get("excludeFromContext") == Some(&Value::Bool(true));
            str_field(command, "command")
                .ok_or_else(|| "bash requires a command".to_string())
                .and_then(|text| session.bash(text.to_string(), exclude, reply))
                .map(|()| None)
        }
        "abort_bash" => {
            session.abort_bash();
            done(None)
        }
        _ => return None,
    })
}

fn run_session_read_command(session: &mut Remote, command: &Value, kind: &str, _out: &Output) -> Option<CommandResult> {
    Some(match kind {
        "get_state" => done(Some(session.state())),
        "get_session_stats" => done(Some(session.session_stats())),
        "get_fork_messages" => done(Some(json!({"messages": session.fork_messages()}))),
        "get_entries" => session
            .entries(str_field(command, "since"))
            .map(|(entries, leaf)| Some(Some(json!({"entries": entries, "leafId": leaf})))),
        "get_tree" => {
            let (tree, leaf) = session.tree();
            done(Some(json!({"tree": tree, "leafId": leaf})))
        }
        "get_last_assistant_text" => done(Some(json!({"text": session.last_assistant_text()}))),
        "get_messages" => done(Some(json!({"messages": session.messages()}))),
        "get_commands" => session
            .commands()
            .map(|commands| Some(Some(json!({"commands": commands})))),
        _ => return None,
    })
}

fn run_session_mutate_command(
    session: &mut Remote,
    command: &Value,
    kind: &str,
    _out: &Output,
) -> Option<CommandResult> {
    Some(match kind {
        "new_session" => session.new_session().map(|()| Some(Some(json!({"cancelled": false})))),
        "switch_session" => str_field(command, "sessionPath")
            .ok_or_else(|| "switch_session requires sessionPath".to_string())
            .and_then(|selector| session.switch_session(selector))
            .map(|()| Some(Some(json!({"cancelled": false})))),
        "fork" => str_field(command, "entryId")
            .ok_or_else(|| "fork requires entryId".to_string())
            .and_then(|entry_id| session.fork(entry_id))
            .map(|text| Some(Some(json!({"text": text, "cancelled": false})))),
        "clone" => session
            .clone_session()
            .map(|()| Some(Some(json!({"cancelled": false})))),
        "set_session_name" => {
            let name = str_field(command, "name").map_or_default(jsrt::string::trim);
            if name.is_empty() {
                Err("Session name cannot be empty".into())
            } else {
                session.set_session_name(name).and_then(|()| done(None))
            }
        }
        _ => return None,
    })
}

fn run_extension_command(session: &mut Remote, command: &Value, kind: &str, _out: &Output) -> Option<CommandResult> {
    Some(match kind {
        "observer" => session
            .observer(str_field(command, "args").unwrap_or_default())
            .map(|text| Some(Some(json!({"text": text})))),
        "goal" => session
            .goal(str_field(command, "args").unwrap_or_default())
            .map(|text| Some(Some(json!({"text": text})))),
        "persona" => session
            .persona("persona", str_field(command, "args").unwrap_or_default())
            .map(|v| Some(Some(v))),
        "keep_going" => session
            .persona("keepgoing", str_field(command, "args").unwrap_or_default())
            .map(|v| Some(Some(v))),
        "inspect" => str_field(command, "request")
            .ok_or_else(|| "inspect requires a request type".to_string())
            .and_then(|request| {
                let payload = command.get("payload").cloned().unwrap_or_else(|| json!({}));
                session.inspect(request, payload)
            })
            .map(|v| Some(Some(v))),
        "export_html" => session
            .export_html(str_field(command, "outputPath"))
            .map(|path| Some(Some(json!({ "path": path })))),
        _ => return None,
    })
}

const COMMAND_HANDLERS: &[CommandHandler] = &[
    run_prompt_command,
    run_model_command,
    run_queue_and_retry_command,
    run_exec_command,
    run_session_read_command,
    run_session_mutate_command,
    run_extension_command,
];

/// One command: `Ok(None)` when the response comes later (or was already written).
fn run_command(session: &mut Remote, command: &Value, kind: &str, out: &Output) -> CommandResult {
    for handler in COMMAND_HANDLERS {
        if let Some(result) = handler(session, command, kind, out) {
            return result;
        }
    }
    Err(format!("Unknown command: {kind}"))
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
