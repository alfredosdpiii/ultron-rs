//! `judge.decide` for a session ([`judge`]): the global `judge.*` settings read at each call, the TypeSafe and d1
//! keys from the session's environment and the profile's auth.json, the chat rung on the session's models and
//! provider keys, the default escalation family from the root lane's model, and every call settled in the session's
//! usage ledger and written as a session usage row (`details.source = "judge"`). Loki's borderline writes are put to
//! the same seat, off the write path ([`loki_asker`]).

use crate::compose::Config;
use crate::request_keys::Keys;
use ai::{Cost, Model, Usage};
use delegation::usage::UsageLedger;
use judge::keys::{D1, TYPESAFE, key_source};
use judge::seat::{OnReceipt, Receipt};
use judge::{DispatchChat, JudgeModule, Rung, Seat, SeatDeps};
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};

/// The session usage row of one judge call: its tokens and cost (`details.cost` null when the backend reported
/// none, so the row's zero is not mistaken for a known price).
fn usage_row(values: &services::SessionValues, receipt: &Receipt) {
    if values.handed_over() {
        return;
    }
    let spend = &receipt.spend;
    let (input, output) = (spend.input_tokens.unwrap_or(0), spend.output_tokens.unwrap_or(0));
    let usage = Usage {
        input,
        output,
        total_tokens: input + output,
        cost: Cost {
            total: spend.cost.unwrap_or(0.0),
            ..Cost::default()
        },
        ..Usage::default()
    };
    let Ok(usage) = serde_json::to_value(&usage) else {
        return;
    };
    let details = json!({
        "source": "judge",
        "requestKey": receipt.id,
        "backend": receipt.backend,
        "model": receipt.model,
        "kind": receipt.kind,
        "status": receipt.status,
        "escalation": receipt.escalation,
        "cost": spend.cost,
    });
    let mut session = crate::durable_store::lock(values.session());
    // A journal that cannot take the row is the session's to report; the decision stands.
    let _ = crate::core::record_usage_row(&mut session, usage, details);
}

/// The judge module of a session. `ledger` is the session's usage ledger, set once the stack has built it; `rows` the
/// session whose usage rows record each call (none written while it is unset).
pub fn module(
    config: &Arc<Config>,
    root_model: Arc<Mutex<Model>>,
    keys: Keys,
    rows: Arc<OnceLock<services::SessionValues>>,
    ledger: Arc<OnceLock<UsageLedger>>,
) -> JudgeModule {
    let settings = config.settings_reader();
    let config = config.clone();
    let auth = config::paths::AgentPaths::new(&config.agent_dir).auth();
    let on_receipt: Option<OnReceipt> = Some(Arc::new(move |receipt: &Receipt| {
        if let Some(values) = rows.get() {
            usage_row(values, receipt);
        }
    }));
    JudgeModule::new(
        Arc::new(move || config::keys::judge_settings(settings().global_settings())),
        Arc::new(move |settings| {
            let transport = Arc::new(blocking_http::ReqwestTransport::new()?);
            let models = config.clone();
            let mut deps = SeatDeps::new(
                transport,
                key_source(TYPESAFE, config.var.clone(), auth.clone()),
                key_source(D1, config.var.clone(), auth.clone()),
            );
            deps.chat = Some(Arc::new(DispatchChat {
                models: Arc::new(move |provider: &str, id: &str| models.model(provider, id)),
                keys: keys.request_keys(),
                transport: Arc::new(ai::ReqwestTransport::for_env(&*config.var, None)),
            }));
            let model = jsrt::sync::lock(&root_model).clone();
            deps.session_model = Some(format!("{}/{}", model.provider, model.id));
            deps.ledger = ledger.get().cloned();
            deps.on_receipt = on_receipt.clone();
            Ok(Seat::new(settings, deps))
        }),
    )
}

/// Dreaming's judge (kernel_apis `MemoryService::with_judge`): the session's `judge.decide`, under the same seat,
/// limits and receipts as the model's own calls.
pub fn decider(module: Arc<JudgeModule>) -> kernel_apis::memory::Decide {
    Arc::new(move |state, questions, kind, signal| module.decide_json(state, questions, Some(kind), &[], signal))
}

/// Loki's questions about borderline writes ([`host::file_hooks::Verdict::Ask`]) put to the session's judge seat
/// (`slot`, set once the session's modules are built), on the hooks' own thread: `Ok(true)` the write should not
/// stand. An answer the seat is not sure of (review, abstained) is not one: the user decides.
pub fn loki_asker(slot: Arc<OnceLock<Arc<JudgeModule>>>) -> host::file_hooks::Asker {
    Arc::new(move |question: &str| {
        let module = slot.get().ok_or("the session's judge seat is not ready")?;
        let questions = judge::question::parse_questions(&json!({"refuse": {
            "type": "yesno",
            "instructions": "The text is a guardrail's question about a file write that was just made. Answer yes \
                             when the write should not stand as written, no when it may.",
        }}))?;
        let verdict = module.decide_now(question, &questions, Some("loki.ask"), &[], &kernel::Signal::new())?;
        match (verdict.rung, verdict.answers.first()) {
            (Rung::First | Rung::Escalated, Some((_, answer))) => answer
                .value
                .as_bool()
                .ok_or_else(|| "the judge's answer was not yes or no".into()),
            _ => Err(verdict.reason.unwrap_or_else(|| "the judge was not sure".into())),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_core_loki_asker_leaves_an_unsettled_question_to_the_user() {
        let slot: Arc<OnceLock<Arc<JudgeModule>>> = Arc::default();
        let asker = loki_asker(slot.clone());
        assert_eq!(
            asker("Is it real?"),
            Err("the session's judge seat is not ready".into())
        );
        // A seat with no backend abstains: not an answer, so the user decides.
        let module = JudgeModule::new(
            Arc::new(|| config::keys::judge_settings(json!({"judge": {"provider": []}}).as_object().unwrap())),
            Arc::new(|settings| {
                let none: judge::backend::KeySource = Arc::new(|| None);
                let deps = SeatDeps::new(Arc::new(blocking_http::ReqwestTransport::new()?), none.clone(), none);
                Ok(Seat::new(settings, deps))
            }),
        );
        let _ = slot.set(Arc::new(module));
        assert_eq!(
            asker("Is it real?"),
            Err("no judge backend is configured (judge.provider, judge.fallbackModel)".into())
        );
    }
}
