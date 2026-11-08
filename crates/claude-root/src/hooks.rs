//! `ultron hook <event>` (hooks.ts): Claude Code hook commands for an `ultron mcp` session. Claude Code runs them
//! with the hook input as JSON on stdin; each forwards it to its session's server over the control socket and
//! prints the hook output the server returns (JSON on stdout, exit 0). Failures are quiet: the hook prints nothing
//! and exits 0, so a missing server never blocks a prompt.

use crate::control::{ControlClient, ServerQuery, control_dir, find_server};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// An `ultron hook <command>`: the Claude Code event the launcher installs it for, and the server's answer to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPrompt,
    Stop,
    /// Inside herdr only (crate::herdr_hook).
    Notification,
    /// Inside herdr only (crate::herdr_hook).
    SessionEnd,
    /// An async PostToolUse hook (`asyncRewake`) that waits in the background for the root's next runtime event
    /// while Claude Code is idle and wakes the model with it (exit 2, the events on stderr).
    WaitEvents,
}

/// The hooks every `ultron claude` session installs (hooks.ts), the ones only herdr reporting adds, and
/// [`HookEvent::WaitEvents`] when events wake the idle root.
pub const SESSION_HOOKS: [HookEvent; 3] = [HookEvent::SessionStart, HookEvent::UserPrompt, HookEvent::Stop];
pub const HERDR_HOOKS: [HookEvent; 2] = [HookEvent::Notification, HookEvent::SessionEnd];

/// How long a waiter may wait; Claude Code's hook timeout is set to match.
pub const WAIT_EVENTS_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

impl HookEvent {
    pub fn parse(command: &str) -> Option<HookEvent> {
        SESSION_HOOKS
            .into_iter()
            .chain(HERDR_HOOKS)
            .chain([HookEvent::WaitEvents])
            .find(|event| event.command() == command)
    }

    /// The `ultron hook` argument.
    pub const fn command(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::UserPrompt => "user-prompt",
            HookEvent::Stop => "stop",
            HookEvent::Notification => "notification",
            HookEvent::SessionEnd => "session-end",
            HookEvent::WaitEvents => "wait-events",
        }
    }

    /// The Claude Code hook event it runs on.
    pub fn claude_event(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPrompt => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::Notification => "Notification",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::WaitEvents => "PostToolUse",
        }
    }

    /// Claude Code's timeout for the command, in seconds.
    pub fn timeout_secs(self) -> u64 {
        match self {
            HookEvent::SessionStart | HookEvent::Stop => 30,
            HookEvent::UserPrompt => 60,
            HookEvent::Notification | HookEvent::SessionEnd => 10,
            HookEvent::WaitEvents => WAIT_EVENTS_TIMEOUT.as_secs(),
        }
    }
}

/// A SessionStart hook can run while Claude Code is still starting the server; later hooks find it running.
const SESSION_START_WAIT: Duration = Duration::from_secs(10);
const CONNECT_WAIT: Duration = Duration::from_millis(1_500);
/// Recall can take up to its own 20 s timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(50);

/// Run one hook; returns the text to print (may be empty). Never fails.
pub fn run_hook(event: &str, socket: Option<PathBuf>, input: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let Some(event) = HookEvent::parse(event) else {
        return String::new();
    };
    run_hook_inner(event, socket, input, var).unwrap_or_default()
}

fn run_hook_inner(
    event: HookEvent,
    mut socket: Option<PathBuf>,
    raw: &str,
    var: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let input: Map<String, Value> = if raw.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(raw).ok()? {
            Value::Object(input) => input,
            _ => Map::new(),
        }
    };
    let claude_session_id = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| var("CLAUDE_CODE_SESSION_ID"));
    let deadline = Instant::now()
        + if event == HookEvent::SessionStart {
            SESSION_START_WAIT
        } else {
            CONNECT_WAIT
        };
    if socket.is_none() {
        let dir = control_dir(var).ok()?;
        let query = ServerQuery {
            claude_session_id,
            cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
            name: None,
        };
        loop {
            socket = find_server(&query, &dir).map(|s| PathBuf::from(s.socket));
            if socket.is_some() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let socket = socket?;
    let mut client = ControlClient::connect(&socket, deadline.saturating_duration_since(Instant::now())).ok()?;
    let mut fields = Map::new();
    fields.insert("event".into(), event.command().into());
    fields.insert("input".into(), Value::Object(input));
    let result = client.request(crate::control::op::HOOK, fields, REQUEST_TIMEOUT).ok()?;
    match &result {
        Value::Object(map) if !map.is_empty() => Some(result.to_string()),
        _ => None,
    }
}

/// The events a `wait-events` waiter got from its server, or `None` (nothing to wake for, replaced, no server).
fn wait_events(socket: Option<PathBuf>) -> Option<String> {
    let mut client = ControlClient::connect(&socket?, CONNECT_WAIT).ok()?;
    let result = client
        .request(crate::control::op::WAIT_EVENTS, Map::new(), WAIT_EVENTS_TIMEOUT)
        .ok()?;
    result.get("events").and_then(Value::as_str).map(str::to_string)
}

/// `ultron hook <event> [--socket path]`; returns the exit code.
pub fn run(
    args: &[String],
    stdin: &mut dyn std::io::Read,
    stdin_is_tty: bool,
    var: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    let event = args.first().map(String::as_str).unwrap_or("");
    let mut socket = None;
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--socket" && index + 1 < args.len() {
            socket = Some(PathBuf::from(&args[index + 1]));
            index += 2;
        } else {
            let _ = writeln!(err, "ultron hook: unknown option {}", args[index]);
            return 2;
        }
    }
    let Some(parsed) = HookEvent::parse(event) else {
        let names: Vec<&str> = SESSION_HOOKS.iter().map(|event| event.command()).collect();
        let shown = if event.is_empty() { "(none)" } else { event };
        let _ = writeln!(err, "ultron hook: unknown event {shown}; one of {}", names.join(", "));
        return 2;
    };
    let mut raw = String::new();
    if !stdin_is_tty {
        let mut bytes = Vec::new();
        let _ = stdin.read_to_end(&mut bytes);
        raw = String::from_utf8_lossy(&bytes).into_owned();
    }
    if parsed == HookEvent::WaitEvents {
        return match wait_events(socket) {
            Some(events) => {
                let _ = writeln!(
                    err,
                    "Background work you started ended while you were idle:\n{events}\nCarry on with it (rlm.collect what you need)."
                );
                2
            }
            None => 0,
        };
    }
    let output = run_hook(event, socket, &raw, var);
    if !output.is_empty() {
        let _ = writeln!(out, "{output}");
    }
    0
}
