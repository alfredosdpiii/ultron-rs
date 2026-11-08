//! The RPC session as `rpc-native.ts` drives it: this process's server generation, the session attached through a
//! [`SessionLink`], every command an operation of the session worker (crates/session-core `Op`), and the run's progress
//! the transcript's lane-watch events translated to Pi's RPC events. New, switched, forked and cloned sessions are
//! created by the server and re-attached, as TypeScript clients do.
//!
//! Supervision (Ultron Durable): while this process runs, its server generation restarts a session worker that
//! crashes, attached or not (busy or detached work held). When RPC mode ends (stdin closed, a signal), the server
//! releases the worker: a busy one (a run, running shell jobs or subagents, events waiting) keeps its work running
//! detached and retires when it is done, but nothing supervises it any more, so a crash after that is not restarted.
//! This is the TypeScript behaviour (`rpc-native.ts`: a released worker runs on alone, and a crashed one is only
//! resumed by the next client, `ultron -c`). The session's durable runtime then resumes what was interrupted (pinned
//! requests resent, cells reported interrupted, subagents reattached, and shell jobs that outlived the worker
//! followed; a sandboxed job dies with its worker) when the next client attaches.

use super::events::RpcEventTranslator;
use super::{Input, Output, Reply};
use crate::Env;
use crate::args::Args;
use crate::print_mode::Target;
use client::session::SessionLink;
use protocol::watch::{QueueKind, WatchEvent};
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

/// What reaches the command loop besides stdin.
pub enum Event {
    /// A lane-watch event of the attached session.
    Watch(Value),
    /// A `bash` or `compact` command finished: its response, and the count of commands still running.
    BashDone {
        reply: Reply,
        result: Result<Option<Value>, String>,
        pending: Arc<AtomicUsize>,
    },
    /// A signal ended RPC mode: release the session (nothing is aborted) and exit with this code.
    Signal(i32),
    /// A `/mcp` dialog or an MCP notice of the worker, written as Pi's `extension_ui_request` line.
    ExtensionUi(Value),
}

/// `/mcp` dialogs and MCP notices from the worker reach this client as Pi's `extension_ui_request` lines
/// (rpc-native.ts `uiLoop`); a different session starts again from its open dialogs.
fn serve_extension_ui(link: client::session::LinkHandle, tx: Sender<Input>, stop: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        let mut poll = crate::link::ExtensionUiPoll::default();
        while !stop.load(Ordering::SeqCst) {
            for request in poll.poll(&link.session_id(), &link.connection()) {
                if tx.send(Input::Event(Event::ExtensionUi(request))).is_err() {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

/// The run this front end waits on, as the transcript and the link report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTrack {
    Idle,
    /// A run is going: prompted (its `run_start` not seen yet), attached while streaming, or started.
    Running,
    /// The link lost its worker or server while a run was going; it ends as failed when the link rejoins or is lost,
    /// unless a `run_start` or `run_end` arrives first.
    Lost,
}

impl RunTrack {
    /// The state after `event`, and whether `event` is the link settling a lost run (a failed `run_end` is due).
    pub fn next(self, event: &Value) -> (RunTrack, bool) {
        match (event["type"].as_str(), event["state"].as_str()) {
            (Some("run_start"), _) => (RunTrack::Running, false),
            (Some("run_end"), _) => (RunTrack::Idle, false),
            (Some("connection"), Some("reconnecting")) if self != RunTrack::Idle => (RunTrack::Lost, false),
            (Some("connection"), Some("rejoined" | "lost")) if self == RunTrack::Lost => (RunTrack::Idle, true),
            _ => (self, false),
        }
    }
}

pub struct Remote<'a> {
    env: &'a Env,
    parsed: &'a Args,
    link: Option<SessionLink>,
    tx: Sender<Input>,
    translator: RpcEventTranslator,
    run: RunTrack,
    /// Commands answered when they end (`bash`, `compact`) still running.
    bash: Arc<AtomicUsize>,
    /// Ends the extension UI relay.
    ui_stop: Arc<std::sync::atomic::AtomicBool>,
    /// herdr's agents panel: each run's `working` and end (crate::herdr_report); released when dropped.
    herdr: Option<crate::herdr_report::RpcReport>,
    /// The reply of the last prompt when the worker answered it as one of the session's commands (`/goal`,
    /// `/persona`, `/keepgoing`; daemon::session_commands) instead of starting a run.
    command_reply: Option<Value>,
}

/// The session id of a `switch_session` selector (rpc-native.ts `sessionIdFromSelector`): an id, or a session file
/// whose native header names one.
fn selector_id(env: &Env, selector: &str) -> Result<String, String> {
    if !crate::session::names_file(selector) {
        return Ok(selector.to_string());
    }
    let path = config::paths::normalize(&env.cwd.join(selector));
    let header = crate::session::read_header(&path)?
        .native()
        .ok_or_else(|| format!("Session file is not a valid ultron session: {selector}"))?;
    header
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Session file has no ID: {selector}"))
}

impl<'a> Remote<'a> {
    pub fn open(parsed: &'a Args, env: &'a Env, target: Target, tx: Sender<Input>) -> Result<Self, String> {
        if matches!(target, Target::New { .. } | Target::Ephemeral) {
            crate::print_mode::resolve_model(parsed, env)?;
        }
        let watch = tx.clone();
        let link = crate::link::open(
            parsed,
            env,
            &target,
            "rpc",
            Arc::new(move |event| {
                let _ = watch.send(Input::Event(Event::Watch(event)));
            }),
        )?;
        let ui_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        serve_extension_ui(link.handle(), tx.clone(), ui_stop.clone());
        Ok(Remote {
            ui_stop,
            env,
            parsed,
            link: Some(link),
            tx,
            translator: RpcEventTranslator::default(),
            run: RunTrack::Idle,
            bash: Arc::new(AtomicUsize::new(0)),
            herdr: crate::herdr_report::RpcReport::start(env),
            command_reply: None,
        })
    }

    /// Create a session (named `name`) and switch to it, as `new_session` does.
    pub fn new_named_session(&mut self, name: Option<&str>) -> Result<(), String> {
        let options = name.map_or_else(|| json!({}), |name| json!({"name": name}));
        let created = self.link.as_mut().ok_or("The session is closed")?.create(options)?;
        self.switch_to(&created)
    }

    /// The reply of the last prompt the worker answered as a session command (`{command, text, data}`), once.
    pub fn take_command_reply(&mut self) -> Option<Value> {
        self.command_reply.take()
    }

    fn link(&self) -> Result<&SessionLink, String> {
        self.link.as_ref().ok_or_else(|| "The session is closed".to_string())
    }

    fn op(&self, op: Op) -> Result<Option<Value>, String> {
        self.link()?.op(&op)
    }

    /// An operation whose data is a value the command answers with as it is (`Null` on failure).
    fn read(&self, op: Op) -> Value {
        self.op(op).ok().flatten().unwrap_or(Value::Null)
    }

    /// Re-attach to session `id` (rpc-native.ts `switchTo`): the session left keeps any run it has going, detached;
    /// the one attached may itself be running (`snapshot().operation?.kind === "run"`).
    fn switch_to(&mut self, id: &str) -> Result<(), String> {
        let link = self.link.as_mut().ok_or("The session is closed")?;
        link.attach(id)?;
        self.run = if self.read(Op::State)["isStreaming"] == true {
            RunTrack::Running
        } else {
            RunTrack::Idle
        };
        Ok(())
    }

    /// Fork the current session (`forkPath`), record the fork in the new session, and attach to it.
    fn fork_to(&mut self, fork_path: Value) -> Result<(), String> {
        let source = self.link()?.session_id();
        let created = self
            .link
            .as_mut()
            .ok_or("The session is closed")?
            .create(json!({"forkFromSessionId": source, "forkPath": fork_path}))?;
        if let Some(file) = self.link()?.session_file(&created)? {
            self.op(Op::Forked { target: file })?;
        }
        self.switch_to(&created)
    }
}

/// Errors are the `error` text of the failed response.
impl Remote<'_> {
    pub fn deliver(&mut self, event: Event, out: &Output) {
        let event = match event {
            Event::Watch(event) => event,
            Event::BashDone { reply, result, pending } => {
                reply.send(result);
                pending.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            Event::Signal(code) => {
                if let Some(link) = self.link.take() {
                    link.close(false);
                }
                std::process::exit(code);
            }
            Event::ExtensionUi(request) => {
                out.emit(&request);
                return;
            }
        };
        // A run its worker lost (also one prompted whose `run_start` never came) ends for RPC clients as failed when
        // the link rejoins (recovery settled it) or is lost, unless its end, or another run, was seen meanwhile.
        let (run, lost) = self.run.next(&event);
        self.run = run;
        let event = if lost {
            json!({"type": "run_end", "status": "failed", "endedAt": jsrt::date::now_ms(),
                "error": {"code": "interrupted", "message": "The session worker stopped during this run"}})
        } else {
            event
        };
        if let Some(herdr) = &self.herdr {
            herdr.event(&event);
        }
        if let Some(lane_event) = WatchEvent::decode(&event) {
            for line in self.translator.translate(lane_event) {
                out.emit(&line);
            }
        }
    }

    /// Whether work is still in flight that must finish before the process exits.
    pub fn busy(&self) -> bool {
        self.is_running() || self.bash.load(Ordering::SeqCst) > 0
    }

    /// Stdin ended: stop runs, then [`busy`](Self::busy) drains the rest.
    pub fn input_ended(&mut self) {
        // Leaving is not an abort (TS: a root turn in flight when its client closes stdin runs to completion).
        if signals::received().is_some() {
            return;
        }
        self.close();
        self.run = RunTrack::Idle;
        self.bash.store(0, Ordering::SeqCst);
    }

    /// Release the session (delete `--no-session` files, stop the kernel).
    pub fn close(&mut self) {
        self.ui_stop.store(true, Ordering::SeqCst);
        if let Some(link) = self.link.take() {
            link.close(self.parsed.no_session);
        }
    }

    pub fn is_running(&self) -> bool {
        self.run != RunTrack::Idle
    }

    pub fn prompt(&mut self, message: String, images: Vec<Value>) -> Result<(), String> {
        let reply = self.op(Op::Prompt {
            message,
            images,
            queue: None,
        })?;
        // The worker ran one of the session's commands: no run starts.
        if let Some(reply) = reply.filter(|r| r["command"].is_string()) {
            self.command_reply = Some(reply);
            return Ok(());
        }
        // The run starts now; its `run_start` follows on the transcript.
        if self.run == RunTrack::Idle {
            self.run = RunTrack::Running;
        }
        Ok(())
    }

    pub fn enqueue(&mut self, kind: QueueKind, message: String, images: Vec<Value>) -> Result<(), String> {
        self.op(Op::Enqueue {
            kind,
            message,
            images,
            entry_id: None,
        })
        .map(drop)
    }

    pub fn abort(&mut self) {
        let _ = self.op(Op::Abort);
    }

    /// Empty the queues; returns the texts that were queued.
    pub fn clear_queue(&mut self) -> Value {
        self.read(Op::ClearQueue)
    }

    /// rpc-native.ts:256-261: create a session and switch to it, also during a run (that turn keeps running, detached).
    pub fn new_session(&mut self) -> Result<(), String> {
        let created = self.link.as_mut().ok_or("The session is closed")?.create(json!({}))?;
        self.switch_to(&created)
    }

    pub fn state(&self) -> Value {
        self.read(Op::State)
    }

    pub fn available_models(&self) -> Vec<Value> {
        self.read(Op::AvailableModels).as_array().cloned().unwrap_or_default()
    }

    /// Select a model from the available ones; returns it.
    pub fn set_model(&mut self, provider: &str, model_id: &str) -> Result<Value, String> {
        self.op(Op::SetModel {
            provider: provider.into(),
            model_id: model_id.into(),
        })
        .map(Option::unwrap_or_default)
    }

    /// The next available model, or `None` with fewer than two.
    pub fn cycle_model(&mut self) -> Result<Option<Value>, String> {
        self.op(Op::CycleModel).map(|v| v.filter(|v| !v.is_null()))
    }

    pub fn thinking_levels(&self) -> Vec<String> {
        self.read(Op::ThinkingLevels)
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect()
    }

    pub fn set_thinking_level(&mut self, level: &str) -> Result<(), String> {
        self.op(Op::SetThinking { level: level.into() }).map(drop)
    }

    pub fn cycle_thinking_level(&mut self) -> Result<Option<String>, String> {
        self.op(Op::CycleThinking)
            .map(|v| v.and_then(|v| v.as_str().map(str::to_string)))
    }

    pub fn set_queue_mode(&mut self, kind: QueueKind, mode: &str) -> Result<(), String> {
        self.op(Op::SetQueueMode {
            kind,
            mode: mode.into(),
        })
        .map(drop)
    }

    /// Compact; `reply` answers once the compaction ends, so `abort` and `get_state` are served meanwhile.
    pub fn compact(&mut self, custom_instructions: Option<&str>, reply: Reply) -> Result<(), String> {
        let connection = self.link()?.connection();
        let session = connection.session_target().map_err(|e| e.to_string())?;
        let op = Op::Compact {
            instructions: custom_instructions.map(str::to_string),
        };
        let request = connection
            .start(session, protocol::names::SESSION_OPS, "op", json!([op.to_json()]))
            .map_err(|e| e.to_string())?;
        let (pending, tx) = (self.bash.clone(), self.tx.clone());
        pending.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let result = connection
                .wait(&request)
                .map(|data| Some(data.unwrap_or_default()))
                .map_err(client::lost::message);
            // The command loop answers it: that thread holds the process's stdout.
            let _ = tx.send(Input::Event(Event::BashDone { reply, result, pending }));
        });
        Ok(())
    }

    pub fn set_auto_compaction(&mut self, enabled: bool) {
        let _ = self.op(Op::SetAutoCompaction(enabled));
    }

    pub fn set_auto_retry(&mut self, enabled: bool) {
        let _ = self.op(Op::SetAutoRetry(enabled));
    }

    pub fn abort_retry(&mut self) {
        let _ = self.op(Op::AbortRetry);
    }

    pub fn bash(&mut self, command: String, exclude_from_context: bool, reply: Reply) -> Result<(), String> {
        let connection = self.link()?.connection();
        let session = connection.session_target().map_err(|e| e.to_string())?;
        // The command's id tags its streamed output (`bash_execution_update`), as rpc-mode.ts passes it.
        let id = reply.id.clone().filter(Value::is_string).unwrap_or(Value::Null);
        let request = connection
            .start(
                session,
                protocol::names::SESSION_CONTROL,
                "bash",
                json!([command, exclude_from_context, id]),
            )
            .map_err(|e| e.to_string())?;
        let (pending, tx) = (self.bash.clone(), self.tx.clone());
        pending.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let result = connection.wait(&request).map_err(|e| e.to_string());
            let _ = tx.send(Input::Event(Event::BashDone { reply, result, pending }));
        });
        Ok(())
    }

    pub fn abort_bash(&mut self) {
        let _ = self.op(Op::AbortBash);
    }

    pub fn session_stats(&self) -> Value {
        self.read(Op::Stats)
    }

    /// rpc-native.ts `switch_session`: also during a run, which keeps running detached.
    pub fn switch_session(&mut self, selector: &str) -> Result<(), String> {
        let id = selector_id(self.env, selector)?;
        if id == self.link()?.session_id() {
            return Ok(());
        }
        if self.link()?.session_file(&id)?.is_none() {
            return Err(format!("Session not found: {id}"));
        }
        self.switch_to(&id)
    }

    /// Fork before a user message; returns its text.
    pub fn fork(&mut self, entry_id: &str) -> Result<String, String> {
        let text = self
            .op(Op::ForkSource {
                entry_id: entry_id.into(),
            })?
            .and_then(|t| t.as_str().map(str::to_string))
            .unwrap_or_default();
        // As in Pi, the new session holds only the path from the root to just before that message.
        self.fork_to(json!({"entryId": entry_id, "position": "before"}))?;
        Ok(text)
    }

    pub fn clone_session(&mut self) -> Result<(), String> {
        self.op(Op::CloneSource)?;
        self.fork_to(json!({"position": "at"}))
    }

    pub fn fork_messages(&self) -> Vec<Value> {
        self.read(Op::ForkMessages).as_array().cloned().unwrap_or_default()
    }

    /// Entries after `since` (all without it), and the leaf id.
    pub fn entries(&self, since: Option<&str>) -> Result<(Vec<Value>, Value), String> {
        let data = self
            .op(Op::Entries {
                since: since.map(str::to_string),
            })?
            .unwrap_or_default();
        Ok((
            data["entries"].as_array().cloned().unwrap_or_default(),
            data["leafId"].clone(),
        ))
    }

    pub fn tree(&self) -> (Value, Value) {
        let data = self.read(Op::Tree);
        (data["tree"].clone(), data["leafId"].clone())
    }

    pub fn last_assistant_text(&self) -> Option<String> {
        self.read(Op::LastAssistantText).as_str().map(str::to_string)
    }

    pub fn set_session_name(&mut self, name: &str) -> Result<(), String> {
        self.op(Op::SetName { name: name.into() }).map(drop)
    }

    pub fn messages(&self) -> Vec<Value> {
        self.read(Op::Messages).as_array().cloned().unwrap_or_default()
    }

    pub fn commands(&self) -> Result<Vec<Value>, String> {
        Ok(self
            .op(Op::Commands)?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default())
    }

    /// A read-only inspection request (`agents.status`, `rlm.frames`, ...) through the session's inspector.
    pub fn inspect(&mut self, request: &str, payload: Value) -> Result<Value, String> {
        Ok(self
            .op(Op::Inspect {
                request: request.into(),
                payload,
            })?
            .unwrap_or(Value::Null))
    }

    /// Pi's `extension_ui_response`: the answer to a `/mcp` dialog of the worker.
    pub fn extension_ui_response(&mut self, id: &str, answer: Value) {
        if let Some(link) = &self.link {
            let _ = link.call(protocol::names::EXTENSION_UI, "respond", json!([id, answer]));
        }
    }

    /// Write the session's active branch as an HTML page; returns the path written.
    pub fn export_html(&mut self, output_path: Option<&str>) -> Result<String, String> {
        // The session's active branch as the worker holds it (`snapshot().transcript`), under its header's creation
        // time; the page's working directory is this process's, as in rpc-native.ts.
        let described = self.op(Op::Describe)?.unwrap_or_default();
        let id = described["sessionId"].as_str().unwrap_or_default().to_string();
        let created_at = described["sessionFile"]
            .as_str()
            .and_then(|file| crate::session::read_header(std::path::Path::new(file)).ok()?.native())
            .and_then(|header| header.get("createdAt")?.as_f64())
            .unwrap_or_else(|| jsrt::date::now_ms() as f64);
        let entries = described["entries"].as_array().cloned().unwrap_or_default();
        let cwd = self.env.cwd.display().to_string();
        let data = crate::export_html::native_entries_data(&id, &cwd, created_at, &entries);
        crate::export_html::write_native_session_html(&data, &id, output_path, self.env)
    }

    /// `/persona on|off|auto`, `/keepgoing on|off` (`command`: `persona` or `keepgoing`; empty `args` reports): the
    /// session's persona toggle and keep-going mode (resources::keep_going); `{text, persona, effective, keepGoing}`.
    pub fn persona(&mut self, command: &str, args: &str) -> Result<Value, String> {
        let args = args.to_string();
        let op = if command == "keepgoing" {
            Op::KeepGoing { args }
        } else {
            Op::Persona { args }
        };
        Ok(self.op(op)?.unwrap_or_default())
    }

    /// `/goal <args>`: the session goal's command (set, pause, resume, clear, check); returns its reply text.
    pub fn goal(&mut self, args: &str) -> Result<String, String> {
        let reply = self.op(Op::Goal { args: args.to_string() })?;
        Ok(reply
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("No goal reply")
            .to_string())
    }
}

/// Run RPC mode over `input`/`output` until the input ends; returns the exit code.
pub fn run(
    parsed: &Args,
    env: &Env,
    target: Target,
    input: Box<dyn std::io::Read + Send>,
    output: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // The link traces rpc-native.ts's `rpc.open` .. `rpc.attached` milestones as it starts the server, selects the
    // session and attaches.
    let mut session = Remote::open(parsed, env, target, tx.clone())?;
    // A signal ends RPC mode as the end of its input does (the session detaches, the server closes), then exits
    // with the signal's code.
    let signalled = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let (code, end) = (signalled.clone(), std::sync::Mutex::new(tx.clone()));
    crate::print_mode::exit_on_signal(move |exit| {
        code.store(exit, Ordering::SeqCst);
        let _ = end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(Input::Event(Event::Signal(exit)));
    });
    let reader = std::thread::spawn(move || {
        for line in super::jsonl::JsonlLines::new(input) {
            match line {
                Ok(line) => {
                    if tx.send(Input::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Input::Eof);
    });
    let out = Output::new(output);
    super::serve(&mut session, &rx, &out);
    drop(rx);
    let _ = reader;
    let code = signalled.load(Ordering::SeqCst);
    Ok(if code != 0 {
        code
    } else {
        signals::received().map_or(0, signals::exit_code)
    })
}
