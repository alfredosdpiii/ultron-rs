//! Session naming by one small model call. The deterministic name from the first prompt
//! (`session_journal::names::auto_name`) is written at once; this asks a cheap model for a better title, which
//! replaces it only while the name is still the automatic one. Settings: `sessionNaming.enabled` (default true)
//! and `sessionNaming.model` (`provider/id`, or an id of the session's provider). The model defaults to the
//! session provider's cheapest catalog model, else the session model.

use ai::{
    AbortSignal, AssistantContent, AssistantMessage, Message, Model, SimpleStreamOptions, StopReason, StreamOptions,
    TextContent, TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use serde_json::Value;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

pub const SYSTEM: &str = "Name this coding session in 3-6 words. Reply with the title only.";
/// How much of the first prompt the title request carries, in characters.
pub const PROMPT_CHARS: usize = 2_000;
pub const MAX_TOKENS: u64 = 24;
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded wait at session close so an in-flight naming reply that trails a fast turn is not dropped, while a slow
/// naming server never stalls exit.
pub const CLOSE_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamingSettings {
    pub enabled: bool,
    pub model: Option<String>,
}

/// `sessionNaming` of the merged settings.
pub fn naming_settings(settings: &Value) -> NamingSettings {
    let section = &settings["sessionNaming"];
    NamingSettings {
        enabled: section["enabled"].as_bool() != Some(false),
        model: section["model"]
            .as_str()
            .map(str::to_string)
            .filter(|m| !m.trim().is_empty()),
    }
}

fn price(model: &Model) -> f64 {
    model.cost.input + model.cost.output
}

/// The naming model: the setting, else the provider's cheapest priced text model whose API this build streams
/// (non-reasoning models first; never one that always thinks), else `session`.
pub fn naming_model(setting: Option<&str>, session: &Model, supported: &dyn Fn(&str) -> bool) -> Model {
    if let Some(spec) = setting {
        let (provider, id) = spec.split_once('/').unwrap_or((session.provider.as_str(), spec));
        if let Some(model) = ai::models::builtin_model(provider, id) {
            let mut model = model.clone();
            if model.provider == session.provider {
                model.base_url = session.base_url.clone();
            }
            return model;
        }
    }
    let cheapest = ai::models::builtin_models(&session.provider)
        .filter(|m| supported(&m.api) && m.input.iter().any(|i| i == "text") && price(m) > 0.0)
        .filter(|m| {
            m.compat.as_ref().and_then(|c| c.get("supportsMidConvoEffort")) != Some(&serde_json::Value::Bool(true))
        })
        .min_by(|l, r| {
            l.reasoning
                .cmp(&r.reasoning)
                .then(price(l).total_cmp(&price(r)))
                .then_with(|| l.id.cmp(&r.id))
        });
    match cheapest {
        Some(model) => {
            let mut model = model.clone();
            model.base_url = session.base_url.clone();
            model
        }
        None => session.clone(),
    }
}

/// The request: the system line and the first prompt, cut to [`PROMPT_CHARS`].
pub fn request(prompt: &str) -> TranscriptContext {
    let text: String = prompt.chars().take(PROMPT_CHARS).collect();
    let mut messages = Vec::new();
    if let Some(system) = ai::transcript::create_initial_system_message(Some(SYSTEM), None) {
        messages.push(Message::System(system));
    }
    messages.push(Message::User(UserMessage {
        content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent {
            text,
            text_signature: None,
        })]),
        timestamp: 0,
    }));
    TranscriptContext { messages }
}

pub fn options(api_key: Option<String>) -> SimpleStreamOptions {
    options_with_signal(api_key, None)
}

pub fn options_with_signal(api_key: Option<String>, signal: Option<AbortSignal>) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            api_key,
            signal,
            max_tokens: Some(MAX_TOKENS),
            max_retries: Some(0),
            ..Default::default()
        },
        reasoning: None,
        ..Default::default()
    }
}

/// An in-flight background naming request tracked by the session actor so normal shutdown can wait briefly for a
/// trailing reply without stalling on a slow server.
pub struct Pending {
    signal: AbortSignal,
    done: Receiver<()>,
}

impl Pending {
    pub fn spawn(
        provider: Arc<crate::lane::Provider>,
        model: Model,
        key: Option<String>,
        text: String,
        tx: Sender<crate::core::Input>,
    ) -> Pending {
        let signal = AbortSignal::new();
        let (done_tx, done) = channel();
        let abort = signal.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                let request = request(&text);
                let options = options_with_signal(key, Some(abort));
                let mut sink = |_: &ai::AssistantMessageEvent| {};
                let call = <crate::lane::Provider as agent::StreamFn>::stream(
                    &provider, &model, &request, &options, &mut sink,
                );
                runtime
                    .block_on(async { tokio::time::timeout(TIMEOUT, call).await })
                    .ok()
            })();
            if let Some(title) = result {
                let _ = tx.send(crate::core::Input::Title(Box::new(title)));
            }
            let _ = done_tx.send(());
        });
        Pending { signal, done }
    }

    /// Wait up to [`CLOSE_WAIT`] for the naming thread to finish and enqueue `Input::Title`; abort it on timeout.
    pub fn wait_on_close(self) -> bool {
        match self.done.recv_timeout(CLOSE_WAIT) {
            Ok(()) => true,
            Err(_) => {
                self.signal.abort();
                false
            }
        }
    }
}

pub fn title_of(message: &AssistantMessage) -> Option<String> {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        return None;
    }
    let text: String = message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    session_journal::names::clean_title(&text)
}
