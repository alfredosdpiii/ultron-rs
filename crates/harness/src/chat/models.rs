//! What generation needs from the provider layer, and the settings it reads.
//!
//! [`Models`] is upstream `HarnessOptions.models` reduced to what the generation and compaction tasks call:
//! resolve a model, stream one response under the invocation's signal, and poll or cancel a deferred response. It
//! blocks the calling task thread. The product implementation is `session_core::turns::TurnModels`; tests use
//! `faux::Faux` (feature `test-support`).

use std::sync::{Arc, LazyLock};

use ai::{AssistantMessage, AssistantMessageEvent, DeferredHandle, Model, StopReason, TranscriptContext};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::inbox::{QueueMode, QueueModes};
use crate::scheduler::{Cancel, HarnessError, Result};

/// A model by provider and ID, as `ultron.agent` and generation checkpoints pin it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider: &str, model_id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        }
    }
}

/// One request's options: the pinned stream options (`ConversationStreamOptions` as JSON, e.g. `maxTokens`,
/// `temperature`), the provider session ID, and the reasoning level (`None` for thinking off).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamRequest {
    pub options: Map<String, Value>,
    pub session_id: Option<String>,
    pub reasoning: Option<String>,
    /// The conversation the request is for (e.g. to charge its usage to the work that owns it).
    pub conversation_id: Option<durable::ConversationId>,
    /// Set on a run's final request ([`super::gate::RequestGate::Final`]): the note its context ends with. The
    /// request's options carry tool choice `none`; [`super::gate::apply_final`] applies both to what is sent.
    pub final_note: Option<String>,
}

impl StreamRequest {
    /// The request as `ai::SimpleStreamOptions` (the signal is set by the caller).
    pub fn simple_options(&self) -> ai::SimpleStreamOptions {
        let mut options: ai::SimpleStreamOptions =
            serde_json::from_value(Value::Object(self.options.clone())).unwrap_or_default();
        options.base.session_id = self.session_id.clone();
        options.reasoning = self.reasoning.clone();
        options
    }
}

/// The provider layer as generation uses it. Every call blocks; `signal` is the task invocation's.
pub trait Models: Send + Sync {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model>;

    /// Stream one response. Events go to `on_event` as they arrive; provider failures are messages with stop reason
    /// `error`; `Err` is a transport bug. An aborted signal ends the stream with an `aborted` message or `Err`.
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> Result<AssistantMessage>;

    /// Poll a deferred response; a still deferred result keeps stop reason `deferred`.
    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<AssistantMessage> {
        let _ = (model, handle, signal);
        Err(HarnessError::msg("This provider has no deferred responses"))
    }

    fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<()> {
        let _ = (model, handle, signal);
        Ok(())
    }
}

/// `settings.compaction` (spec §8.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionPolicy {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub background_tokens: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        CompactionPolicy {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            background_tokens: 32_768,
        }
    }
}

/// Whether a round's cells run at once or one after another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolExecution {
    #[default]
    Parallel,
    Sequential,
}

/// Harness settings, read at each decision so changes apply to the next one.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Request options pinned at preparation (`ConversationStreamOptions`).
    pub stream: Map<String, Value>,
    pub retry: ai::retry::RetryPolicy,
    pub compaction: CompactionPolicy,
    /// Throttle of committed generation partials.
    pub partial_interval_ms: u64,
    /// Throttle of committed cell output.
    pub output_interval_ms: u64,
    pub tool_execution: ToolExecution,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    /// A final boundary that places steering messages leaves follow-ups for the next one (the TypeScript agent loop:
    /// steering continues the run, follow-ups are polled only when it would stop), so a steer and a follow-up
    /// queued at a final answer run as two turns. Off: the spec's boundary places both at once.
    pub follow_up_after_steer: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            stream: Map::new(),
            retry: ai::retry::RetryPolicy::default(),
            compaction: CompactionPolicy::default(),
            partial_interval_ms: 100,
            output_interval_ms: 100,
            tool_execution: ToolExecution::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            follow_up_after_steer: false,
        }
    }
}

impl Settings {
    pub(crate) fn queue_modes(&self) -> QueueModes {
        QueueModes {
            steering: self.steering_mode,
            follow_up: self.follow_up_mode,
            follow_up_after_steer: self.follow_up_after_steer,
        }
    }
}

/// Where settings are read from: a fixed value or a live source.
pub type SettingsSource = Arc<dyn Fn() -> Settings + Send + Sync>;

fn patterns(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .map(|source| Regex::new(&format!("(?i){source}")).expect("valid pattern"))
        .collect()
}

static OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"prompt (?:is )?too long",
        r"prompt exceeds max length",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum",
        r"maximum prompt length is \d+",
        r"reduce the length of the messages",
        r"maximum context length is \d+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"exceeds the limit of \d+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length",
        r"range of input length should be",
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ])
});

static NON_OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"^(Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ])
});

static CEREBRAS_BODYLESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("valid pattern"));

/// pi-ai `isContextOverflow` without a context window: an error message a provider uses for an oversized request.
pub fn is_context_overflow(message: &AssistantMessage) -> bool {
    let Some(text) = message
        .error_message
        .as_deref()
        .filter(|_| message.stop_reason == StopReason::Error)
    else {
        return false;
    };
    if NON_OVERFLOW.iter().any(|pattern| pattern.is_match(text)) {
        return false;
    }
    OVERFLOW.iter().any(|pattern| pattern.is_match(text))
        || (message.provider == "cerebras" && CEREBRAS_BODYLESS.is_match(text))
}
