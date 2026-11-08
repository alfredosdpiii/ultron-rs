//! `judge.decide` for a session ([`judge`]): the global `judge.*` settings read at each call, the TypeSafe and d1
//! keys from the session's environment and the profile's auth.json, the chat rung on the session's models and
//! provider keys, the default escalation family from the root lane's model, and every call settled in the session's
//! usage ledger and written as a session usage row (`details.source = "judge"`).

use crate::compose::Config;
use crate::request_keys::Keys;
use ai::{Cost, Model, Usage};
use delegation::usage::UsageLedger;
use judge::keys::{D1, TYPESAFE, key_source};
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

/// The judge module of a session. `ledger` is the session's usage ledger, set once the stack has built it.
pub fn module(
    config: &Arc<Config>,
    root_model: Arc<Mutex<Model>>,
    keys: Keys,
    values: Option<services::SessionValues>,
    ledger: Arc<OnceLock<UsageLedger>>,
) -> JudgeModule {
    let settings = config.settings_reader();
    let config = config.clone();
    let auth = config::paths::AgentPaths::new(&config.agent_dir).auth();
    let on_receipt: Option<OnReceipt> =
        values.map(|values| -> OnReceipt { Arc::new(move |receipt: &Receipt| usage_row(&values, receipt)) });
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
