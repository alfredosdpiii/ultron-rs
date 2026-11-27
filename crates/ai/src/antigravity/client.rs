//! ACP child process client for `agy acp`: process spawn, JSON-RPC framing, permission checks, and turn execution.

use super::bridge;
use super::catalog;
use super::prompt::{PROMPT_BUDGET, prompt_blocks_with_system, system_text};
use crate::env::ProviderEnv;
use crate::types::{
    AssistantContent, Message, Model, TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// agy keeps about 20K tokens of one external MCP server's tool schemas and silently drops the tools past it
/// (agy issue #1530): 3 characters per token, less a fifth for margin.
const MANIFEST_BUDGET: usize = 48_000;
const STDERR_TAIL: usize = 4096;
const FRAME_LIMIT: u64 = 8 << 20;

/// Detects `agy` quota/limit warning banners emitted as `end_turn` text with zero token usage so they surface as
/// errors instead of being committed to the transcript as assistant replies.
pub(super) fn quota_banner_error(text: &str, usage: &Usage) -> Option<String> {
    if usage.total_tokens > 0 || usage.input > 0 || usage.output > 0 {
        return None;
    }
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let is_quota =
        trimmed.starts_with('⚠') && (lower.contains("quota") || lower.contains("limit") || lower.contains("rate"));
    let is_wallet = lower.contains("prepaid wallet") || lower.contains("exhausted your quota");
    (is_quota || is_wallet).then(|| trimmed.to_string())
}

/// Normalizes per-turn prompt usage at the driver boundary, falling back to mid-turn `usage_update` context size
/// when `inputTokens` is absent, clamping multi-step cumulative turn sums back to `context_used` so compaction
/// tracks actual context window size, and filling `total_tokens` when `totalTokens` is omitted.
pub(super) fn normalize_usage(result: &Value, context_used: u64) -> Usage {
    let count = |key: &str| result["usage"][key].as_u64().unwrap_or(0);
    let raw_input = count("inputTokens");
    let output = count("outputTokens");
    let thought = count("thoughtTokens");
    let cache_read = count("cachedReadTokens");
    let cache_write = count("cachedWriteTokens");
    let raw_total = count("totalTokens");
    let doubled_context = context_used.saturating_add(output).saturating_mul(2);
    if context_used > 0 && (cache_read > context_used || raw_total > doubled_context) {
        return Usage {
            input: context_used,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: (thought > 0).then_some(thought),
            total_tokens: context_used,
            ..Usage::default()
        };
    }
    let input = if raw_input > 0 { raw_input } else { context_used };
    let total_tokens = if raw_total > 0 {
        raw_total
    } else {
        input.saturating_add(output)
    };
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: (thought > 0).then_some(thought),
        total_tokens,
        ..Usage::default()
    }
}

/// Whether an ACP permission request is for a tool of this request's inventory, in agy's `mcp_ultron_<tool>` form
/// (a session's own server) or its `call_mcp_tool` form (a server from agy's config).
pub(super) fn permitted(tool: &Value, names: &HashSet<String>) -> bool {
    if tool["name"] == "call_mcp_tool" {
        let args = &tool["args"];
        return args["ServerName"] == "ultron" && args["ToolName"].as_str().is_some_and(|n| names.contains(n));
    }
    names.iter().any(|name| {
        let qualified = format!("mcp_ultron_{name}");
        match tool["name"].as_str() {
            Some(tool_name) => tool_name == qualified,
            None => tool["title"] == format!("Run {qualified}"),
        }
    })
}

/// Live chunk streamed by `agy acp` during a turn.
#[derive(Clone, Debug)]
pub(super) enum AgyDelta {
    Text(String),
    Thinking(String),
}

enum Event {
    Reply(Value),
    Call(bridge::Captured),
}

/// What advances agy to Ultron's next model response.
pub(super) enum Step {
    Prompt(Vec<Value>),
    Answer(Value),
}

/// Workspace directory for an `agy acp` process: ephemeral for one-shot turns, persistent for resumable sessions.
pub(super) enum WorkspaceDir {
    Temp(tempfile::TempDir),
    Persistent(PathBuf),
}

impl WorkspaceDir {
    pub(super) fn path(&self) -> &std::path::Path {
        match self {
            WorkspaceDir::Temp(dir) => dir.path(),
            WorkspaceDir::Persistent(path) => path.as_path(),
        }
    }
}

/// One running `agy acp` with its tool bridge; dropping it kills agy's process group.
pub(super) struct Agy {
    pub(super) child: Child,
    /// Taken at spawn: `child.id()` is gone once tokio reaps agy, but its MCP servers still need killing.
    group: Option<rustix::process::Pid>,
    input: ChildStdin,
    frames: mpsc::UnboundedReceiver<io::Result<Value>>,
    tail: tokio::task::JoinHandle<Vec<u8>>,
    bridge: bridge::Bridge,
    names: HashSet<String>,
    pub(super) workspace: Option<WorkspaceDir>,
    pub(super) session: String,
    next_id: u64,
    /// The `session/prompt` in flight.
    pub(super) turn: Option<u64>,
    /// The tool call Ultron is running for agy, with the held MCP request it answers.
    pub(super) held: Option<(String, oneshot::Sender<Value>)>,
    text: String,
    thought: String,
    /// The context agy last reported in a `usage_update` (`used`), the only size it sends mid-turn.
    context_used: u64,
}

impl Drop for Agy {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

fn resolve_agy_bin(env: Option<&ProviderEnv>) -> PathBuf {
    let path_env = env
        .and_then(|e| e.get("PATH").cloned())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let home_env = env
        .and_then(|e| e.get("HOME").cloned())
        .or_else(|| std::env::var("HOME").ok())
        .map(PathBuf::from);
    catalog::resolve_agy(&path_env, home_env.as_deref()).unwrap_or_else(|| PathBuf::from("agy"))
}

fn build_agy_command(
    agy_bin: PathBuf,
    workspace: &WorkspaceDir,
    env: Option<&ProviderEnv>,
    bridge: &bridge::Bridge,
) -> Command {
    let mut command = Command::new(agy_bin);
    command
        .args([
            "acp",
            "--stdio",
            "--sandbox",
            "--include-default-customizations=false",
            "--disable-telemetry",
        ])
        .current_dir(workspace.path())
        .process_group(0)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = env {
        command.envs(env);
    }
    command
        .env("ULTRON_AGY_BRIDGE_URL", &bridge.url)
        .env("ULTRON_AGY_BRIDGE_TOKEN", &bridge.token);
    command
}

fn spawn_stderr_tail(mut stderr: ChildStderr) -> tokio::task::JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (mut tail, mut chunk) = (Vec::new(), [0; 4096]);
        while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
            tail.extend_from_slice(&chunk[..read]);
            tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
        }
        tail
    })
}

fn spawn_frame_reader(stdout: ChildStdout) -> mpsc::UnboundedReceiver<io::Result<Value>> {
    let mut output = BufReader::new(stdout);
    let (sender, frames) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let mut line = Vec::new();
            let frame = match (&mut output).take(FRAME_LIMIT).read_until(b'\n', &mut line).await {
                Ok(_) if line.last() == Some(&b'\n') => serde_json::from_slice(&line).map_err(io::Error::from),
                Ok(_) => Err(io::Error::other("agy closed stdout or exceeded ACP frame limit")),
                Err(error) => Err(error),
            };
            let last = frame.is_err();
            if sender.send(frame).is_err() || last {
                break;
            }
        }
    });
    frames
}

fn handle_reply(frame: &Value, id: u64) -> Option<io::Result<Event>> {
    if frame["id"] != id {
        return None;
    }
    Some(match frame.get("result") {
        Some(result) => Ok(Event::Reply(result.clone())),
        None => Err(io::Error::other(format!("agy: {}", frame["error"]["message"]))),
    })
}

fn build_server_reply(frame: &Value, method: &str, names: &HashSet<String>) -> Value {
    if method != "session/request_permission" {
        return json!({
            "jsonrpc": "2.0",
            "id": frame["id"],
            "error": {"code": -32601, "message": "Client capability not supported"}
        });
    }
    let params = &frame["params"];
    let allowed = permitted(&params["toolCall"], names);
    let option = params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["kind"] == "allow_once" && allowed);
    let outcome = match option.and_then(|v| v["optionId"].as_str()) {
        Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
        None => json!({"outcome": "cancelled"}),
    };
    json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"outcome": outcome}})
}

fn push_stream_chunk(
    target: &mut String,
    chunk: &str,
    limit_error: &str,
    make_delta: impl FnOnce(String) -> AgyDelta,
    deltas: Option<&mpsc::UnboundedSender<AgyDelta>>,
) -> io::Result<()> {
    if !chunk.is_empty() {
        target.push_str(chunk);
        if let Some(tx) = deltas {
            let _ = tx.send(make_delta(chunk.to_string()));
        }
    }
    if target.len() > FRAME_LIMIT as usize {
        return Err(io::Error::other(limit_error));
    }
    Ok(())
}

impl Agy {
    /// Spawns `agy acp` in `workspace` with its loopback MCP bridge ready.
    async fn spawn_in(messages: &[Message], env: Option<&ProviderEnv>, workspace: WorkspaceDir) -> io::Result<Agy> {
        let tools = crate::transcript::current_tools(messages);
        let manifest = bridge::manifest(&tools);
        let size = manifest.to_string().len();
        if size > MANIFEST_BUDGET {
            return Err(io::Error::other(format!(
                "The {} tool schemas are {size} bytes, over the {MANIFEST_BUDGET} agy keeps for one MCP server; agy would drop tools silently",
                tools.len()
            )));
        }
        let bridge = bridge::Bridge::start(manifest).await?;
        let mut command = build_agy_command(resolve_agy_bin(env), &workspace, env, &bridge);
        let mut child = command.spawn()?;
        let tail = spawn_stderr_tail(child.stderr.take().expect("piped"));
        let frames = spawn_frame_reader(child.stdout.take().expect("piped"));
        Ok(Agy {
            input: child.stdin.take().expect("piped"),
            group: child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)),
            child,
            frames,
            tail,
            names: tools.into_iter().map(|t| t.name).collect(),
            bridge,
            workspace: Some(workspace),
            session: String::new(),
            next_id: 0,
            turn: None,
            held: None,
            text: String::new(),
            thought: String::new(),
            context_used: 0,
        })
    }

    /// Starts a fresh `agy` session in `workspace` and returns the step that sends their full replay.
    pub(super) async fn start_in(
        model: &Model,
        messages: &[Message],
        env: Option<&ProviderEnv>,
        sessions: bool,
        workspace: WorkspaceDir,
    ) -> io::Result<(Agy, Step)> {
        let mut agy = Self::spawn_in(messages, env, workspace).await?;
        match agy.open(model, messages, sessions).await {
            Ok(step) => Ok((agy, step)),
            Err(error) => Err(agy.failed(error).await),
        }
    }

    /// Starts a fresh `agy` session for `messages` in a temporary workspace.
    async fn start(
        model: &Model,
        messages: &[Message],
        env: Option<&ProviderEnv>,
        sessions: bool,
    ) -> io::Result<(Agy, Step)> {
        Self::start_in(
            model,
            messages,
            env,
            sessions,
            WorkspaceDir::Temp(tempfile::tempdir_in(jsrt::fs::temp_dir())?),
        )
        .await
    }

    /// Reattaches a fresh `agy acp` process to a previously saved `session_id` via ACP `session/resume` (when
    /// advertised, avoiding history replay over stdout) or `session/load`, avoiding a full transcript replay after an
    /// idle eviction or Ultron restart.
    pub(super) async fn resume(
        model: &Model,
        messages: &[Message],
        env: Option<&ProviderEnv>,
        session_id: String,
        workspace: WorkspaceDir,
    ) -> io::Result<Agy> {
        let mut agy = Self::spawn_in(messages, env, workspace).await?;
        let loaded = async {
            let init = agy
                .request("initialize", json!({"protocolVersion":1,"clientCapabilities":{}}))
                .await?;
            let method = if init["agentCapabilities"]["sessionCapabilities"]["resume"].is_object() {
                "session/resume"
            } else {
                "session/load"
            };
            let cwd = agy.workspace.as_ref().expect("workspace").path().to_path_buf();
            agy.request(method, json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}))
                .await?;
            agy.session = session_id;
            agy.request(
                "session/set_model",
                json!({"sessionId": agy.session, "modelId": model.id}),
            )
            .await?;
            Ok(())
        }
        .await;
        match loaded {
            Ok(()) => Ok(agy),
            Err(error) => Err(agy.failed(error).await),
        }
    }

    async fn open(&mut self, model: &Model, messages: &[Message], sessions: bool) -> io::Result<Step> {
        let init = self
            .request("initialize", json!({"protocolVersion":1,"clientCapabilities":{}}))
            .await?;
        let supports_append =
            init["agentCapabilities"]["_meta"]["antigravity"]["systemPrompt"]["append"].as_bool() == Some(true);
        let append = supports_append.then(|| system_text(messages)).flatten();
        let servers = if sessions {
            json!([])
        } else {
            let authorization = format!("Bearer {}", self.bridge.token);
            json!([{"type":"http","name":"ultron","url":self.bridge.url,"headers":[{"name":"Authorization","value":authorization}]}])
        };
        let cwd = self.workspace.as_ref().expect("workspace").path();
        let mut params = json!({"cwd": cwd, "mcpServers": servers});
        if let Some(ref text) = append {
            params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": text}}});
        }
        let session = self.request("session/new", params).await?;
        self.session = session["sessionId"]
            .as_str()
            .ok_or_else(|| io::Error::other("agy did not return a session ID"))?
            .to_string();
        self.request(
            "session/set_model",
            json!({"sessionId":self.session,"modelId":model.id}),
        )
        .await?;
        Ok(Step::Prompt(prompt_blocks_with_system(
            messages,
            PROMPT_BUDGET,
            append.is_none(),
        )?))
    }

    pub(super) async fn send(&mut self, message: Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        self.input.write_all(&line).await
    }

    async fn call(&mut self, method: &str, params: Value) -> io::Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    async fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.call(method, params).await?;
        match self.next(id, None).await? {
            Event::Reply(result) => Ok(result),
            Event::Call(_) => unreachable!("tool calls are watched only during a prompt"),
        }
    }

    fn handle_session_update(
        &mut self,
        update: &Value,
        deltas: Option<&mpsc::UnboundedSender<AgyDelta>>,
    ) -> io::Result<()> {
        match update["sessionUpdate"].as_str() {
            Some("usage_update") => {
                self.context_used = update["used"].as_u64().unwrap_or(self.context_used);
                Ok(())
            }
            Some("agent_message_chunk") if self.turn.is_some() => {
                let chunk = update["content"]["text"].as_str().unwrap_or_default();
                push_stream_chunk(
                    &mut self.text,
                    chunk,
                    "agy response exceeds limit",
                    AgyDelta::Text,
                    deltas,
                )
            }
            Some("agent_thought_chunk") if self.turn.is_some() => {
                let chunk = update["content"]["text"].as_str().unwrap_or_default();
                push_stream_chunk(
                    &mut self.thought,
                    chunk,
                    "agy thought exceeds limit",
                    AgyDelta::Thinking,
                    deltas,
                )
            }
            _ => Ok(()),
        }
    }

    async fn dispatch_frame(
        &mut self,
        frame: &Value,
        id: u64,
        deltas: Option<&mpsc::UnboundedSender<AgyDelta>>,
    ) -> io::Result<Option<Event>> {
        let Some(method) = frame["method"].as_str() else {
            return handle_reply(frame, id).transpose();
        };
        if !frame["id"].is_null() {
            let reply = build_server_reply(frame, method, &self.names);
            self.send(reply).await?;
        } else if method == "session/update" {
            self.handle_session_update(&frame["params"]["update"], deltas)?;
        }
        Ok(None)
    }

    /// Serves agy's requests and collects its text until the reply to `id` or, during a prompt, a captured tool call.
    async fn next(&mut self, id: u64, deltas: Option<&mpsc::UnboundedSender<AgyDelta>>) -> io::Result<Event> {
        loop {
            let frame = tokio::select! {
                biased;
                call = self.bridge.calls.recv(), if self.turn == Some(id) => {
                    return call.map(Event::Call).ok_or_else(|| io::Error::other("Ultron tool bridge closed"));
                }
                frame = self.frames.recv() => frame.unwrap_or_else(|| Err(io::Error::other("agy closed stdout")))?,
            };
            if let Some(event) = self.dispatch_frame(&frame, id, deltas).await? {
                return Ok(event);
            }
        }
    }

    async fn start_step(&mut self, step: Step) -> io::Result<u64> {
        match step {
            Step::Prompt(blocks) => {
                self.context_used = 0;
                let params = json!({"sessionId": self.session, "prompt": blocks});
                self.turn = Some(self.call("session/prompt", params).await?);
            }
            Step::Answer(result) => {
                let (_, held) = self
                    .held
                    .take()
                    .ok_or_else(|| io::Error::other("no held agy tool call"))?;
                held.send(result)
                    .map_err(|_| io::Error::other("agy dropped its held tool call"))?;
            }
        }
        self.text.clear();
        self.thought.clear();
        self.turn.ok_or_else(|| io::Error::other("agy has no turn in flight"))
    }

    fn push_buffered_thought(&mut self, content: &mut Vec<AssistantContent>) {
        if !self.thought.is_empty() {
            content.push(AssistantContent::Thinking(ThinkingContent {
                thinking: std::mem::take(&mut self.thought),
                thinking_signature: None,
                redacted: None,
            }));
        }
    }

    fn finish_tool_call(&mut self, call: bridge::Captured) -> (Vec<AssistantContent>, Usage) {
        let mut content = Vec::new();
        self.push_buffered_thought(&mut content);
        if !self.text.is_empty() {
            content.push(AssistantContent::Text(TextContent {
                text: std::mem::take(&mut self.text),
                text_signature: None,
            }));
        }
        let id = jsrt::random::random_hex(16);
        self.held = Some((id.clone(), call.reply));
        content.push(AssistantContent::ToolCall(ToolCall {
            name: call.name,
            arguments: call.arguments,
            id,
            namespace: None,
            thought_signature: None,
        }));
        let usage = Usage {
            input: self.context_used,
            total_tokens: self.context_used,
            ..Usage::default()
        };
        (content, usage)
    }

    fn finish_turn_reply(&mut self, result: Value) -> io::Result<(Vec<AssistantContent>, Usage)> {
        self.turn = None;
        if result["stopReason"] != "end_turn" {
            return Err(io::Error::other(format!("agy stopped: {}", result["stopReason"])));
        }
        if self.text.trim().is_empty() {
            return Err(io::Error::other(
                "agy ended its turn without text or an Ultron tool call",
            ));
        }
        let usage = normalize_usage(&result, self.context_used);
        if let Some(banner) = quota_banner_error(&self.text, &usage) {
            return Err(io::Error::other(format!("agy: {banner}")));
        }
        let mut content = Vec::new();
        self.push_buffered_thought(&mut content);
        content.push(AssistantContent::Text(TextContent {
            text: std::mem::take(&mut self.text),
            text_signature: None,
        }));
        Ok((content, usage))
    }

    /// Runs agy to its next captured tool call or the end of its turn.
    pub(super) async fn advance(
        &mut self,
        step: Step,
        deltas: Option<&mpsc::UnboundedSender<AgyDelta>>,
    ) -> io::Result<(Vec<AssistantContent>, Usage)> {
        let turn = self.start_step(step).await?;
        match self.next(turn, deltas).await? {
            Event::Call(call) => Ok(self.finish_tool_call(call)),
            Event::Reply(result) => self.finish_turn_reply(result),
        }
    }

    /// Kills agy and returns `error` with the end of what agy wrote to stderr.
    pub(super) async fn failed(mut self, error: io::Error) -> io::Error {
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let tail = tokio::time::timeout(Duration::from_secs(1), &mut self.tail)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        match String::from_utf8_lossy(&tail).trim() {
            "" => error,
            tail => io::Error::new(error.kind(), format!("{error}; agy stderr: {tail}")),
        }
    }
}

pub(super) async fn turn(
    model: &Model,
    context: &TranscriptContext,
    env: Option<&ProviderEnv>,
    deltas: mpsc::UnboundedSender<AgyDelta>,
) -> io::Result<(Vec<AssistantContent>, Usage)> {
    let (mut agy, step) = Agy::start(model, &context.messages, env, false).await?;
    match agy.advance(step, Some(&deltas)).await {
        Ok(done) => Ok(done),
        Err(error) => Err(agy.failed(error).await),
    }
}
