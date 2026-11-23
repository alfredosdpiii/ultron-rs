use super::{Adapter, AdapterRequest, json_from};
use jsrt::{json, string};
use kernel::Signal;
use serde_json::json;
use std::sync::Arc;

/// One predict model call: the system and user prompts of `createPredictAdapter`.
pub struct PredictCall<'a> {
    pub system: &'a str,
    pub user: String,
    pub signal: &'a Signal,
}

/// A finished model call: its text parts joined, its `stopReason` and `errorMessage`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    pub text: String,
    pub stop_reason: String,
    pub error_message: Option<String>,
}

/// One model completion (`completeSimple`); an `Err` is a call that could not be made.
pub type Complete = Arc<dyn Fn(PredictCall<'_>) -> Result<Completion, String> + Send + Sync>;

pub const PREDICT_SYSTEM_PROMPT: &str = "You are a typed function. Reply with a single JSON value that satisfies the output schema. No prose, no code fences.";

/// `createPredictAdapter`: one model call per attempt with no tools and no kernel; the reply is parsed as JSON (a
/// fenced block unwrapped) and returned as text when it is not JSON, so the host's validation drives the repair.
pub fn predict_adapter(complete: Complete) -> Adapter {
    Arc::new(move |request: AdapterRequest<'_>| {
        let definition = request.definition;
        let repair = match &request.repair {
            None => String::new(),
            Some(repair) => format!(
                "\n\nYour previous answer was rejected (attempt {}): {}\nPrevious answer: {}",
                json::stringify(&repair["attempt"]),
                repair["error"].as_str().unwrap_or_default(),
                json::stringify(&repair["previous"])
            ),
        };
        let user = format!(
            "{}\n\nInput:\n{}\n\nOutput schema:\n{}\n\nOutput contract:\n{}{repair}",
            definition["instructions"].as_str().unwrap_or_default(),
            json::stringify(request.input),
            json::stringify(&definition["outputSchema"]),
            definition["outputDescription"].as_str().unwrap_or_default(),
        );
        let reply = complete(PredictCall {
            system: PREDICT_SYSTEM_PROMPT,
            user,
            signal: request.signal,
        })?;
        if reply.stop_reason == "error" || reply.stop_reason == "aborted" {
            return Err(reply
                .error_message
                .unwrap_or_else(|| format!("Predict call {}", reply.stop_reason)));
        }
        let text = reply.text;
        Ok(json_from(&text).unwrap_or_else(|_| {
            let trimmed = string::trim(&text);
            json!(string::strip_json_fence(trimmed))
        }))
    })
}
