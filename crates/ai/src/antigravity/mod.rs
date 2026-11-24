//! Ultron's tool loop over `agy acp`. By default (`sessions`) agy stays alive across model calls: a held MCP call is
//! answered with the next tool result and the same turn continues. With `ULTRON_AGY_SESSIONS=0`, or when agy's config
//! cannot take Ultron's relay entry, each model call starts agy, replays the transcript as one prompt and returns the
//! first captured MCP call.
mod bridge;
pub mod catalog;
mod prompt;
mod sessions;

pub use sessions::ensure_relay;

/// Environment variable carrying the JSON argv that `ultron-mcp-relay.py` execs for `ultron agy`.
pub const MCP_EXEC_ENV: &str = "ULTRON_AGY_MCP_EXEC";

use crate::env::ProviderEnv;
use crate::options::SimpleStreamOptions;
use crate::provider::Run;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, EventSink, Message, Model, StopReason, TextContent,
    ThinkingContent, ToolCall, TranscriptContext, Usage,
};
use prompt::{PROMPT_BUDGET, delta_prompt, normalize_mime_type, prompt_blocks_with_system, system_text};
#[cfg(test)]
use prompt::{prompt, prompt_blocks};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// agy keeps about 20K tokens of one external MCP server's tool schemas and silently drops the tools past it
/// (agy issue #1530): 3 characters per token, less a fifth for margin.
const MANIFEST_BUDGET: usize = 48_000;
const STDERR_TAIL: usize = 4096;
const FRAME_LIMIT: u64 = 8 << 20;

/// Normalizes per-turn prompt usage at the driver boundary, falling back to mid-turn `usage_update` context size
/// when `inputTokens` is absent, clamping multi-step cumulative turn sums back to `context_used` so compaction
/// tracks actual context window size, and filling `total_tokens` when `totalTokens` is omitted.
fn normalize_usage(result: &Value, context_used: u64) -> Usage {
    let count = |key: &str| result["usage"][key].as_u64().unwrap_or(0);
    let raw_input = count("inputTokens");
    let output = count("outputTokens");
    let thought = count("thoughtTokens");
    let cache_read = count("cachedReadTokens");
    let cache_write = count("cachedWriteTokens");
    let raw_total = count("totalTokens");
    if context_used > 0
        && (cache_read > context_used
            || raw_total > context_used.saturating_add(output).saturating_mul(2))
    {
        return Usage {
            input: context_used,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: (thought > 0).then_some(thought),
            total_tokens: context_used.saturating_add(output),
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
fn permitted(tool: &Value, names: &HashSet<String>) -> bool {
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
enum Step {
    Prompt(Vec<Value>),
    Answer(Value),
}

/// Workspace directory for an `agy acp` process: ephemeral for one-shot turns, persistent for resumable sessions.
pub(super) enum WorkspaceDir {
    Temp(tempfile::TempDir),
    Persistent(std::path::PathBuf),
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
struct Agy {
    child: Child,
    /// Taken at spawn: `child.id()` is gone once tokio reaps agy, but its MCP servers still need killing.
    group: Option<rustix::process::Pid>,
    input: ChildStdin,
    frames: mpsc::UnboundedReceiver<io::Result<Value>>,
    tail: tokio::task::JoinHandle<Vec<u8>>,
    bridge: bridge::Bridge,
    names: HashSet<String>,
    workspace: Option<WorkspaceDir>,
    session: String,
    next_id: u64,
    /// The `session/prompt` in flight.
    turn: Option<u64>,
    /// The tool call Ultron is running for agy, with the held MCP request it answers.
    held: Option<(String, oneshot::Sender<Value>)>,
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
        let path_env = env
            .and_then(|e| e.get("PATH").cloned())
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let home_env = env
            .and_then(|e| e.get("HOME").cloned())
            .or_else(|| std::env::var("HOME").ok())
            .map(std::path::PathBuf::from);
        let agy_bin =
            catalog::resolve_agy(&path_env, home_env.as_deref()).unwrap_or_else(|| std::path::PathBuf::from("agy"));
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
        let mut child = command.spawn()?;
        let mut stderr = child.stderr.take().expect("piped");
        let tail = tokio::spawn(async move {
            let (mut tail, mut chunk) = (Vec::new(), [0; 4096]);
            while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                tail.extend_from_slice(&chunk[..read]);
                tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
            }
            tail
        });
        let mut output = BufReader::new(child.stdout.take().expect("piped"));
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
    async fn start_in(
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
        Self::start_in(model, messages, env, sessions, WorkspaceDir::Temp(tempfile::tempdir()?)).await
    }

    /// Reattaches a fresh `agy acp` process to a previously saved `session_id` via ACP `session/load`, avoiding a
    /// full transcript replay after an idle eviction or Ultron restart.
    async fn resume(
        model: &Model,
        messages: &[Message],
        env: Option<&ProviderEnv>,
        session_id: String,
        workspace: WorkspaceDir,
    ) -> io::Result<Agy> {
        let mut agy = Self::spawn_in(messages, env, workspace).await?;
        let loaded = async {
            agy.request("initialize", json!({"protocolVersion":1,"clientCapabilities":{}}))
                .await?;
            let cwd = agy.workspace.as_ref().expect("workspace").path().to_path_buf();
            agy.request(
                "session/load",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
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

    async fn send(&mut self, message: Value) -> io::Result<()> {
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
            let Some(method) = frame["method"].as_str() else {
                if frame["id"] == id {
                    return match frame.get("result") {
                        Some(result) => Ok(Event::Reply(result.clone())),
                        None => Err(io::Error::other(format!("agy: {}", frame["error"]["message"]))),
                    };
                }
                continue;
            };
            if !frame["id"].is_null() {
                let reply = if method == "session/request_permission" {
                    let params = &frame["params"];
                    let allowed = permitted(&params["toolCall"], &self.names);
                    let option = params["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|v| v["kind"] == "allow_once" && allowed);
                    let outcome = match option.and_then(|v| v["optionId"].as_str()) {
                        Some(id) => json!({"outcome":"selected","optionId":id}),
                        None => json!({"outcome":"cancelled"}),
                    };
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"outcome":outcome}})
                } else {
                    json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Client capability not supported"}})
                };
                self.send(reply).await?;
            } else if method == "session/update" {
                let update = &frame["params"]["update"];
                if update["sessionUpdate"] == "usage_update" {
                    self.context_used = update["used"].as_u64().unwrap_or(self.context_used);
                } else if self.turn.is_some() && update["sessionUpdate"] == "agent_message_chunk" {
                    let chunk = update["content"]["text"].as_str().unwrap_or_default();
                    if !chunk.is_empty() {
                        self.text.push_str(chunk);
                        if let Some(tx) = deltas {
                            let _ = tx.send(AgyDelta::Text(chunk.to_string()));
                        }
                    }
                    if self.text.len() > FRAME_LIMIT as usize {
                        return Err(io::Error::other("agy response exceeds limit"));
                    }
                } else if self.turn.is_some() && update["sessionUpdate"] == "agent_thought_chunk" {
                    let chunk = update["content"]["text"].as_str().unwrap_or_default();
                    if !chunk.is_empty() {
                        self.thought.push_str(chunk);
                        if let Some(tx) = deltas {
                            let _ = tx.send(AgyDelta::Thinking(chunk.to_string()));
                        }
                    }
                    if self.thought.len() > FRAME_LIMIT as usize {
                        return Err(io::Error::other("agy thought exceeds limit"));
                    }
                }
            }
        }
    }

    /// Runs agy to its next captured tool call or the end of its turn.
    async fn advance(
        &mut self,
        step: Step,
        deltas: Option<&mpsc::UnboundedSender<AgyDelta>>,
    ) -> io::Result<(Vec<AssistantContent>, Usage)> {
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
        let turn = self.turn.ok_or_else(|| io::Error::other("agy has no turn in flight"))?;
        match self.next(turn, deltas).await? {
            Event::Call(call) => {
                let mut content = Vec::new();
                if !self.thought.is_empty() {
                    content.push(AssistantContent::Thinking(ThinkingContent {
                        thinking: std::mem::take(&mut self.thought),
                        thinking_signature: None,
                        redacted: None,
                    }));
                }
                if !self.text.is_empty() {
                    content.push(AssistantContent::Text(TextContent {
                        text: std::mem::take(&mut self.text),
                        text_signature: None,
                    }));
                }
                let id = jsrt::random::random_hex(16);
                self.held = Some((id.clone(), call.reply));
                let call = ToolCall {
                    name: call.name,
                    arguments: call.arguments,
                    id,
                    namespace: None,
                    thought_signature: None,
                };
                content.push(AssistantContent::ToolCall(call));
                let usage = Usage {
                    input: self.context_used,
                    total_tokens: self.context_used,
                    ..Usage::default()
                };
                Ok((content, usage))
            }
            Event::Reply(result) => {
                self.turn = None;
                if result["stopReason"] != "end_turn" {
                    return Err(io::Error::other(format!("agy stopped: {}", result["stopReason"])));
                }
                if self.text.trim().is_empty() {
                    return Err(io::Error::other(
                        "agy ended its turn without text or an Ultron tool call",
                    ));
                }
                let mut content = Vec::new();
                if !self.thought.is_empty() {
                    content.push(AssistantContent::Thinking(ThinkingContent {
                        thinking: std::mem::take(&mut self.thought),
                        thinking_signature: None,
                        redacted: None,
                    }));
                }
                content.push(AssistantContent::Text(TextContent {
                    text: std::mem::take(&mut self.text),
                    text_signature: None,
                }));
                Ok((content, normalize_usage(&result, self.context_used)))
            }
        }
    }

    /// Kills agy and returns `error` with the end of what agy wrote to stderr.
    async fn failed(mut self, error: io::Error) -> io::Error {
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

async fn turn(
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

#[derive(Default)]
struct AgyStream {
    text_index: Option<usize>,
    thought_index: Option<usize>,
}

impl Run<'_, '_, AgyStream> {
    fn delta(&mut self, delta: AgyDelta) {
        match delta {
            AgyDelta::Text(chunk) => {
                let index = self.ensure_text_block();
                if let AssistantContent::Text(block) = &mut self.output.content[index] {
                    block.text.push_str(&chunk);
                }
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: chunk,
                    partial,
                });
            }
            AgyDelta::Thinking(chunk) => {
                let index = self.ensure_thinking_block();
                if let AssistantContent::Thinking(block) = &mut self.output.content[index] {
                    block.thinking.push_str(&chunk);
                }
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: chunk,
                    partial,
                });
            }
        }
    }

    fn ensure_text_block(&mut self) -> usize {
        if let Some(index) = self.state.text_index {
            return index;
        }
        let index = self.output.content.len();
        self.output.content.push(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        }));
        self.state.text_index = Some(index);
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: index,
            partial,
        });
        index
    }

    fn ensure_thinking_block(&mut self) -> usize {
        if let Some(index) = self.state.thought_index {
            return index;
        }
        let index = self.output.content.len();
        self.output.content.push(AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: None,
            redacted: None,
        }));
        self.state.thought_index = Some(index);
        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
            content_index: index,
            partial,
        });
        index
    }
}

pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    let mut run = Run::new(
        AssistantMessage::pending(model, jsrt::date::now_ms()),
        sink,
        AgyStream::default(),
    );
    run.start();
    let signal = options.base.signal.as_ref();
    let bound = Duration::from_millis(options.base.timeout_ms.unwrap_or(300_000));
    let env = options.base.env.as_ref();
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();
    let work = async {
        if sessions::enabled(env) {
            sessions::submit(model.clone(), context.clone(), env.cloned(), delta_tx).await
        } else {
            turn(model, context, env, delta_tx).await
        }
    };
    tokio::pin!(work);
    let sleep = tokio::time::sleep(bound);
    tokio::pin!(sleep);
    let result = loop {
        tokio::select! {
            biased;
            () = crate::http::cancelled(signal) => break Err("Antigravity request aborted".to_string()),
            Some(delta) = delta_rx.recv() => run.delta(delta),
            () = &mut sleep => break Err("Antigravity request timed out".to_string()),
            response = &mut work => {
                while let Ok(delta) = delta_rx.try_recv() {
                    run.delta(delta);
                }
                break response.map_err(|e| e.to_string());
            }
        }
    };
    let ended = result.map(|(content, usage)| {
        run.output.usage = usage;
        let streamed_thought = run.state.thought_index;
        let streamed_text = run.state.text_index;
        if let Some(index) = streamed_thought
            && let Some(AssistantContent::Thinking(block)) = run.output.content.get(index).cloned()
        {
            run.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: block.thinking,
                partial,
            });
        }
        if let Some(index) = streamed_text
            && let Some(AssistantContent::Text(block)) = run.output.content.get(index).cloned()
        {
            run.emit(|partial| AssistantMessageEvent::TextEnd {
                content_index: index,
                content: block.text,
                partial,
            });
        }
        let mut has_tool = false;
        for block in content {
            match block {
                AssistantContent::ToolCall(tool_call) => {
                    has_tool = true;
                    let index = run.output.content.len();
                    run.output.content.push(AssistantContent::ToolCall(tool_call.clone()));
                    run.emit(|partial| AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial,
                    });
                    run.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call,
                        partial,
                    });
                }
                AssistantContent::Text(block) if streamed_text.is_none() => {
                    let index = run.output.content.len();
                    run.output.content.push(AssistantContent::Text(block.clone()));
                    run.emit(|partial| AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial,
                    });
                    run.emit(|partial| AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: block.text.clone(),
                        partial,
                    });
                    run.emit(|partial| AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: block.text,
                        partial,
                    });
                }
                AssistantContent::Thinking(block) if streamed_thought.is_none() => {
                    let index = run.output.content.len();
                    run.output.content.push(AssistantContent::Thinking(block.clone()));
                    run.emit(|partial| AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial,
                    });
                    run.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: block.thinking.clone(),
                        partial,
                    });
                    run.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: block.thinking,
                        partial,
                    });
                }
                _ => {}
            }
        }
        run.output.stop_reason = if has_tool {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    });
    Ok(run.end(ended, signal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn message(value: Value) -> Message {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn prompt_keeps_the_final_message_when_the_system_text_is_huge() {
        let big = "x".repeat(500_000);
        let mut messages = vec![message(json!({"role":"system","content":big,"timestamp":0}))];
        for i in 0..200 {
            messages.push(message(
                json!({"role":"user","content":format!("old {i} {}", "y".repeat(2_000)),"timestamp":0}),
            ));
        }
        messages.push(message(json!({"role":"user","content":"THE REQUEST","timestamp":0})));
        let text = prompt(&messages, 150_000).unwrap();
        assert!(text.len() < 151_000, "{}", text.len());
        assert!(text.contains("THE REQUEST"));
        assert!(text.contains("[system text cut to fit]"));
        assert!(text.contains("oldest messages were left out"));
    }

    #[test]
    fn prompt_is_whole_when_it_fits() {
        let messages = vec![
            message(json!({"role":"system","content":"be brief","timestamp":0})),
            message(json!({"role":"user","content":"hi","timestamp":0})),
        ];
        let text = prompt(&messages, 150_000).unwrap();
        assert!(text.contains("be brief") && text.contains("hi"));
        assert!(!text.contains("cut to fit") && !text.contains("left out"));
    }

    #[test]
    fn prompt_refuses_a_final_message_over_the_budget() {
        let messages = vec![message(
            json!({"role":"user","content":"z".repeat(200_000),"timestamp":0}),
        )];
        let error = prompt(&messages, 150_000)
            .map(|text| text.len())
            .unwrap_err()
            .to_string();
        assert!(error.contains("final message") && error.contains("150000"), "{error}");
    }

    fn fake_agy_options() -> SimpleStreamOptions {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fake-agy");
        let env = [
            ("PATH".to_string(), format!("{fixture}:/usr/bin:/bin")),
            ("ULTRON_AGY_SESSIONS".to_string(), "0".to_string()),
        ]
        .into_iter()
        .collect();
        let mut options = SimpleStreamOptions::default();
        options.base.env = Some(env);
        options
    }

    fn tool_result(text: &str) -> Message {
        message(
            json!({"role":"toolResult","toolCallId":"c","toolName":"rlm","content":[{"type":"text","text":text}],"isError":false,"timestamp":0}),
        )
    }

    #[tokio::test]
    async fn stream_simple_records_agy_usage() {
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let context = TranscriptContext {
            messages: vec![tool_result("one"), tool_result("two")],
        };
        let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
        let usage = &message.usage;
        assert_eq!((usage.input, usage.output, usage.reasoning), (12000, 47, Some(7)));
        assert_eq!(
            (usage.cache_read, usage.cache_write, usage.total_tokens),
            (9000, 100, 12047)
        );
    }

    #[tokio::test]
    async fn turn_errors_carry_agy_stderr() {
        let model = crate::models::antigravity_model("offline-logged-out").unwrap();
        let context = TranscriptContext {
            messages: vec![tool_result("one")],
        };
        let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
            .await
            .unwrap();
        let error = message.error_message.unwrap_or_default();
        assert!(error.contains("agy login"), "{error}");
    }

    fn system(tools: Value) -> Message {
        message(json!({"role":"system","content":"SYSTEM RULES","toolsAdded":tools,"timestamp":0}))
    }

    fn rlm_tool() -> Value {
        json!([{"name":"rlm","description":"run a cell","parameters":{"type":"object"}}])
    }

    fn reply_text(message: &AssistantMessage) -> Value {
        match &message.content[..] {
            [AssistantContent::Text(text)] => serde_json::from_str(&text.text).unwrap(),
            other => panic!("{other:?} {:?}", message.error_message),
        }
    }

    #[test]
    fn permissions_cover_the_inventory_in_both_agy_forms() {
        let names: HashSet<String> = ["rlm".to_string(), "read".to_string()].into();
        let allowed = [
            json!({"name":"mcp_ultron_read"}),
            json!({"title":"Run mcp_ultron_rlm"}),
            json!({"name":"call_mcp_tool","args":{"ServerName":"ultron","ToolName":"read","Arguments":{}}}),
        ];
        let refused = [
            json!({"name":"mcp_ultron_write"}),
            json!({"name":"mcp_other_rlm"}),
            json!({"name":"run_command","title":"Run rlm"}),
            json!({"name":"call_mcp_tool","args":{"ServerName":"other","ToolName":"rlm"}}),
            json!({"name":"call_mcp_tool","args":{"ServerName":"ultron","ToolName":"write"}}),
        ];
        assert!(allowed.iter().all(|tool| permitted(tool, &names)));
        assert!(refused.iter().all(|tool| !permitted(tool, &names)));
    }

    #[tokio::test]
    async fn oversized_tool_schemas_are_an_error() {
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let big = json!([{"name":"rlm","description":"d".repeat(60_000),"parameters":{"type":"object"}}]);
        let context = TranscriptContext {
            messages: vec![
                system(big),
                message(json!({"role":"user","content":"hi","timestamp":0})),
            ],
        };
        let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
            .await
            .unwrap();
        let error = message.error_message.unwrap_or_default();
        assert!(error.contains("tool schemas") && error.contains("48000"), "{error}");
    }

    #[tokio::test]
    async fn the_system_prompt_uses_append_when_advertised() {
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let context = TranscriptContext {
            messages: vec![
                system(json!([])),
                message(json!({"role":"user","content":"hi","timestamp":0})),
            ],
        };
        let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
            .await
            .unwrap();
        let report = reply_text(&message);
        assert_eq!(report["append"], json!("SYSTEM RULES"));
        assert_eq!(report["system"], false);
    }

    #[test]
    fn sessions_are_the_default_and_write_only_ultrons_config_entry() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".gemini/config");
        std::fs::create_dir_all(&config).unwrap();
        let mine = r#"{"mcpServers":{"other":{"serverUrl":"http://x"}},"keep":1}"#;
        std::fs::write(config.join("mcp_config.json"), mine).unwrap();
        let env: ProviderEnv = [
            ("HOME".to_string(), home.path().display().to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]
        .into();
        assert!(sessions::enabled(Some(&env)));
        let written: Value = serde_json::from_slice(&std::fs::read(config.join("mcp_config.json")).unwrap()).unwrap();
        assert_eq!(written["keep"], 1);
        assert_eq!(written["mcpServers"]["other"]["serverUrl"], "http://x");
        assert_eq!(written["mcpServers"]["ultron"]["timeoutSeconds"], -1);
        assert_eq!(
            written["mcpServers"]["ultron"]["tools"],
            json!({"rlm": {"eager": true, "background": "off"}})
        );
        let script = config.join("ultron-mcp-relay.py");
        assert_eq!(written["mcpServers"]["ultron"]["args"], json!([script]));
        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .contains("ULTRON_AGY_BRIDGE_URL")
        );
        let mut off = env.clone();
        off.insert("ULTRON_AGY_SESSIONS".into(), "0".into());
        assert!(!sessions::enabled(Some(&off)));
        std::fs::write(config.join("mcp_config.json"), "[1]").unwrap();
        assert!(
            !sessions::enabled(Some(&env)),
            "a config Ultron cannot extend leaves session mode off"
        );
        assert_eq!(std::fs::read_to_string(config.join("mcp_config.json")).unwrap(), "[1]");
    }

    /// One agy serves a tool loop inside one turn, then a later prompt that carries only the new messages.
    #[tokio::test]
    async fn sessions_replay_after_agy_times_out_a_held_call() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".gemini/config")).unwrap();
        let config = r#"{"mcpServers":{"ultron":{"command":"python3"}}}"#;
        std::fs::write(home.path().join(".gemini/config/mcp_config.json"), config).unwrap();
        let mut options = fake_agy_options();
        let env = options.base.env.as_mut().unwrap();
        env.insert("ULTRON_AGY_SESSIONS".into(), "1".into());
        env.insert("HOME".into(), home.path().display().to_string());
        let flag = home.path().join("cancelled-once");
        env.insert("FAKE_AGY_CANCEL".into(), flag.display().to_string());
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let mut messages = vec![
            system(rlm_tool()),
            message(json!({"role":"user","content":"cancel case","timestamp":0})),
        ];
        for output in ["32", "olleh"] {
            let reply = stream_simple(
                &model,
                &TranscriptContext {
                    messages: messages.clone(),
                },
                &options,
                &mut |_| {},
            )
            .await
            .unwrap();
            assert_eq!(reply.stop_reason, StopReason::ToolUse, "{:?}", reply.error_message);
            let AssistantContent::ToolCall(call) = &reply.content[0] else {
                panic!()
            };
            messages.push(Message::Assistant(reply.clone()));
            messages.push(message(
                json!({"role":"toolResult","toolCallId":call.id,"toolName":"rlm","content":[{"type":"text","text":output}],"isError":false,"timestamp":0}),
            ));
            if output == "32" {
                assert!(flag.exists(), "agy timed out the first held call");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        let last = stream_simple(&model, &TranscriptContext { messages }, &options, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(last.stop_reason, StopReason::Stop, "{:?}", last.error_message);
        assert_eq!(reply_text(&last)["echo"], json!([{"type":"text","text":"olleh"}]));
    }

    #[tokio::test]
    async fn sessions_continue_one_agy_across_model_calls() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".gemini/config")).unwrap();
        let config = r#"{"mcpServers":{"ultron":{"command":"python3"}}}"#;
        std::fs::write(home.path().join(".gemini/config/mcp_config.json"), config).unwrap();
        let mut options = fake_agy_options();
        let env = options.base.env.as_mut().unwrap();
        env.insert("ULTRON_AGY_SESSIONS".into(), "1".into());
        env.insert("HOME".into(), home.path().display().to_string());
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let mut messages = vec![
            system(rlm_tool()),
            message(json!({"role":"user","content":"go","timestamp":0})),
        ];
        for output in ["32", "olleh"] {
            let reply = stream_simple(
                &model,
                &TranscriptContext {
                    messages: messages.clone(),
                },
                &options,
                &mut |_| {},
            )
            .await
            .unwrap();
            assert_eq!(reply.stop_reason, StopReason::ToolUse, "{:?}", reply.error_message);
            assert_eq!(reply.usage.input, 11000, "a tool-call step reports agy's context size");
            let AssistantContent::ToolCall(call) = &reply.content[0] else {
                panic!()
            };
            messages.push(Message::Assistant(reply.clone()));
            messages.push(message(
                json!({"role":"toolResult","toolCallId":call.id,"toolName":"rlm","content":[{"type":"text","text":output}],"isError":false,"timestamp":0}),
            ));
        }
        let first = stream_simple(
            &model,
            &TranscriptContext {
                messages: messages.clone(),
            },
            &options,
            &mut |_| {},
        )
        .await
        .unwrap();
        let report = reply_text(&first);
        assert_eq!(report["echo"], json!([{"type":"text","text":"olleh"}]));
        assert_eq!(
            (
                report["append"].as_str(),
                report["system"].as_bool(),
                report["prompts"].as_u64()
            ),
            (Some("SYSTEM RULES"), Some(false), Some(1))
        );
        assert_eq!(first.usage.input, 12000);
        messages.push(Message::Assistant(first));
        messages.push(message(json!({"role":"user","content":"again","timestamp":0})));
        let second = stream_simple(
            &model,
            &TranscriptContext {
                messages: messages.clone(),
            },
            &options,
            &mut |_| {},
        )
        .await
        .unwrap();
        let again = reply_text(&second);
        assert_eq!(
            (again["pid"].clone(), again["prompts"].as_u64(), again["delta"].as_u64()),
            (report["pid"].clone(), Some(2), Some(1))
        );
        messages.insert(
            2,
            message(json!({"role":"user","content":"edited history","timestamp":0})),
        );
        let fresh = stream_simple(&model, &TranscriptContext { messages }, &options, &mut |_| {})
            .await
            .unwrap();
        assert_ne!(reply_text(&fresh)["pid"], report["pid"]);
    }

    #[tokio::test]
    async fn sessions_keep_live_agy_when_user_or_runtime_event_arrives_mid_tool_call() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".gemini/config")).unwrap();
        let config = r#"{"mcpServers":{"ultron":{"command":"python3"}}}"#;
        std::fs::write(home.path().join(".gemini/config/mcp_config.json"), config).unwrap();
        let mut options = fake_agy_options();
        let env = options.base.env.as_mut().unwrap();
        env.insert("ULTRON_AGY_SESSIONS".into(), "1".into());
        env.insert("HOME".into(), home.path().display().to_string());
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let mut messages = vec![
            system(rlm_tool()),
            message(json!({"role":"user","content":"go","timestamp":0})),
        ];
        for (i, output) in ["32", "olleh"].into_iter().enumerate() {
            let reply = stream_simple(
                &model,
                &TranscriptContext {
                    messages: messages.clone(),
                },
                &options,
                &mut |_| {},
            )
            .await
            .unwrap();
            assert_eq!(reply.stop_reason, StopReason::ToolUse, "{:?}", reply.error_message);
            let AssistantContent::ToolCall(call) = &reply.content[0] else {
                panic!()
            };
            messages.push(Message::Assistant(reply.clone()));
            messages.push(message(
                json!({"role":"toolResult","toolCallId":call.id,"toolName":"rlm","content":[{"type":"text","text":output}],"isError":false,"timestamp":0}),
            ));
            if i == 1 {
                messages.push(message(
                    json!({"role":"user","content":"<runtime_event kind=\"cell_done\" id=\"cell-1\"/>","timestamp":0}),
                ));
            }
        }
        let done = stream_simple(
            &model,
            &TranscriptContext {
                messages: messages.clone(),
            },
            &options,
            &mut |_| {},
        )
        .await
        .unwrap();
        let report = reply_text(&done);
        assert_eq!(report["prompts"].as_u64(), Some(1), "agy must stay in one live turn");
        assert_eq!(
            report["echo"],
            json!([
                {"type": "text", "text": "olleh"},
                {"type": "text", "text": "[Message received while tool ran]\n<runtime_event kind=\"cell_done\" id=\"cell-1\"/>"}
            ])
        );
    }

    #[tokio::test]
    async fn session_load_resumes_with_delta_prompt_and_ignores_replay_chunks() {
        let model = crate::models::antigravity_model("offline-test").unwrap();
        let options = fake_agy_options();
        let messages = vec![
            system(rlm_tool()),
            message(json!({"role":"user","content":"follow up","timestamp":0})),
        ];
        let mut agy = Agy::resume(
            &model,
            &messages,
            options.base.env.as_ref(),
            "offline".into(),
            WorkspaceDir::Temp(tempfile::tempdir().unwrap()),
        )
        .await
        .unwrap();
        let prompt = delta_prompt(&messages[1..]).unwrap();
        let (content, _) = agy.advance(Step::Prompt(prompt), None).await.unwrap();
        let [AssistantContent::Text(text)] = &content[..] else {
            panic!("{content:?}");
        };
        assert!(!text.text.contains("HISTORICAL-REPLAY"), "{}", text.text);
        let report: Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(report["loaded"], true);
        assert_eq!(report["delta"], 1);
    }

    #[test]
    fn prompt_blocks_extracts_images_as_native_acp_blocks() {
        let messages = vec![message(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image", "data": "QUJD", "mimeType": "image/png"}
            ],
            "timestamp": 0
        }))];
        let blocks = prompt_blocks(&messages, 150_000).unwrap();
        assert_eq!(blocks.len(), 2);
        let text = blocks[0]["text"].as_str().unwrap();
        assert!(text.contains("[attached image #1]"));
        assert!(text.contains("An MCP timeout or dropped tool wait does not mean"));
        assert!(!text.contains("QUJD"));
        assert_eq!(
            blocks[1],
            json!({"type": "image", "data": "QUJD", "mimeType": "image/png"})
        );
    }

    #[test]
    fn prompt_blocks_strips_mime_parameters_on_image_attachments() {
        let messages = vec![message(json!({
            "role": "user",
            "content": [
                {"type": "image", "data": "QUJD", "mimeType": " image/png ; charset=binary"}
            ],
            "timestamp": 0
        }))];
        let blocks = prompt_blocks(&messages, 150_000).unwrap();
        assert_eq!(
            blocks[1],
            json!({"type": "image", "data": "QUJD", "mimeType": "image/png"})
        );
    }

    #[test]
    fn normalize_usage_fills_missing_totals_and_falls_back_to_context_used() {
        let missing_total = normalize_usage(&json!({"usage": {"inputTokens": 500, "outputTokens": 25}}), 999);
        assert_eq!(
            (missing_total.input, missing_total.output, missing_total.total_tokens),
            (500, 25, 525)
        );

        let fallback = normalize_usage(&json!({"usage": {"outputTokens": 10}}), 4200);
        assert_eq!(
            (fallback.input, fallback.output, fallback.total_tokens),
            (4200, 10, 4210)
        );

        let cumulative = normalize_usage(
            &json!({"usage": {
                "inputTokens": 21_093,
                "cachedReadTokens": 494_396,
                "outputTokens": 1_256,
                "totalTokens": 516_745
            }}),
            105_069,
        );
        assert_eq!(
            (cumulative.input, cumulative.cache_read, cumulative.output, cumulative.total_tokens),
            (105_069, 0, 1_256, 106_325)
        );
    }

    #[test]
    fn prompt_blocks_extracts_file_attachments_as_acp_embedded_resources() {
        let messages = vec![message(json!({
            "role": "user",
            "content": "<file name=\"/tmp/foo.rs\">\nfn main() {}\n</file>\nexplain this",
            "timestamp": 0
        }))];
        let blocks = prompt_blocks(&messages, 150_000).unwrap();
        assert_eq!(blocks.len(), 2);
        let text = blocks[0]["text"].as_str().unwrap();
        assert!(text.contains("[attached file #1: /tmp/foo.rs]"));
        assert!(!text.contains("fn main() {}"));
        assert_eq!(
            blocks[1],
            json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/foo.rs",
                    "mimeType": "text/plain",
                    "text": "fn main() {}"
                }
            })
        );
    }
}
