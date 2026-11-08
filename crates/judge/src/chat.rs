//! The chat rung: a configured model answers through an `answer` tool whose parameters are the closed answer set
//! (providers that sample against a tool's JSON schema keep it inside the set; every answer is checked anyway). It
//! is the last fallback (`judge.fallbackModel`) and the escalation (`judge.escalateModel`).

use crate::backend::{Backend, Failure, Reply, Spend};
use crate::question::{self, Question};
use ai::{AssistantContent, AssistantMessage, Context, Model, StopReason, Tool, TranscriptContext};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How long a chat-rung request may take.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(60);
const ANSWER_TOOL: &str = "answer";
const SYSTEM: &str = "You are a judge. Read the state and answer every question by calling the `answer` tool once. \
Choose only from the answers each question allows. `confidence` is your probability, from 0 to 1, that the answer is \
right; say so when the state does not settle it.";

/// One request to a `provider/model`, no retries; `Err` when it could not be sent.
pub trait ChatModel: Send + Sync {
    fn complete(
        &self,
        model_ref: &str,
        context: &TranscriptContext,
        signal: &Signal,
    ) -> Result<AssistantMessage, String>;
}

/// The model of a `provider/model` reference, ready for requests.
pub type ModelSource = Arc<dyn Fn(&str, &str) -> Option<Model> + Send + Sync>;
/// A provider's API key, read per request.
pub type ProviderKeys = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// [`ChatModel`] over `ai::dispatch::stream_simple`: provider retries off, the answer capped.
pub struct DispatchChat {
    pub models: ModelSource,
    pub keys: ProviderKeys,
    pub transport: Arc<dyn ai::Transport>,
}

impl ChatModel for DispatchChat {
    fn complete(
        &self,
        model_ref: &str,
        context: &TranscriptContext,
        signal: &Signal,
    ) -> Result<AssistantMessage, String> {
        let (provider, id) = model_ref
            .split_once('/')
            .ok_or_else(|| format!("{model_ref} is not provider/model"))?;
        let model = (self.models)(provider, id).ok_or_else(|| format!("Model \"{model_ref}\" not found"))?;
        let abort = ai::AbortSignal::new();
        let _link = signal.link({
            let abort = abort.clone();
            move |_| abort.abort()
        });
        let mut options = ai::SimpleStreamOptions::default();
        options.base.api_key = (self.keys)(provider);
        options.base.max_retries = Some(0);
        options.base.max_tokens = Some(2048);
        options.base.timeout_ms = Some(CHAT_TIMEOUT.as_millis() as u64);
        options.base.signal = Some(abort);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        runtime.block_on(ai::dispatch::stream_simple(
            &model,
            context,
            &options,
            &*self.transport,
            &mut sink,
        ))
    }
}

/// The chat rung on one model.
pub struct Chat {
    pub model: String,
    pub chat: Arc<dyn ChatModel>,
}

fn context(state: &str, questions: &[Question]) -> Result<TranscriptContext, Failure> {
    let user = serde_json::from_value(json!({
        "role": "user",
        "content": question::chat_prompt(state, questions),
        "timestamp": 0,
    }))
    .map_err(|e| Failure::Failed(e.to_string()))?;
    Ok(ai::transcript::normalize_context(&Context {
        system_prompt: Some(SYSTEM.into()),
        messages: vec![user],
        tools: Some(vec![Tool {
            name: ANSWER_TOOL.into(),
            description: "Your answers, one per question.".into(),
            parameters: question::chat_schema(questions),
            constrained_sampling: None,
        }]),
    }))
}

/// The `answer` call's arguments, else a JSON object in the text.
fn arguments(message: &AssistantMessage) -> Option<Value> {
    let call = message.content.iter().find_map(|block| match block {
        AssistantContent::ToolCall(call) if call.name == ANSWER_TOOL => Some(call.arguments.clone()),
        _ => None,
    });
    call.or_else(|| {
        message.content.iter().find_map(|block| match block {
            AssistantContent::Text(text) => {
                let text = text.text.trim();
                let start = text.find('{')?;
                serde_json::from_str(&text[start..=text.rfind('}')?]).ok()
            }
            _ => None,
        })
    })
}

impl Backend for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn paid(&self) -> bool {
        true
    }

    fn ready(&self) -> Result<(), String> {
        Ok(())
    }

    fn call(&self, state: &str, questions: &[Question], signal: &Signal) -> Result<Reply, Failure> {
        let context = context(state, questions)?;
        let message = self
            .chat
            .complete(&self.model, &context, signal)
            .map_err(Failure::Unavailable)?;
        if signal.is_aborted() || message.stop_reason == StopReason::Aborted {
            return Err(Failure::Aborted);
        }
        if message.stop_reason == StopReason::Error {
            return Err(Failure::Failed(
                message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "the model failed".into()),
            ));
        }
        let answers = arguments(&message)
            .ok_or_else(|| "no answer call".to_string())
            .and_then(|args| question::chat_answers(questions, &args))
            .map_err(Failure::Failed)?;
        Ok(Reply {
            answers,
            model: format!("{}/{}", message.provider, message.model),
            spend: Spend {
                input_tokens: Some(message.usage.input + message.usage.cache_read + message.usage.cache_write),
                output_tokens: Some(message.usage.output),
                cost: Some(message.usage.cost.total),
            },
        })
    }
}

/// A model's family: Claude for every Claude route (`anthropic`, `claude-code`, `claude-subscription`, a `claude-*`
/// id), else the id's first word (`gpt`, `gemini`), else the provider; `jev` and `d1` are their own.
pub fn family(model: &str) -> String {
    let (provider, id) = model.split_once('/').unwrap_or(("", model));
    let id = id.rsplit('/').next().unwrap_or(id);
    let word = id.split(['-', '.', ':']).next().unwrap_or(id).to_ascii_lowercase();
    if word.starts_with("claude") || matches!(provider, "anthropic" | "claude-code" | "claude-subscription") {
        return "claude".into();
    }
    if word.is_empty() { provider.to_string() } else { word }
}

/// The cheapest priced model of `family` among `providers`' catalogs (ties go to the later, newer entry).
fn cheapest<'a>(family: &str, providers: impl Iterator<Item = &'a str>) -> Option<&'static str> {
    let mut cheapest: Option<(&'static str, f64)> = None;
    for provider in providers {
        for model in ai::models::builtin_models(provider) {
            let price = model.cost.input;
            if model.id.contains('/') || price <= 0.0 || family_of_id(&model.id) != family {
                continue;
            }
            if cheapest.is_none_or(|(_, best)| price <= best) {
                cheapest = Some((&model.id, price));
            }
        }
    }
    cheapest.map(|(id, _)| id)
}

/// `judge.escalateModel`'s default: the cheapest model of the session model's family on the session's provider, by
/// the provider's catalog prices; for a provider without prices (a subscription), the family's cheapest model by
/// any provider's prices, when the session's provider serves it.
pub fn default_escalate_model(session_model: &str) -> Option<String> {
    let (provider, _) = session_model.split_once('/')?;
    let family = family(session_model);
    let id = match cheapest(&family, std::iter::once(provider)) {
        Some(id) => id,
        None => {
            let id = cheapest(&family, ai::models::builtin_providers())?;
            let served =
                provider == ai::claude_subscription::PROVIDER || ai::models::builtin_model(provider, id).is_some();
            served.then_some(id)?
        }
    };
    Some(format!("{provider}/{id}"))
}

fn family_of_id(id: &str) -> String {
    family(&format!("/{id}"))
}
