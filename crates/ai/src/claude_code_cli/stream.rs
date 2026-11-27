//! Stream-json event loop, usage/error classification, and `stream` / `stream_simple` entry points for `claude -p`.

use super::probe::{ClaudeCliCapabilities, empty_work_dir, ensure_claude_cli, resolve_claude_cli, tail};
use super::process::{ChildEvent, CliProcess, PROVEN_KEYS, acquire, start_spare, take_spare};
use super::request::{PlannedRequest, payload, plan_request};
use super::{
    ClaudeCodeOptions, Env, RuntimeConfig, TOOLS_UNSUPPORTED, USAGE_LIMIT_DIAGNOSTIC, child_env, runtime_config,
};
use crate::provider::Run;
use crate::transcript::current_tools;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, DiagnosticErrorInfo,
    EventSink, Model, StopReason, TextContent, ThinkingContent, TranscriptContext, Usage,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)
}

/// `usageFromResult`: token counts from the result event; the cost is the CLI's `total_cost_usd`, or unknown (NaN).
pub fn usage_from_result(result: &Value) -> Usage {
    let usage = result
        .get("usage")
        .filter(|u| u.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let count = |key: &str| num(usage.get(key)).unwrap_or(0.0) as u64;
    let mut out = Usage {
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_write: count("cache_creation_input_tokens"),
        ..Usage::default()
    };
    if let Some(long) = num(usage.pointer("/cache_creation/ephemeral_1h_input_tokens")).filter(|n| *n > 0.0) {
        out.cache_write_1h = Some(long as u64);
    }
    if let Some(thinking) = num(usage.pointer("/output_tokens_details/thinking_tokens")) {
        out.reasoning = Some(thinking as u64);
    }
    out.total_tokens = out.input + out.output + out.cache_read + out.cache_write;
    out.cost.total = num(result.get("total_cost_usd")).unwrap_or(f64::NAN);
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitInfo {
    pub status: Option<String>,
    pub rate_limit_type: Option<String>,
    pub resets_at: Option<f64>,
}

pub fn rate_limit_info(event: &Value) -> Option<RateLimitInfo> {
    let info = event.get("rate_limit_info").filter(|i| i.is_object())?;
    Some(RateLimitInfo {
        status: info.get("status").and_then(Value::as_str).map(str::to_string),
        rate_limit_type: info.get("rateLimitType").and_then(Value::as_str).map(str::to_string),
        resets_at: num(info.get("resetsAt")),
    })
}

pub fn reset_text(info: Option<&RateLimitInfo>) -> String {
    match info.and_then(|i| i.resets_at).filter(|r| *r != 0.0) {
        Some(resets_at) => {
            let ms = if resets_at < 1e12 {
                resets_at * 1000.0
            } else {
                resets_at
            };
            jsrt::date::time_clip(ms)
                .map_or_else(String::new, |ms| format!("; resets at {}", jsrt::date::iso_string(ms)))
        }
        None => String::new(),
    }
}

enum Failure {
    Length,
    SchemaRejected(String),
    Error { message: String, usage_limit: bool },
}

fn contains_ci(text: &str, needle: &str) -> bool {
    text.to_lowercase().contains(needle)
}

fn is_usage_limit(lower: &str, rate_limit: Option<&RateLimitInfo>) -> bool {
    rate_limit.and_then(|r| r.status.as_deref()) == Some("rejected")
        || ["usage limit", "limit reached", "out of usage", "out of extra usage"]
            .iter()
            .any(|needle| lower.contains(needle))
}

fn usage_limit_failure(text: String, rate_limit: Option<&RateLimitInfo>) -> Failure {
    let window = match rate_limit.and_then(|r| r.rate_limit_type.as_deref()) {
        Some(kind) => format!(" ({kind} window{})", reset_text(rate_limit)),
        None => String::new(),
    };
    Failure::Error {
        message: format!(
            "Claude Code usage limit reached{window}: {text}. The subscription's capacity is exhausted for now; this is not retried."
        ),
        usage_limit: true,
    }
}

fn is_auth_failure(text: &str) -> bool {
    [
        "not logged in",
        "please run /login",
        "invalid api key",
        "authentication",
    ]
    .iter()
    .any(|n| contains_ci(text, n))
}

fn classify_failure(result: &Value, rate_limit: Option<&RateLimitInfo>) -> Failure {
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown error")
        .to_string();
    let status = num(result.get("api_error_status")).map(|s| s as u64);
    let lower = text.to_lowercase();
    if output_limit_exceeded(&lower) {
        return Failure::Length;
    }
    if status == Some(400) && lower.contains("input_schema") {
        return Failure::SchemaRejected(text);
    }
    if is_usage_limit(&lower, rate_limit) {
        return usage_limit_failure(text, rate_limit);
    }
    if status == Some(429) || crate::jsutil::loose_match(&text, "rate", "limit") {
        return Failure::Error {
            message: format!("Claude Code rate limited (429): {text}"),
            usage_limit: false,
        };
    }
    if status == Some(529) || lower.contains("overloaded") {
        return Failure::Error {
            message: format!("Claude Code overloaded (529): {text}"),
            usage_limit: false,
        };
    }
    if is_auth_failure(&text) {
        return Failure::Error {
            message: format!("Claude Code CLI authentication failed: {text}. Run `claude auth login` and try again."),
            usage_limit: false,
        };
    }
    Failure::Error {
        message: format!(
            "Claude Code CLI error{}: {text}",
            status.filter(|s| *s != 0).map_or_default(|s| format!(" ({s})"))
        ),
        usage_limit: false,
    }
}

/// `/exceeded the \d+ output token maximum/i`.
fn output_limit_exceeded(lower: &str) -> bool {
    let mut rest = lower;
    while let Some(index) = rest.find("exceeded the ") {
        let after = &rest[index + "exceeded the ".len()..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(" output token maximum") {
            return true;
        }
        rest = &rest[index + 1..];
    }
    false
}

fn diagnostic(kind: &str, message: &str) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind: kind.into(),
        timestamp: jsrt::date::now_ms(),
        error: Some(DiagnosticErrorInfo {
            name: None,
            message: message.into(),
            stack: None,
            code: None,
        }),
        details: None,
    }
}

/// Emit the closing `TextEnd` or `ThinkingEnd` event for a completed content block.
pub(crate) fn emit_text_or_thinking_end<S>(run: &mut Run<'_, '_, S>, content_index: usize) {
    match &run.output.content[content_index] {
        AssistantContent::Text(block) => {
            let content = block.text.clone();
            run.emit(|partial| AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            });
        }
        AssistantContent::Thinking(block) => {
            let content = block.thinking.clone();
            run.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            });
        }
        AssistantContent::ToolCall(_) => {}
    }
}

struct CliStream {
    blocks: HashMap<i64, usize>,
    streamed: HashSet<String>,
    rate_limit: Option<RateLimitInfo>,
    usage_known: bool,
    warnings: Vec<String>,
}

enum RunError {
    Aborted(String),
    Failed { message: String, usage_limit: bool },
}

impl Run<'_, '_, CliStream> {
    fn warn(&mut self, message: String) {
        if !self.state.warnings.contains(&message) {
            self.state.warnings.push(message);
        }
    }

    fn add_text(&mut self, text: &str) {
        self.output.content.push(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        }));
        let index = self.output.content.len() - 1;
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: index,
            partial,
        });
        if let AssistantContent::Text(block) = &mut self.output.content[index] {
            block.text = text.to_string();
        }
        let delta = text.to_string();
        self.emit(|partial| AssistantMessageEvent::TextDelta {
            content_index: index,
            delta,
            partial,
        });
        let content = text.to_string();
        self.emit(|partial| AssistantMessageEvent::TextEnd {
            content_index: index,
            content,
            partial,
        });
    }

    fn on_message_start(&mut self, event: &Value) {
        self.state.blocks.clear();
        if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
            self.state.streamed.insert(id.to_string());
            self.output.response_id = Some(id.to_string());
        }
        if let Some(model) = event.pointer("/message/model").and_then(Value::as_str)
            && model != self.output.model
        {
            self.output.response_model = Some(model.to_string());
        }
    }

    fn on_content_block_start(&mut self, event: &Value, cli_index: i64, structured: bool) {
        match event.pointer("/content_block/type").and_then(Value::as_str) {
            Some("text") if !structured => {
                self.output.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                let index = self.output.content.len() - 1;
                self.state.blocks.insert(cli_index, index);
                self.emit(|partial| AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial,
                });
            }
            Some("thinking") => {
                self.output.content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }));
                let index = self.output.content.len() - 1;
                self.state.blocks.insert(cli_index, index);
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial,
                });
            }
            _ => {}
        }
    }

    fn on_content_block_delta(&mut self, event: &Value, index: usize) {
        let delta = event.get("delta").cloned().unwrap_or(Value::Null);
        match (
            delta.get("type").and_then(Value::as_str),
            &mut self.output.content[index],
        ) {
            (Some("text_delta"), AssistantContent::Text(block)) => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    block.text.push_str(text);
                    let delta = text.to_string();
                    self.emit(|partial| AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta,
                        partial,
                    });
                }
            }
            (Some("thinking_delta"), AssistantContent::Thinking(block)) => {
                if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                    block.thinking.push_str(text);
                    let delta = text.to_string();
                    self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta,
                        partial,
                    });
                }
            }
            _ => {}
        }
    }

    fn on_stream_event(&mut self, event: &Value, structured: bool) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "message_start" {
            self.on_message_start(event);
            return;
        }
        let cli_index = event.get("index").and_then(Value::as_i64).unwrap_or(-1);
        if kind == "content_block_start" {
            self.on_content_block_start(event, cli_index, structured);
            return;
        }
        let Some(&index) = self.state.blocks.get(&cli_index) else {
            return;
        };
        if kind == "content_block_delta" {
            self.on_content_block_delta(event, index);
        } else if kind == "content_block_stop" {
            self.state.blocks.remove(&cli_index);
            emit_text_or_thinking_end(self, index);
        }
    }

    fn on_assistant(&mut self, event: &Value, structured: bool) {
        let Some(message) = event.get("message").filter(|m| m.is_object()) else {
            return;
        };
        if structured {
            return;
        }
        if let Some(id) = message.get("id").and_then(Value::as_str)
            && self.state.streamed.contains(id)
        {
            return;
        }
        if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
            return;
        }
        for part in message.get("content").and_then(Value::as_array).into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = part.get("text").and_then(Value::as_str).filter(|t| !t.is_empty())
            {
                self.add_text(text);
            }
        }
    }

    fn finish(&mut self, result: &Value, plan: &PlannedRequest) -> Option<Failure> {
        self.output.usage = usage_from_result(result);
        self.state.usage_known = true;
        if result.get("is_error") == Some(&json!(true)) {
            return match classify_failure(result, self.state.rate_limit.as_ref()) {
                Failure::Length => {
                    self.output.stop_reason = StopReason::Length;
                    self.output.raw_stop_reason = Some("max_output_tokens".into());
                    None
                }
                other => Some(other),
            };
        }
        if plan.structured {
            let text = match result.get("structured_output").filter(|v| !v.is_null()) {
                Some(value) => jsrt::json::stringify(value),
                None => result
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            self.add_text(&text);
        } else if !self
            .output
            .content
            .iter()
            .any(|p| matches!(p, AssistantContent::Text(_)))
            && let Some(text) = result.get("result").and_then(Value::as_str).filter(|t| !t.is_empty())
        {
            self.add_text(text);
        }
        let stop = result.get("stop_reason").and_then(Value::as_str);
        self.output.stop_reason = if stop == Some("max_tokens") {
            StopReason::Length
        } else {
            StopReason::Stop
        };
        if let Some(stop) = stop {
            self.output.raw_stop_reason = Some(stop.to_string());
        }
        self.output.end_turn = Some(stop == Some("end_turn"));
        None
    }

    fn on_rate_limit_event(&mut self, event: &Value) {
        if let Some(info) = rate_limit_info(event) {
            self.state.rate_limit = Some(info);
        }
        if let Some(info) = self.state.rate_limit.clone()
            && info.status.as_deref() == Some("allowed_warning")
        {
            self.warn(format!(
                "Claude Code subscription usage is near its {} limit{}",
                info.rate_limit_type.as_deref().unwrap_or_default(),
                reset_text(Some(&info))
            ));
        }
    }

    fn dispatch_child_event(&mut self, event: &Value, plan: &PlannedRequest) -> Option<Option<Failure>> {
        match (
            event.get("type").and_then(Value::as_str),
            event.get("subtype").and_then(Value::as_str),
        ) {
            (Some("system"), Some("init")) => {
                if let Some(source) = event
                    .get("apiKeySource")
                    .and_then(Value::as_str)
                    .filter(|s| *s != "none")
                {
                    self.warn(format!(
                        "the Claude Code CLI is using an API key (apiKeySource: {source}) instead of the subscription login; calls may be billed to that key"
                    ));
                }
            }
            (Some("stream_event"), _) => {
                if let Some(inner) = event.get("event").filter(|e| e.is_object()) {
                    self.on_stream_event(inner, plan.structured);
                }
            }
            (Some("assistant"), _) => self.on_assistant(event, plan.structured),
            (Some("rate_limit_event"), _) => self.on_rate_limit_event(event),
            (Some("result"), _) => return Some(self.finish(event, plan)),
            _ => {}
        }
        None
    }

    async fn drive(
        &mut self,
        child: &mut CliProcess,
        plan: &PlannedRequest,
        signal: Option<&crate::http::AbortSignal>,
        deadline: Instant,
        timeout_ms: u64,
    ) -> Result<Option<Failure>, RunError> {
        if crate::http::is_aborted(signal) {
            child.kill();
            return Err(RunError::Aborted(crate::provider::REQUEST_WAS_ABORTED.into()));
        }
        child.send(&plan.message);
        loop {
            let event = recv_child_event(child, signal, deadline, timeout_ms).await?;
            if let Some(result) = self.dispatch_child_event(&event, plan) {
                return Ok(result);
            }
        }
    }
}

async fn recv_child_event(
    child: &mut CliProcess,
    signal: Option<&crate::http::AbortSignal>,
    deadline: Instant,
    timeout_ms: u64,
) -> Result<Value, RunError> {
    let next = tokio::select! {
        event = child.events.recv() => event,
        () = crate::http::cancelled(signal) => {
            child.kill();
            return Err(RunError::Aborted(crate::provider::REQUEST_WAS_ABORTED.into()));
        }
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            child.kill();
            return Err(RunError::Failed {
                message: format!("Claude Code CLI request timed out after {}s", (timeout_ms as f64 / 1000.0).round()),
                usage_limit: false,
            });
        }
    };
    match next {
        Some(ChildEvent::Line(event)) => Ok(event),
        Some(ChildEvent::Exited(code, signal)) => Err(child_exit_error(child, code, signal)),
        None => Err(child_exit_error(child, None, None)),
    }
}

fn child_exit_error(child: &CliProcess, code: Option<i32>, signal: Option<i32>) -> RunError {
    if let Some(error) = &child.spawn_error {
        return RunError::Failed {
            message: format!("Claude Code CLI failed to start: {error}"),
            usage_limit: false,
        };
    }
    let stderr = jsrt::sync::lock(&child.stderr).clone();
    if let Some(start) = stderr.find("unknown option '") {
        let rest = &stderr[start + "unknown option '".len()..];
        if let Some(end) = rest.find('\'') {
            return RunError::Failed {
                message: format!(
                    "claude-code provider: the installed Claude Code CLI rejected {}. Update Claude Code (`claude update`).",
                    &rest[..end]
                ),
                usage_limit: false,
            };
        }
    }
    let how = match (signal, code) {
        (Some(signal), _) => jsrt::process::signal_name(signal),
        (None, code) => format!("code {}", code.map_or("null".into(), |c| c.to_string())),
    };
    let trimmed = stderr.trim();
    RunError::Failed {
        message: format!(
            "Claude Code CLI exited ({how}) before a result{}",
            if trimmed.is_empty() {
                String::new()
            } else {
                format!(": {}", tail(trimmed, 1_000))
            }
        ),
        usage_limit: false,
    }
}

/// One request against the CLI as an assistant-message stream.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let mut output = AssistantMessage::pending(model, jsrt::date::now_ms());
    output.stop_reason = StopReason::Stop;
    let state = CliStream {
        blocks: HashMap::new(),
        streamed: HashSet::new(),
        rate_limit: None,
        usage_known: false,
        warnings: Vec::new(),
    };
    let mut run = Run::new(output, sink, state);
    let mut spawned = false;
    let result = drive(&mut run, model, context, options, &mut spawned).await;
    match result {
        Ok(()) => {
            if !run.state.warnings.is_empty() {
                run.output.diagnostics = Some(
                    run.state
                        .warnings
                        .iter()
                        .map(|w| diagnostic("claude_code_warning", w))
                        .collect(),
                );
            }
            let reason = if run.output.stop_reason == StopReason::Length {
                StopReason::Length
            } else {
                StopReason::Stop
            };
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(error) => {
            let aborted = crate::http::is_aborted(options.base.base.signal());
            let (message, usage_limit, aborted) = match error {
                RunError::Aborted(message) => (message, false, true),
                RunError::Failed { message, usage_limit } => (message, usage_limit, aborted),
            };
            run.output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            run.output.error_message = Some(message.clone());
            let mut diagnostics: Vec<AssistantMessageDiagnostic> = run
                .state
                .warnings
                .iter()
                .map(|w| diagnostic("claude_code_warning", w))
                .collect();
            if usage_limit {
                diagnostics.push(diagnostic(USAGE_LIMIT_DIAGNOSTIC, &message));
            }
            if !diagnostics.is_empty() {
                run.output.diagnostics = Some(diagnostics);
            }
            if spawned && !run.state.usage_known {
                run.output.usage.cost.total = f64::NAN;
            }
            let reason = run.output.stop_reason;
            run.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    run.output
}

fn failed(message: String) -> RunError {
    RunError::Failed {
        message,
        usage_limit: false,
    }
}

fn validate_no_tools(model: &Model, context: &TranscriptContext) -> Result<(), RunError> {
    let tools: Vec<String> = current_tools(&context.messages).into_iter().map(|t| t.name).collect();
    if tools.is_empty() {
        return Ok(());
    }
    let listed = tools.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
    let more = if tools.len() > 8 { ", ..." } else { "" };
    Err(failed(format!(
        "{}/{} {TOOLS_UNSUPPORTED} (this lane declares tools: {listed}{more}).",
        model.provider, model.id
    )))
}

struct DriveSetup {
    env: Env,
    config: RuntimeConfig,
    bin: String,
    cwd: PathBuf,
    capabilities: ClaudeCliCapabilities,
    request: Value,
}

async fn prepare_drive_setup(
    run: &mut Run<'_, '_, CliStream>,
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
) -> Result<DriveSetup, RunError> {
    let base = &options.base.base;
    let signal = base.signal();
    if crate::http::is_aborted(signal) {
        return Err(RunError::Aborted(crate::provider::OPERATION_ABORTED.into()));
    }
    validate_no_tools(model, context)?;
    let env = child_env(base);
    let config = runtime_config(&env, base);
    let bin = resolve_claude_cli(&env).map_err(failed)?;
    let cwd = empty_work_dir().map_err(failed)?;
    let capabilities = ensure_claude_cli(&bin, &env, &cwd).await.map_err(failed)?;
    for warning in &capabilities.warnings {
        run.warn(warning.clone());
    }
    if crate::http::is_aborted(signal) {
        return Err(RunError::Aborted(crate::provider::OPERATION_ABORTED.into()));
    }
    let mut request = payload(model, context, options);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&request, model)) {
        request = next;
    }
    Ok(DriveSetup {
        env,
        config,
        bin,
        cwd,
        capabilities,
        request,
    })
}

fn remember_proven_key(
    setup: &DriveSetup,
    plan: &PlannedRequest,
    key: &str,
    signal: Option<&crate::http::AbortSignal>,
    session_id: Option<&str>,
) {
    if crate::http::is_aborted(signal) {
        return;
    }
    jsrt::sync::lock(&PROVEN_KEYS).insert(key.to_string());
    if plan._system_prompt_file.is_none() && session_id.is_none() {
        start_spare(key, &setup.bin, &plan.args, &plan.env, &setup.cwd, setup.config);
    }
}

fn spawn_or_reuse_child(setup: &DriveSetup, plan: &PlannedRequest, key: &str, spawned: &mut bool) -> CliProcess {
    let child = take_spare(key).unwrap_or_else(|| CliProcess::spawn(&setup.bin, &plan.args, &plan.env, &setup.cwd));
    *spawned = true;
    if jsrt::sync::lock(&PROVEN_KEYS).contains(key) {
        start_spare(key, &setup.bin, &plan.args, &plan.env, &setup.cwd, setup.config);
    }
    child
}

enum AttemptOutcome {
    Done,
    RetryWithoutSchema,
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal helper running one Claude CLI request attempt"
)]
async fn run_cli_attempt(
    run: &mut Run<'_, '_, CliStream>,
    setup: &DriveSetup,
    model: &Model,
    options: &ClaudeCodeOptions,
    use_schema_flag: bool,
    deadline: Instant,
    spawned: &mut bool,
) -> Result<AttemptOutcome, RunError> {
    let signal = options.base.base.signal();
    let session_id = options.base.base.session_id.as_deref();
    let plan = plan_request(
        model,
        &setup.request,
        &setup.env,
        model.max_tokens,
        &setup.capabilities,
        use_schema_flag,
        session_id,
    )
    .map_err(failed)?;
    let key = jsrt::json::stringify(&json!([setup.bin, setup.cwd.to_string_lossy(), plan.args, plan.env]));
    let mut child = spawn_or_reuse_child(setup, &plan, &key, spawned);
    let attempt = run
        .drive(&mut child, &plan, signal, deadline, setup.config.timeout_ms)
        .await;
    child.finish();
    match attempt? {
        Some(Failure::SchemaRejected(message)) if use_schema_flag => {
            run.warn(format!(
                "--json-schema was rejected ({message}); asked for JSON in the prompt instead"
            ));
            Ok(AttemptOutcome::RetryWithoutSchema)
        }
        Some(Failure::SchemaRejected(message)) => Err(failed(message)),
        Some(Failure::Error { message, usage_limit }) => Err(RunError::Failed { message, usage_limit }),
        Some(Failure::Length) | None => {
            remember_proven_key(setup, &plan, &key, signal, session_id);
            Ok(AttemptOutcome::Done)
        }
    }
}

async fn drive(
    run: &mut Run<'_, '_, CliStream>,
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    spawned: &mut bool,
) -> Result<(), RunError> {
    let setup = prepare_drive_setup(run, model, context, options).await?;
    let _permit = acquire(setup.config.concurrency, options.base.base.signal())
        .await
        .map_err(RunError::Aborted)?;
    let deadline = Instant::now() + Duration::from_millis(setup.config.timeout_ms);
    run.start();
    let mut use_schema_flag = true;
    loop {
        match run_cli_attempt(run, &setup, model, options, use_schema_flag, deadline, spawned).await? {
            AttemptOutcome::Done => return Ok(()),
            AttemptOutcome::RetryWithoutSchema => use_schema_flag = false,
        }
    }
}

/// `stream`: the same as [`stream_simple`].
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    stream_simple(model, context, options, sink).await
}
