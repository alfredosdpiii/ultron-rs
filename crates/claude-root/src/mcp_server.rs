//! `ultron mcp` (mcp-server.ts): Ultron's REPL as an MCP server for Claude Code. One server process is one Claude
//! Code session (or one Claude Code subagent, `--child`), and it runs Ultron's own session runtime on an Ultron
//! session of its own: the same host stack, kernel, file hooks (Loki), secret masking, subagents and frames as
//! Ultron's root lane. Claude Code is the root agent; the server exposes:
//!
//! - tool `rlm` (and nothing else): run a Python cell in the persistent kernel ([`ExternalRoot::run_cell`]);
//! - prompt `ultron-guide` and resource `ultron://guide`: the runtime guide (also the server instructions, unless
//!   `--no-instructions`, which `ultron claude` passes because the guide is its system prompt);
//! - a control socket for hooks (`ultron hook`), the viewer (`ultron watch`) and subagent servers.
//!
//! The Ultron session of a Claude Code session is remembered by Claude Code's session id, so `claude --resume`
//! reopens the same session.

mod args;
mod runtime;

pub use args::{
    CLAUDE_CHILDREN_ENV, McpServerArgs, Resolved, claude_code_setting, default_frame_model, parse_mcp_args, resolve,
    split_model, subscription_model, ultron_child_model,
};
pub use runtime::Opened;

use crate::Ctx;
use crate::claude_cli::resolve_claude_binary;
use crate::control::{
    ControlClient, ControlHandler, ServerRecord, control_dir, op, socket_path_for, start_control_server,
};
use crate::guide::{Audience, GuideOptions, claude_runtime_guide_for, claude_system_prompt};
use crate::hooks::HookEvent;
use crate::mcp_protocol::{McpHandlers, McpPrompt, McpResource, serve};
use crate::settings::ChildrenMode;
use ai::AbortSignal;
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use session_journal::run::MAIN_LANE;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often a `wait-events` waiter looks at the root ([`Server::wait_events`]).
const WAIT_EVENTS_POLL: Duration = Duration::from_millis(250);
/// How long nothing may run and nothing wait before a `wait-events` waiter gives up.
const WAIT_EVENTS_GRACE: Duration = Duration::from_secs(2);

/// Child tokens of running Claude Code subagents, by token: their lane.
type ChildTokens = Arc<Mutex<std::collections::HashMap<String, String>>>;

/// A root cell as the viewer shows it.
#[derive(Clone, Debug)]
struct CellRecord {
    n: u64,
    started_at: i64,
    ended_at: Option<i64>,
    code: String,
    status: &'static str,
    output: Option<String>,
}

impl CellRecord {
    fn to_json(&self) -> Value {
        let mut cell = Map::new();
        cell.insert("n".into(), self.n.into());
        cell.insert("startedAt".into(), self.started_at.into());
        cell.insert("code".into(), self.code.clone().into());
        cell.insert("status".into(), self.status.into());
        if let Some(ended) = self.ended_at {
            cell.insert("endedAt".into(), ended.into());
        }
        if let Some(output) = &self.output {
            cell.insert("output".into(), output.clone().into());
        }
        Value::Object(cell)
    }
}

/// The first `n` UTF-16 units of `text` (JS `slice(0, n)`), never splitting a character.
fn js_head(text: &str, n: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect()
}

/// The last `n` UTF-16 units of `text` (JS `slice(-n)`).
fn js_tail(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// The runtime once it opened, or why it failed.
#[derive(Default)]
struct Startup {
    done: bool,
    opened: Option<Arc<Opened>>,
    error: Option<String>,
}

pub struct Server {
    args: McpServerArgs,
    env: RunEnv,
    name: String,
    record: ServerRecord,
    resolved: Resolved,
    claude_session_id: Option<String>,
    startup: Mutex<Startup>,
    ready: Condvar,
    cells: Mutex<(u64, Vec<CellRecord>)>,
    hints: bool,
    /// Tokens of running Claude Code subagents: a verdict is accepted only from the child holding its token.
    child_tokens: ChildTokens,
    /// The persona's nudges since the user's last prompt (crate::persona_hook).
    persona_nudges: Mutex<u32>,
    /// The latest `wait-events` waiter's number ([`Server::wait_events`]).
    waiters: std::sync::atomic::AtomicU64,
    /// The server is shutting down: waiters return.
    closed: std::sync::atomic::AtomicBool,
    /// The session's persona toggle differs from the launch prompt's and the model was told once (this server).
    persona_told: Mutex<Option<bool>>,
    /// Rigor playbooks already injected into this external-root session (reset on SessionStart).
    shown_playbooks: Mutex<Vec<String>>,
    /// herdr's agents panel (crate::herdr_hook).
    herdr: crate::herdr_hook::HerdrHook,
    /// The kernel APIs the guide names (crate::guide::runtime_apis).
    apis: resources::runtime_guide::RuntimeGuideOptions,
    /// Where this server's subagents reach the session's swarm, board and notes (crate::shared_world).
    world: Arc<crate::shared_world::SharedWorld>,
}

fn object_field(request: &Map<String, Value>, key: &str) -> Map<String, Value> {
    match request.get(key) {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

fn string_field(request: &Map<String, Value>, key: &str) -> String {
    match request.get(key) {
        None | Some(Value::Null) => String::new(),
        other => jsrt::value::string(other),
    }
}

fn hook_context(name: &str, context: Option<String>) -> Value {
    match context {
        Some(text) => json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text}}),
        None => json!({}),
    }
}

impl Server {
    /// Wait for the runtime; the startup error when it failed.
    fn runtime(&self) -> Result<Arc<Opened>, String> {
        let mut startup = jsrt::sync::lock(&self.startup);
        while !startup.done {
            startup = self.ready.wait(startup).unwrap_or_else(PoisonError::into_inner);
        }
        match (&startup.opened, &startup.error) {
            (Some(opened), _) => Ok(opened.clone()),
            (None, error) => Err(error.clone().unwrap_or_else(|| "the runtime did not start".into())),
        }
    }

    fn opened_now(&self) -> (Option<Arc<Opened>>, Option<String>) {
        let startup = jsrt::sync::lock(&self.startup);
        (startup.opened.clone(), startup.error.clone())
    }

    fn status(&self) -> Value {
        let (opened, error) = self.opened_now();
        let mut status = Map::new();
        status.insert("name".into(), self.name.clone().into());
        status.insert("pid".into(), std::process::id().into());
        status.insert("cwd".into(), self.record.cwd.clone().into());
        status.insert("child".into(), self.args.child.into());
        status.insert("startedAt".into(), self.record.started_at.into());
        status.insert("ready".into(), opened.is_some().into());
        if let Some(error) = error {
            status.insert("error".into(), error.into());
        }
        if let Some(id) = &self.claude_session_id {
            status.insert("claudeSessionId".into(), id.clone().into());
        }
        if let Some(opened) = opened {
            status.insert("sessionId".into(), opened.session_id.clone().into());
            status.insert("frameModel".into(), opened.model.clone().into());
            status.insert("children".into(), self.resolved.children.as_str().into());
            status.insert("turn".into(), opened.root.turn().map_or(Value::Null, Value::from));
            status.insert("cells".into(), opened.root.cells().into());
            status.insert("pendingEvents".into(), opened.root.pending_events().into());
        }
        Value::Object(status)
    }

    fn control_inspect(&self, request: &Map<String, Value>) -> Result<Value, String> {
        let inspect = string_field(request, "request");
        if !session_core::inspect::VIEWER_INSPECTIONS.contains(&inspect.as_str()) {
            return Err(format!("not a read-only inspection: {inspect}"));
        }
        let payload = object_field(request, "payload");
        let opened = self.runtime()?;
        let stack = &opened.lane.stack;
        match inspect.as_str() {
            "rlm.pool" => Ok(stack.pool_stats()),
            "async.pending" => Ok(crate::external_lane::async_pending(stack, &opened.sink)),
            "usage.report" => {
                stack.stats.flush();
                usage::session_report(
                    &opened.session_path,
                    &session_core::lane::subscription_providers(&self.env),
                )
            }
            _ => stack
                .cell_handler(MAIN_LANE, None)
                .handle(&inspect, payload, &kernel::Signal::new()),
        }
    }

    fn control_child_lane(&self, request: &Map<String, Value>, error: &str) -> Result<String, String> {
        request
            .get("token")
            .and_then(Value::as_str)
            .and_then(|token| jsrt::sync::lock(&self.child_tokens).get(token).cloned())
            .ok_or_else(|| error.to_string())
    }

    fn control_child_finish(&self, request: &Map<String, Value>) -> Result<Value, String> {
        let lane = self.control_child_lane(request, "rlm.finish: unknown subagent")?;
        let payload = object_field(request, "payload");
        let opened = self.runtime()?;
        opened
            .lane
            .stack
            .cell_handler(&lane, None)
            .handle("rlm.finish", payload, &kernel::Signal::new())
    }

    fn control_shared(&self, request: &Map<String, Value>) -> Result<Value, String> {
        let lane = self.control_child_lane(request, "the session's shared world: unknown subagent")?;
        let kind = request.get("kind").and_then(Value::as_str).unwrap_or_default();
        let payload = object_field(request, "payload");
        self.runtime()?;
        let name = match request.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => self.world.name_of_lane(&lane),
        };
        self.world.serve(Some(&name), kind, &payload)
    }

    fn control_persona(&self, requested: &str, request: &Map<String, Value>) -> Result<Value, String> {
        let args = request.get("args").and_then(Value::as_str).unwrap_or("").to_string();
        let opened = self.runtime()?;
        let global = self.persona_on();
        let text = self.persona_change(&opened, |state, now, limits| {
            state.command(requested, &args, now, global, limits)
        })?;
        Ok(json!({"text": text}))
    }

    fn control_cells(&self) -> Value {
        let cells = jsrt::sync::lock(&self.cells);
        let start = cells.1.len().saturating_sub(50);
        json!({"cells": cells.1[start..].iter().map(CellRecord::to_json).collect::<Vec<_>>()})
    }

    fn control_hook(&self, request: &Map<String, Value>) -> Result<Value, String> {
        let event = string_field(request, "event");
        let event = HookEvent::parse(&event).ok_or_else(|| format!("unknown hook event: {event}"))?;
        self.hook(event, &object_field(request, "input"))
    }

    pub fn control(&self, request: Map<String, Value>) -> Result<Value, String> {
        let requested = request.get("op").and_then(Value::as_str).unwrap_or("");
        match requested {
            op::STATUS => Ok(self.status()),
            op::CELLS => Ok(self.control_cells()),
            op::INSPECT => self.control_inspect(&request),
            op::HOOK => self.control_hook(&request),
            op::CHILD_FINISH => self.control_child_finish(&request),
            op::SHARED => self.control_shared(&request),
            op::WAIT_EVENTS => Ok(self.wait_events(&*self.runtime()?)),
            "persona" | "keepgoing" => self.control_persona(requested, &request),
            other => Err(format!("unknown control op: {other}")),
        }
    }

    /// Wait until the root's turn has ended with events waiting (`{"events": text}`), or until nothing it started
    /// is running and no event waits, or a newer waiter takes over (`{}`). One waiter at a time: every rlm call starts
    /// one (PostToolUse), and only the latest may wake the model.
    fn wait_events(&self, opened: &Opened) -> Value {
        let root = &opened.root;
        let me = self.waiters.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut idle_since: Option<Instant> = None;
        loop {
            if self.waiters.load(std::sync::atomic::Ordering::SeqCst) != me
                || self.closed.load(std::sync::atomic::Ordering::SeqCst)
            {
                return json!({});
            }
            let events = root.pending_events() > 0;
            if events
                && root.turn().is_none()
                && let Some(text) = root.take_events()
            {
                return json!({"events": text});
            }
            let pending = crate::external_lane::async_pending(&opened.lane.stack, &opened.sink)["pending"] == true
                || opened.schedules_pending();
            if !events && !pending {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= WAIT_EVENTS_GRACE {
                    return json!({});
                }
            } else {
                idle_since = None;
            }
            std::thread::sleep(WAIT_EVENTS_POLL);
        }
    }

    fn hook_user_prompt(&self, opened: &Opened, input: &Map<String, Value>) -> Value {
        let prompt = input.get("prompt").and_then(Value::as_str).unwrap_or("");
        if !self.args.child {
            let global = self.persona_on();
            if let Some(answer) = self.persona_change(opened, |state, now, limits| {
                crate::persona_hook::prompt_toggle(state, prompt, now, global, limits)
            }) {
                return answer;
            }
        }
        *jsrt::sync::lock(&self.persona_nudges) = 0;
        let mut contexts: Vec<String> = opened.root.begin_turn().into_iter().collect();
        let mut notice = None;
        if !self.args.child {
            let (message, start) = self.persona_change(opened, |state, now, _| {
                crate::persona_hook::prompt_input(state, prompt, now)
            });
            notice = message;
            contexts.extend(self.persona_context(opened));
            contexts.extend(start);
            contexts.extend(self.routed_playbook(prompt));
        }
        let mut answer = hook_context(
            HookEvent::UserPrompt.claude_event(),
            (!contexts.is_empty()).then(|| contexts.join("\n\n")),
        );
        if let Some(notice) = notice {
            answer["systemMessage"] = Value::String(notice);
        }
        answer
    }

    fn hook_stop(&self, opened: &Opened, input: &Map<String, Value>) -> Value {
        opened.root.end_turn();
        if self.args.child {
            return json!({});
        }
        let mut nudges = jsrt::sync::lock(&self.persona_nudges);
        let global = self.persona_on();
        let transcript = crate::persona_hook::transcript_of(input);
        let answer = self.persona_change(opened, |state, now, limits| {
            crate::persona_hook::stop_answer_in(state, &mut nudges, global, &transcript, now, limits)
        });
        self.herdr.stopped(input, &answer);
        answer
    }

    /// Claude Code hook events: the hook command forwards its input here and prints what this returns.
    fn hook(&self, event: HookEvent, input: &Map<String, Value>) -> Result<Value, String> {
        if let Some(answer) = self.herdr.before(event, input) {
            return Ok(answer);
        }
        let opened = self.runtime()?;
        match event {
            HookEvent::SessionStart => {
                jsrt::sync::lock(&self.shown_playbooks).clear();
                let mut parts: Vec<String> = opened.root.session_context().into_iter().collect();
                parts.extend(self.session_skills_context());
                Ok(hook_context(
                    event.claude_event(),
                    (!parts.is_empty()).then(|| parts.join("\n\n")),
                ))
            }
            HookEvent::UserPrompt => Ok(self.hook_user_prompt(&opened, input)),
            HookEvent::Stop => Ok(self.hook_stop(&opened, input)),
            HookEvent::Notification | HookEvent::SessionEnd | HookEvent::WaitEvents => {
                Err(format!("unknown hook event: {}", event.command()))
            }
        }
    }

    /// In engineering mode, route `prompt` to its fitting rigor playbook once per session (native parity).
    fn routed_playbook(&self, prompt: &str) -> Option<String> {
        session_core::playbook_route::route(prompt)?;
        let settings =
            config::settings::SettingsManager::for_worker(&self.env.cwd, &self.env.agent_dir(), &self.env.home);
        if !config::keys::engineering(settings.settings()) {
            return None;
        }
        let bundled = session_core::session_resources::bundled_skills(&self.env.skills(), &*self.env.var)?;
        let rigor = PathBuf::from(bundled).join("pstack/rigor");
        let mut shown = jsrt::sync::lock(&self.shown_playbooks);
        let message =
            session_core::playbook_route::playbook_message(prompt, &rigor, &shown, jsrt::date::now_ms() as u64)?;
        if let Some(name) = message["details"]["playbook"].as_str() {
            shown.push(name.to_string());
        }
        message["content"].as_str().map(str::to_string)
    }

    /// For external roots that do not receive `--system-prompt` (e.g. `ultron agy`, where `args.instructions` is
    /// true), deliver engineering-mode `alwaysSkills` and the bundled-skills index via `SessionStart`.
    fn session_skills_context(&self) -> Option<String> {
        if !self.args.instructions || self.args.child || self.args.no_skills {
            return None;
        }
        let bundled =
            session_core::session_resources::bundled_skills(&self.env.skills(), &*self.env.var).map(PathBuf::from);
        let resources = crate::guide::load_prompt_resources(
            &self.env,
            bundled.as_deref(),
            crate::guide::ResourceFlags {
                no_context_files: true,
                no_skills: false,
            },
        );
        if !resources.engineering {
            return None;
        }
        let mut parts = Vec::new();
        let always = resources::skills::format_always_skills_for_prompt(&resources.skills);
        if !always.is_empty() {
            parts.push(always);
        }
        let bundled_str = bundled.as_deref().map(|p| p.display().to_string());
        let note = resources::skills::bundled_skills_note(
            &resources.skills,
            bundled_str.as_deref(),
            resources.proactive_skills,
        );
        if !note.is_empty() {
            parts.push(note);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Change the session's persona state (the session value `ultron.module/persona`) with `change`, written back
    /// when it changed.
    fn persona_change<T>(
        &self,
        opened: &Opened,
        change: impl FnOnce(&mut resources::keep_going::SessionPersona, i64, resources::keep_going::Limits) -> T,
    ) -> T {
        use resources::keep_going::{STATE_KEY, SessionPersona};
        use session_journal::ns::MODULE;
        let mut session = session_core::durable_store::lock(&opened.session);
        let mut state = SessionPersona::from_json(session.value(MODULE, STATE_KEY));
        let before = state.clone();
        let limits = resources::keep_going::Limits::from_env(&*self.env.var);
        let out = change(&mut state, jsrt::date::now_ms(), limits);
        if state != before {
            let _ = session.commit(&[session_journal::Write::set(MODULE, STATE_KEY, state.to_json())]);
        }
        out
    }

    /// When the session's toggle differs from the persona the launch prompt has (the settings'), the model is told
    /// once per change: the persona's section when it is on, that the section is off when it is off.
    fn persona_context(&self, opened: &Opened) -> Option<String> {
        use resources::keep_going::{STATE_KEY, SessionPersona};
        use session_journal::ns::MODULE;
        let state =
            SessionPersona::from_json(session_core::durable_store::lock(&opened.session).value(MODULE, STATE_KEY));
        let launched = self.persona_on();
        let wanted = state.effective(launched);
        let mut told = jsrt::sync::lock(&self.persona_told);
        if wanted == launched {
            *told = None;
            return None;
        }
        if *told == Some(wanted) {
            return None;
        }
        *told = Some(wanted);
        if wanted {
            let personality = resources::persona::read_personality(&self.env.agent_dir())
                .ok()
                .flatten()?;
            Some(format!(
                "<persona>\n{}\n</persona>",
                resources::persona::section_text(&personality)
            ))
        } else {
            Some("The persona is off for this session: ignore the <persona> section of your system prompt.".into())
        }
    }

    /// The root's settings name the persona (read per Stop; a subagent never has it).
    fn persona_on(&self) -> bool {
        if self.args.child {
            return false;
        }
        let agent_dir = self.env.agent_dir();
        let trusted = config::trust::worker_project_trusted(&self.env.cwd, &agent_dir, &self.env.home);
        let settings = config::settings::SettingsManager::create(&self.env.cwd, &agent_dir, trusted);
        resources::persona::enabled(settings.settings())
    }

    fn guide_text(&self) -> String {
        if self.args.child {
            claude_system_prompt(
                Audience::ClaudeChild,
                &GuideOptions {
                    allowance: Some(self.args.allowance),
                    apis: Some(&self.apis),
                    ..GuideOptions::default()
                },
                self.hints,
            )
        } else {
            claude_runtime_guide_for(Some(&self.apis), self.hints, self.args.wake)
        }
    }

    fn tool_description(&self) -> String {
        let config = session_core::compose::Config::from_env(&self.env);
        let listing = kernel_apis::CodeSkills::new(kernel_apis::CodeSkillsOptions::new(
            &config.skills_dir,
            &config.runtime_path,
        ))
        .describe();
        let section = if listing.is_empty() {
            String::new()
        } else {
            format!("\n{listing}")
        };
        format!("{}{section}", crate::guide::rlm_tool_description())
    }

    fn run_cell(&self, code: &str, signal: &AbortSignal) -> Value {
        let opened = match self.runtime() {
            Ok(opened) => opened,
            Err(error) => {
                return json!({"content": [{"type": "text", "text": format!("Ultron's runtime failed to start: {error}")}], "isError": true});
            }
        };
        let n = {
            let mut cells = jsrt::sync::lock(&self.cells);
            cells.0 += 1;
            let n = cells.0;
            cells.1.push(CellRecord {
                n,
                started_at: jsrt::date::now_ms(),
                ended_at: None,
                code: js_head(code, 400),
                status: "running",
                output: None,
            });
            let excess = cells.1.len().saturating_sub(200);
            cells.1.drain(..excess);
            n
        };
        let result = opened.root.run_cell(code, signal);
        if let Some(cell) = jsrt::sync::lock(&self.cells).1.iter_mut().find(|c| c.n == n) {
            cell.ended_at = Some(jsrt::date::now_ms());
            cell.status = if result.is_error { "error" } else { "ok" };
            cell.output = result
                .content
                .first()
                .and_then(|p| p["text"].as_str())
                .map(|t| js_tail(t, 400));
        }
        let mut reply = Map::new();
        reply.insert("content".into(), Value::Array(result.content));
        if result.is_error {
            reply.insert("isError".into(), true.into());
        }
        Value::Object(reply)
    }
}

/// The MCP face of a [`Server`].
pub struct Handlers(Arc<Server>);

impl McpHandlers for Handlers {
    fn name(&self) -> String {
        "ultron".into()
    }
    fn version(&self) -> String {
        config::version().into()
    }
    fn instructions(&self) -> Option<String> {
        self.0
            .args
            .instructions
            .then(|| claude_runtime_guide_for(Some(&self.0.apis), self.0.hints, self.0.args.wake))
    }
    fn tools(&self) -> Vec<Value> {
        let mut input_schema = session_core::lane::rlm_tool().parameters;
        input_schema["additionalProperties"] = json!(false);
        vec![json!({
            "name": "rlm",
            "title": "Ultron REPL",
            "description": self.0.tool_description(),
            "inputSchema": input_schema,
        })]
    }
    fn call_tool(
        &self,
        _name: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
        _meta: Map<String, Value>,
    ) -> Result<Value, String> {
        Ok(match args.get("code") {
            Some(Value::String(code)) => self.0.run_cell(code, signal),
            _ => json!({"content": [{"type": "text", "text": "rlm needs `code` (a string)"}], "isError": true}),
        })
    }
    fn prompts(&self) -> Option<Vec<McpPrompt>> {
        Some(vec![McpPrompt {
            name: "ultron-guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL".into(),
            text: self.0.guide_text(),
        }])
    }
    fn resources(&self) -> Option<Vec<McpResource>> {
        Some(vec![McpResource {
            uri: "ultron://guide".into(),
            name: "guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL (the rlm tool)".into(),
            mime_type: "text/markdown".into(),
            text: self.0.guide_text(),
        }])
    }
}

/// What a running server holds: shut down once, by Claude Code disconnecting or by a signal.
pub struct Running {
    pub server: Arc<Server>,
    control: Mutex<Option<crate::control::ControlServer>>,
    parent: Arc<Mutex<Option<ControlClient>>>,
    done: std::sync::atomic::AtomicBool,
}

impl Running {
    /// Shut down like a disconnect (TypeScript's end of `runMcpServer`): stop the control server and unregister,
    /// drop the parent connection, and close the runtime once it opened (kernels are snapshotted, running subagents
    /// and jobs stop). A second call does nothing.
    pub fn shutdown(&self) {
        if self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.server.herdr.release();
        self.server.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(control) = jsrt::sync::lock(&self.control).take() {
            control.close();
        }
        jsrt::sync::lock(&self.parent).take();
        if let Ok(opened) = self.server.runtime() {
            opened.close();
        }
    }

    /// How many `wait-events` waiters have started ([`Server::wait_events`]).
    pub fn event_waiters(&self) -> u64 {
        self.server.waiters.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn socket(&self) -> String {
        self.server.record.socket.clone()
    }

    /// The runtime once it opened (waits for it), or why it failed.
    pub fn opened(&self) -> Result<Arc<Opened>, String> {
        self.server.runtime()
    }
}

/// Run `ultron mcp` on stdio until Claude Code disconnects; returns the exit code. SIGTERM, SIGINT and SIGHUP of
/// this process shut it down the same way and exit 0.
pub fn run(
    argv: &[String],
    ctx: &Ctx,
    stdin: &mut dyn std::io::BufRead,
    stdout: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (running, handlers) = start(argv, ctx)?;
    let signalled = running.clone();
    signals::set_handler(Arc::new(move |_| {
        signalled.shutdown();
        std::process::exit(0);
    }));
    serve(handlers, stdin, stdout);
    running.shutdown();
    signals::clear_handler();
    Ok(0)
}

fn apply_child_cwd(args: &McpServerArgs, env: &mut RunEnv) -> Result<(), String> {
    if !args.child {
        return Ok(());
    }
    if let Some(dir) = (env.var)(crate::child::CHILD_CWD_ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        std::env::set_current_dir(&dir).map_err(|e| format!("{dir}: {e}"))?;
        env.cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn build_server_name(args: &McpServerArgs, claude_session_id: Option<&str>) -> String {
    if args.child {
        return format!("child-{}-{}", std::process::id(), jsrt::random::random_hex(3));
    }
    claude_session_id.map_or_else(
        || {
            format!(
                "mcp-{}-{}",
                std::process::id(),
                crate::launcher::base36(jsrt::date::now_ms() as u64)
            )
        },
        str::to_string,
    )
}

fn connect_parent_client(args: &McpServerArgs) -> Result<Arc<Mutex<Option<ControlClient>>>, String> {
    let client = if args.child {
        Some(ControlClient::connect(
            Path::new(args.parent_socket.as_deref().unwrap_or_default()),
            Duration::from_secs(10),
        )?)
    } else {
        None
    };
    Ok(Arc::new(Mutex::new(client)))
}

fn spawn_runtime_starter(
    server: Arc<Server>,
    claude: Option<String>,
    root_finish: Option<delegation::host::RootFinish>,
) {
    std::thread::spawn(move || {
        let outcome = runtime::open_runtime(&server, claude, root_finish);
        let mut startup = jsrt::sync::lock(&server.startup);
        match outcome {
            Ok(opened) => startup.opened = Some(Arc::new(opened)),
            Err(error) => {
                eprintln!("ultron mcp: runtime failed to start: {error}");
                startup.error = Some(error);
            }
        }
        startup.done = true;
        server.ready.notify_all();
    });
}

fn build_server_record(
    args: &McpServerArgs,
    var: &dyn Fn(&str) -> Option<String>,
    cwd: String,
    dir: &Path,
) -> (String, Option<String>, ServerRecord) {
    let claude_session_id = var("CLAUDE_CODE_SESSION_ID")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let claude_pid = var("CLAUDE_PID")
        .map(|v| jsrt::number::parse(&v))
        .filter(|n| *n != 0.0 && n.is_finite() && n.fract() == 0.0 && *n > 0.0)
        .map(|n| n as u64);
    let name = build_server_name(args, claude_session_id.as_deref());
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&name, dir).display().to_string());
    let record = ServerRecord {
        name: name.clone(),
        pid: std::process::id(),
        socket,
        cwd,
        started_at: jsrt::date::now_ms(),
        claude_session_id: claude_session_id.clone(),
        claude_pid,
        session_id: None,
        parent: args.parent_name.clone().filter(|_| args.child),
    };
    (name, claude_session_id, record)
}

/// Start a server: its runtime opening in the background, its control socket listening. [`serve`] the returned
/// handlers on the MCP connection, then [`Running::shutdown`].
pub fn start(argv: &[String], ctx: &Ctx) -> Result<(Arc<Running>, Arc<Handlers>), String> {
    let args = parse_mcp_args(argv).map_err(|e| e.0)?;
    let mut env = ctx.env.clone();
    let parent_token = (ctx.env.var)(host::PARENT_TOKEN_ENV);
    apply_child_cwd(&args, &mut env)?;
    let var = env.var.clone();
    let cwd = env.cwd.display().to_string();
    let resolved = resolve(&args, &*var, &env.cwd, &env.agent_dir());
    let dir = control_dir(&*var)?;
    let (name, claude_session_id, record) = build_server_record(&args, &*var, cwd, &dir);
    let parent = connect_parent_client(&args)?;
    let world = if args.child {
        crate::shared_world::SharedWorld::forwarding(upstream(parent.clone(), parent_token.clone()))
    } else {
        crate::shared_world::SharedWorld::root()
    };
    let root_finish = args.child.then(|| root_finish(parent.clone(), parent_token));
    let server = Arc::new(Server {
        hints: session_core::compose::hints(&*var),
        args: args.clone(),
        env: env.clone(),
        name,
        record: record.clone(),
        resolved: resolved.clone(),
        claude_session_id,
        startup: Mutex::default(),
        ready: Condvar::new(),
        cells: Mutex::default(),
        child_tokens: ChildTokens::default(),
        persona_nudges: Mutex::new(0),
        waiters: Default::default(),
        closed: Default::default(),
        persona_told: Mutex::new(None),
        shown_playbooks: Mutex::new(Vec::new()),
        herdr: crate::herdr_hook::HerdrHook::start(&env, args.child),
        apis: crate::guide::runtime_apis(&env),
        world,
    });
    let claude = resolve_claude_binary(&*var).filter(|_| resolved.children == ChildrenMode::Claude);
    spawn_runtime_starter(server.clone(), claude, root_finish);
    let handler_server = server.clone();
    let handler: ControlHandler = Arc::new(move |request| handler_server.control(request));
    let control = start_control_server(&record, handler, &dir)?;
    let handlers = Arc::new(Handlers(server.clone()));
    let running = Arc::new(Running {
        server,
        control: Mutex::new(Some(control)),
        parent,
        done: std::sync::atomic::AtomicBool::new(false),
    });
    Ok((running, handlers))
}

/// `rootFinish`: the verdict goes to the parent server as `child.finish` with this server's token.
fn root_finish(parent: Arc<Mutex<Option<ControlClient>>>, token: Option<String>) -> delegation::host::RootFinish {
    Arc::new(move |payload: Map<String, Value>| {
        let mut parent = jsrt::sync::lock(&parent);
        let client = parent
            .as_mut()
            .ok_or("rlm.finish: the parent server is not reachable")?;
        let mut fields = Map::new();
        if let Some(token) = &token {
            fields.insert("token".into(), token.clone().into());
        }
        fields.insert("payload".into(), Value::Object(payload));
        client.request(op::CHILD_FINISH, fields, Duration::from_secs(30))
    })
}

/// A subagent server's shared-world requests go to its parent server with its token (crate::shared_world).
fn upstream(parent: Arc<Mutex<Option<ControlClient>>>, token: Option<String>) -> crate::shared_world::Upstream {
    Arc::new(move |name: Option<&str>, kind: &str, payload: &Map<String, Value>| {
        let mut parent = jsrt::sync::lock(&parent);
        let client = parent
            .as_mut()
            .ok_or_else(|| format!("{kind}: the parent server is not reachable"))?;
        let mut fields = Map::new();
        if let Some(token) = &token {
            fields.insert("token".into(), token.clone().into());
        }
        if let Some(name) = name {
            fields.insert("name".into(), name.into());
        }
        fields.insert("kind".into(), kind.into());
        fields.insert("payload".into(), Value::Object(payload.clone()));
        client.request(op::SHARED, fields, Duration::from_secs(60))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn env(root: &std::path::Path) -> RunEnv {
        let agent = root.join("agent");
        let vars = [
            (config::paths::ENV_AGENT_DIR.to_string(), agent.display().to_string()),
            (config::env::OFFLINE_ENV.to_string(), "1".to_string()),
        ];
        RunEnv {
            cwd: root.to_path_buf(),
            home: root.to_path_buf(),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            package: root.to_path_buf(),
        }
    }

    #[test]
    fn claude_root_frame_model_names_the_claude_code_providers_own_models() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        for id in ["claude-opus-5-5", "claude-haiku-5-5", "opus", "sonnet", "haiku"] {
            let model = runtime::frame_model(&format!("claude-code/{id}"), &env).unwrap();
            assert_eq!((model.provider.as_str(), model.id.as_str()), ("claude-code", id));
        }
        let sonnet = runtime::frame_model("claude-code/sonnet", &env).unwrap();
        assert_eq!(
            (
                sonnet.name.as_str(),
                sonnet.api.as_str(),
                sonnet.context_window,
                sonnet.max_tokens
            ),
            ("Claude Sonnet (Claude Code CLI)", "claude-code-cli", 1_000_000, 128_000)
        );
        std::fs::create_dir_all(tmp.path().join("agent")).unwrap();
        std::fs::write(
            tmp.path().join("agent/settings.json"),
            r#"{"defaultProvider": "groq", "defaultModel": "llama-3.3-70b-versatile"}"#,
        )
        .unwrap();
        for unknown in ["claude-code/claude-sonnet-9", "groq/no-such-model", "nonsense"] {
            let model = runtime::frame_model(unknown, &env).unwrap();
            assert_eq!(
                (model.provider.as_str(), model.id.as_str()),
                ("claude-code", "claude-opus-5-5"),
                "{unknown}"
            );
        }
    }
}
