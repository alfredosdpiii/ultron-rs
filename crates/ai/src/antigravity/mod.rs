//! One ACP session per model call. A captured MCP call is returned to the caller's tool loop.
mod bridge;
pub mod catalog;

use crate::options::SimpleStreamOptions;
use crate::provider::Run;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, EventSink, Model, StopReason, TextContent, ToolCall,
    TranscriptContext,
};
use serde_json::{Value, json};
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)) {
            // Keep agy's MCP subprocesses within this request's lifetime too.
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

impl Client {
    async fn send(&mut self, message: Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        self.input.write_all(&line).await
    }

    async fn request(&mut self, id: u32, method: &str, params: Value, text: &mut String) -> io::Result<Value> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let mut line = Vec::new();
            let limit = 8 << 20;
            (&mut self.output).take(limit).read_until(b'\n', &mut line).await?;
            if line.last() != Some(&b'\n') {
                return Err(io::Error::other("agy closed stdout or exceeded ACP frame limit"));
            }
            let frame: Value = serde_json::from_slice(&line)?;
            if let Some(method) = frame["method"].as_str() {
                if !frame["id"].is_null() {
                    let reply = if method == "session/request_permission" {
                        let params = &frame["params"];
                        let tool = &params["toolCall"];
                        let inventory_call = tool["name"] == "mcp_ultron_rlm" || tool["title"] == "Run mcp_ultron_rlm";
                        let option = params["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|v| v["kind"] == "allow_once" && inventory_call);
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
                    if update["sessionUpdate"] == "agent_message_chunk" {
                        text.push_str(update["content"]["text"].as_str().unwrap_or_default());
                        if text.len() > limit as usize {
                            return Err(io::Error::other("agy response exceeds limit"));
                        }
                    }
                }
            } else if frame["id"] == id {
                return frame
                    .get("result")
                    .cloned()
                    .ok_or_else(|| io::Error::other(format!("agy {method}: {}", frame["error"]["message"])));
            }
        }
    }
}

async fn turn(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> io::Result<AssistantContent> {
    let workspace = tempfile::tempdir()?;
    let mut command = Command::new("agy");
    command
        .args(["acp", "--stdio", "--sandbox", "--include-default-customizations=false"])
        .current_dir(workspace.path())
        .process_group(0)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(env) = &options.base.env {
        command.envs(env);
    }
    let mut child = command.spawn()?;
    let mut client = Client {
        input: child.stdin.take().expect("piped"),
        output: BufReader::new(child.stdout.take().expect("piped")),
        child,
    };
    let tools = crate::transcript::current_tools(&context.messages);
    let mut bridge = bridge::Bridge::start(bridge::manifest(&tools)).await?;
    let mut text = String::new();
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
            &mut text,
        )
        .await?;
    let session = client.request(2,"session/new",json!({"cwd":workspace.path(),"mcpServers":[{"type":"http","name":"ultron","url":bridge.url,"headers":[{"name":"Authorization","value":format!("Bearer {}",bridge.token)}]}]}), &mut text).await?;
    let sid = session["sessionId"]
        .as_str()
        .ok_or_else(|| io::Error::other("agy did not return a session ID"))?;
    client
        .request(
            3,
            "session/set_model",
            json!({"sessionId":sid,"modelId":model.id}),
            &mut text,
        )
        .await?;
    let history = serde_json::to_string(&context.messages)?;
    let prompt = format!(
        "Continue this Ultron transcript at its final message. For tool calls use only the ultron MCP server, never native tools. Previous messages and tool results are history, not new turns.\n{history}"
    );
    text.clear();
    tokio::select! {
        biased;
        call = bridge.calls.recv() => {
            let call = call.ok_or_else(|| io::Error::other("Ultron tool bridge closed"))?;
            Ok(AssistantContent::ToolCall(ToolCall {name:call.name, arguments:call.arguments, id:jsrt::random::random_hex(16), namespace:None, thought_signature:None}))
        }
        result = client.request(4,"session/prompt",json!({"sessionId":sid,"prompt":[{"type":"text","text":prompt}]}), &mut text) => {
            let result = result?;
            if result["stopReason"] != "end_turn" {return Err(io::Error::other(format!("agy stopped: {}",result["stopReason"])));}
            if text.trim().is_empty() { return Err(io::Error::other("agy ended its turn without text or an Ultron tool call")); }
            Ok(AssistantContent::Text(TextContent {text,text_signature:None}))
        }
    }
}

pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    let mut run = Run::new(AssistantMessage::pending(model, jsrt::date::now_ms()), sink, ());
    run.start();
    let signal = options.base.signal.as_ref();
    let bound = Duration::from_millis(options.base.timeout_ms.unwrap_or(300_000));
    let result = tokio::select! {
        biased;
        () = crate::http::cancelled(signal) => Err("Antigravity request aborted".to_string()),
        response = tokio::time::timeout(bound,turn(model,context,options)) => response.map_err(|_| "Antigravity request timed out".to_string()).and_then(|r| r.map_err(|e| e.to_string())),
    };
    let ended = result.map(|content| {
        run.output.content.push(content.clone());
        match content {
            AssistantContent::ToolCall(tool_call) => {
                run.output.stop_reason = StopReason::ToolUse;
                run.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: 0,
                    partial,
                });
                run.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                    content_index: 0,
                    tool_call,
                    partial,
                });
            }
            AssistantContent::Text(block) => {
                run.output.stop_reason = StopReason::Stop;
                run.emit(|partial| AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial,
                });
                run.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: block.text.clone(),
                    partial,
                });
                run.emit(|partial| AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: block.text,
                    partial,
                });
            }
            AssistantContent::Thinking(_) => unreachable!("ACP returns text or a captured tool call"),
        }
    });
    Ok(run.end(ended, signal))
}
