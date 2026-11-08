//! `judge.decide` for a session ([`judge`]): the global `judge.*` settings read at each call, the TypeSafe and d1
//! keys from the session's environment and the profile's auth.json, the chat rung on the session's models and
//! provider keys, the default escalation family from the root lane's model, and every call settled in the session's
//! usage ledger and written as a session usage row (`details.source = "judge"`).

use crate::compose::Config;
use crate::request_keys::Keys;
use ai::{Cost, Model, Usage};
use delegation::usage::{Measurement, Reservation, ReserveRequest, UsageLedger};
use judge::seat::{OnReceipt, Receipt};
use judge::{DispatchChat, JudgeModule, Seat, SeatDeps};
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

/// The session's usage ledger as the judge seat reserves and settles its calls in it: each a `model` call keyed by
/// its receipt id.
pub struct SessionLedger(pub UsageLedger);

impl judge::SpendLedger for SessionLedger {
    fn cost_cap(&self) -> Option<f64> {
        self.0.limits().max_cost_usd
    }

    fn reserve(&self, id: &str) -> Result<serde_json::Value, String> {
        self.0
            .reserve(&ReserveRequest::model(id))
            .map(|reservation| reservation.0)
    }

    fn settle(&self, reservation: &serde_json::Value, status: &str, spend: &judge::Spend, wall_ms: f64) {
        let total = spend.input_tokens.zip(spend.output_tokens).map(|(i, o)| (i + o) as f64);
        let measured = Measurement {
            input_tokens: spend.input_tokens.map(|n| n as f64),
            output_tokens: spend.output_tokens.map(|n| n as f64),
            total_tokens: total,
            cost: spend.cost,
            wall_ms: Some(wall_ms),
        };
        // A ledger that cannot record is the session's to report; the decision stands.
        let _ = self
            .0
            .settle(&Reservation(reservation.clone()), Some(status), Some(&measured));
    }
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
            let mut deps = SeatDeps::with_profile_keys(transport, config.var.clone(), auth.clone());
            deps.chat = Some(Arc::new(DispatchChat {
                models: Arc::new(move |provider: &str, id: &str| models.model(provider, id)),
                keys: keys.request_keys(),
                transport: Arc::new(ai::ReqwestTransport::for_env(&*config.var, None)),
            }));
            let model = jsrt::sync::lock(&root_model).clone();
            deps.session_model = Some(format!("{}/{}", model.provider, model.id));
            deps.ledger = ledger
                .get()
                .map(|ledger| Arc::new(SessionLedger(ledger.clone())) as Arc<dyn judge::SpendLedger>);
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

#[cfg(test)]
mod tests {
    use super::*;
    use delegation::usage::UsageLimits;
    use judge::SpendLedger;

    /// Each judge call is a `model` call of the session's ledger under its receipt id, settled with its status,
    /// tokens and cost; the ledger's cost cap is what the seat reads.
    #[test]
    fn session_core_judge_calls_are_settled_in_the_session_usage_ledger() {
        let limits = UsageLimits {
            max_cost_usd: Some(0.5),
            ..UsageLimits::default()
        };
        let ledger = UsageLedger::in_memory(limits);
        let session = SessionLedger(ledger.clone());
        assert_eq!(session.cost_cap(), Some(0.5));
        let spend = judge::Spend {
            input_tokens: Some(400),
            output_tokens: Some(42),
            cost: Some(0.0003),
        };
        let reservation = session.reserve("judge:1").unwrap();
        session.settle(&reservation, "succeeded", &spend, 12.0);
        let failed = session.reserve("judge:2").unwrap();
        session.settle(&failed, "failed", &judge::Spend::default(), 3.0);
        let document = ledger.document().unwrap();
        let calls = document["roots"][delegation::usage::DEFAULT_ROOT_ID]["calls"]
            .as_array()
            .cloned()
            .unwrap();
        let seen: Vec<(String, String, String)> = calls
            .iter()
            .map(|c| {
                (
                    c["kind"].to_string(),
                    c["requestKey"].to_string(),
                    c["status"].to_string(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("\"model\"".into(), "\"judge:1\"".into(), "\"succeeded\"".into()),
                ("\"model\"".into(), "\"judge:2\"".into(), "\"failed\"".into()),
            ]
        );
        assert_eq!(calls[0]["usage"]["cost"].as_f64(), Some(0.0003));
        assert_eq!(calls[0]["usage"]["totalTokens"].as_f64(), Some(442.0));
    }
}
