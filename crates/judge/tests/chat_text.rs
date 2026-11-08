//! The chat rung on a model that returns only text (`claude-code`): no `answer` tool, the schema in the prompt, and
//! the JSON object in the reply read as the answers.

use ai::{AssistantMessage, Message, TranscriptContext};
use judge::question::parse_questions;
use judge::{Backend, Chat, ChatModel};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// A text-only model that records the request and replies with `text`.
struct TextOnly {
    text: String,
    asked: Mutex<Option<TranscriptContext>>,
}

impl ChatModel for TextOnly {
    fn complete(
        &self,
        model_ref: &str,
        context: &TranscriptContext,
        _signal: &Signal,
    ) -> Result<AssistantMessage, String> {
        *self.asked.lock().unwrap() = Some(context.clone());
        let (provider, model) = model_ref.split_once('/').unwrap();
        Ok(serde_json::from_value(json!({
            "role": "assistant",
            "content": [{"type": "text", "text": self.text}],
            "api": "claude-code",
            "provider": provider,
            "model": model,
            "usage": {"input": 300, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 320,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "stop",
            "timestamp": 0,
        }))
        .unwrap())
    }

    fn takes_tools(&self, _model_ref: &str) -> bool {
        false
    }
}

#[test]
fn judge_chat_rung_on_a_text_only_model_asks_for_json_and_reads_it() {
    let questions = parse_questions(&json!({
        "shown": {"type": "yesno", "instructions": "Does the quoted code show the claim?"},
    }))
    .unwrap();
    let model = Arc::new(TextOnly {
        text: "Here is my answer:\n{\"shown\": {\"value\": false, \"confidence\": 0.8}}".into(),
        asked: Mutex::new(None),
    });
    let chat = Chat {
        model: "claude-code/haiku".into(),
        chat: model.clone(),
    };
    let reply = chat.call("diff and source", &questions, &Signal::new()).unwrap();
    assert_eq!(reply.answers[0].value, Value::Bool(false));
    assert_eq!(reply.model, "claude-code/haiku");

    let asked = model.asked.lock().unwrap().clone().unwrap();
    let Message::System(system) = &asked.messages[0] else {
        panic!("no system message")
    };
    assert!(system.tools_added.is_none(), "a text-only model is offered no tool");
    let user = serde_json::to_string(&asked.messages[1]).unwrap();
    assert!(user.contains("JSON schema"), "{user}");
    assert!(user.contains("\\\"confidence\\\""), "{user}");
}
