//! The built-in conversation documents (spec §6, §8.2, §8.6): `ultron.live` (run control and presentation), `ultron.inbox`
//! (queued submissions), `ultron.usage` (spend ledger), `ultron.agent` (model and thinking level) and `ultron.provider` (the
//! provider-facing session identity). Every Harness conversation gets them in its creating commit.

use std::sync::LazyLock;

use durable::{Doc, DocDefinition, DocSemantics, DocumentFork, DocumentHistory};
use serde_json::{Value, json};

fn conversation(history: DocumentHistory, fork: DocumentFork) -> DocSemantics {
    DocSemantics::Conversation { history, fork }
}

fn define(definition: DocDefinition) -> Doc {
    definition.define().expect("built-in document definitions are valid")
}

fn running(slots: Option<&Value>) -> bool {
    slots
        .and_then(Value::as_array)
        .is_some_and(|slots| slots.iter().any(|slot| slot["status"] == "running"))
}

/// `ultron.live`: run control (`run`), the current generation attempt, the current tool round, nested calls and
/// compaction statuses. A complete base whenever nothing runs: no `generation` and no running slot.
pub static LIVE: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "ultron.live",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({}),
        )
        .checkpoint_when(|_, value, _, _| {
            Ok(value.get("generation").is_none() && !running(value.get("tools")) && !running(value.get("nestedTools")))
        }),
    )
});

/// `ultron.inbox`: the conversation's queued submissions in ID order; a complete base whenever it is empty.
pub static INBOX: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "ultron.inbox",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "items": [] }),
        )
        .checkpoint_when(|_, value, _, _| Ok(value["items"].as_array().is_none_or(|items| items.is_empty()))),
    )
});

/// `ultron.usage`: assistant spend per `provider/model` and tool spend per tool name; a base on every change.
pub static USAGE: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "ultron.usage",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "models": {}, "tools": {} }),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});

/// `ultron.agent`: the conversation's model (`{provider, modelId}`) and thinking level; rewindable so a fork starts from
/// the agent at its fork entry.
pub static AGENT: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "ultron.agent",
            1,
            conversation(DocumentHistory::Rewindable, DocumentFork::AsOf),
            |_| json!({}),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});

/// `ultron.provider`: the stable provider session ID; every fork starts with a fresh one.
pub static PROVIDER: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "ultron.provider",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "sessionId": jsrt::random::uuid_v7(jsrt::date::now_ms()) }),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});
