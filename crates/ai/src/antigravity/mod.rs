//! Ultron's tool loop over `agy acp`. By default (`sessions`) agy stays alive across model calls: a held MCP call is
//! answered with the next tool result and the same turn continues. With `ULTRON_AGY_SESSIONS=0`, or when agy's config
//! cannot take Ultron's relay entry, each model call starts agy, replays the transcript as one prompt and returns the
//! first captured MCP call.
mod bridge;
pub mod catalog;
mod client;
mod prompt;
mod sessions;
#[cfg(test)]
mod tests;

pub use sessions::ensure_relay;

/// Environment variable carrying the JSON argv that `ultron-mcp-relay.py` execs for `ultron agy`.
pub const MCP_EXEC_ENV: &str = "ULTRON_AGY_MCP_EXEC";

use crate::options::SimpleStreamOptions;
use crate::provider::Run;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, EventSink, Model, StopReason, TextContent,
    ThinkingContent, TranscriptContext, Usage,
};
use client::{Agy, AgyDelta, Step, WorkspaceDir, turn};
use prompt::{delta_prompt, normalize_mime_type};
use std::time::Duration;
use tokio::sync::mpsc;

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

    fn close_streamed_blocks(&mut self) {
        if let Some(index) = self.state.thought_index
            && let Some(AssistantContent::Thinking(block)) = self.output.content.get(index).cloned()
        {
            self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: block.thinking,
                partial,
            });
        }
        if let Some(index) = self.state.text_index
            && let Some(AssistantContent::Text(block)) = self.output.content.get(index).cloned()
        {
            self.emit(|partial| AssistantMessageEvent::TextEnd {
                content_index: index,
                content: block.text,
                partial,
            });
        }
    }

    fn emit_final_block(
        &mut self,
        block: AssistantContent,
        streamed_text: Option<usize>,
        streamed_thought: Option<usize>,
        has_tool: &mut bool,
    ) {
        match block {
            AssistantContent::ToolCall(tool_call) => {
                *has_tool = true;
                let index = self.output.content.len();
                self.output.content.push(AssistantContent::ToolCall(tool_call.clone()));
                self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call,
                    partial,
                });
            }
            AssistantContent::Text(block) if streamed_text.is_none() => {
                let index = self.output.content.len();
                self.output.content.push(AssistantContent::Text(block.clone()));
                self.emit(|partial| AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: block.text.clone(),
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: block.text,
                    partial,
                });
            }
            AssistantContent::Thinking(block) if streamed_thought.is_none() => {
                let index = self.output.content.len();
                self.output.content.push(AssistantContent::Thinking(block.clone()));
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: block.thinking.clone(),
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: block.thinking,
                    partial,
                });
            }
            _ => {}
        }
    }

    fn finish(&mut self, content: Vec<AssistantContent>, usage: Usage) {
        self.output.usage = usage;
        let streamed_thought = self.state.thought_index;
        let streamed_text = self.state.text_index;
        self.close_streamed_blocks();
        let mut has_tool = false;
        for block in content {
            self.emit_final_block(block, streamed_text, streamed_thought, &mut has_tool);
        }
        self.output.stop_reason = if has_tool {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
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
    let ended = result.map(|(content, usage)| run.finish(content, usage));
    Ok(run.end(ended, signal))
}
