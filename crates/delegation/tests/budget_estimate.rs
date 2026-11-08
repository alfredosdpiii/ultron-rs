//! `budget.estimate` over the typed-agent host: a plan's units priced by the session's pricing (catalog rates per
//! million tokens), unknown prices flagged and left out of the total, the caller's cost cap beside it, a workflow's
//! nodes as units, and the stop-line refusing a workflow node whose estimate would cross the cap. New in Rust: no
//! TypeScript counterpart; literal expectations.

use delegation::agents::{AdapterRequest, Agents, AgentsOptions, LaneReply, LaneRun};
use delegation::estimate::{Priced, Rates, Unit, call_cost, parse_plan};
use delegation::usage::{UsageLedger, UsageLimits};
use kernel::store::MemoryDocumentStore;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `test/cheap` costs $1 / $2 per million input / output tokens, `test/dear` $100 / $200; any other model is
/// unpriced. A child defaults to `test/cheap`, a judge has no default.
fn pricing() -> delegation::estimate::Pricing {
    Arc::new(|kind: &str, model: Option<&str>| {
        let model = model
            .map(str::to_string)
            .or_else(|| (kind != "judge").then(|| "test/cheap".to_string()));
        let rates = match model.as_deref() {
            Some("test/cheap") => Some(Rates {
                input: 1.0,
                output: 2.0,
            }),
            Some("test/dear") => Some(Rates {
                input: 100.0,
                output: 200.0,
            }),
            _ => None,
        };
        Priced { model, rates }
    })
}

struct Host {
    agents: Agents,
    usage: UsageLedger,
    lane_runs: Arc<AtomicUsize>,
}

fn host(limits: UsageLimits) -> Host {
    let usage = UsageLedger::in_memory(limits);
    let lane_runs = Arc::new(AtomicUsize::new(0));
    let mut options = AgentsOptions::new(Box::new(MemoryDocumentStore::default()));
    options.definition_store = Some(Box::new(MemoryDocumentStore::default()));
    options.usage = Some(usage.clone());
    options.pricing = Some(pricing());
    options.deterministic = Some(Arc::new(|request: AdapterRequest<'_>| Ok(request.input.clone())));
    let runs = lane_runs.clone();
    options.lanes = Some(Arc::new(move |_: LaneRun| {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(LaneReply {
            text: "done".into(),
            ..LaneReply::default()
        })
    }));
    let (agents, _) = Agents::new(options);
    Host {
        agents,
        usage,
        lane_runs,
    }
}

impl Host {
    fn estimate(&self, payload: Value) -> Result<Value, String> {
        self.agents.handle("budget.estimate", payload, "main", None)
    }
}

#[test]
fn delegation_estimate_plans_are_validated() {
    assert!(parse_plan(&json!([])).is_err());
    let error = parse_plan(&json!({"kind": "swarm"})).unwrap_err();
    assert_eq!(error, "unit kind must be one of child, frame, chat, judge");
    assert!(parse_plan(&json!({"kind": "child", "calls": 0})).is_err());
    assert!(parse_plan(&json!({"kind": "child", "model": "no-slash"})).is_err());
    assert!(parse_plan(&json!({"kind": "chat", "price": 1})).is_err());
    let units = parse_plan(&json!([{"kind": "frame", "calls": 3}, {"kind": "chat", "input_tokens": 10}])).unwrap();
    assert_eq!(units[0], Unit::new("frame", 3, None));
    assert_eq!(units[1].input_tokens, Some(10.0));
}

#[test]
fn delegation_estimate_prices_calls_times_requests_times_tokens() {
    let priced = Priced {
        model: Some("test/cheap".into()),
        rates: Some(Rates {
            input: 1.0,
            output: 2.0,
        }),
    };
    // A child: 20 requests of 30,000 input and 1,500 output tokens = 20 * (0.03 + 0.003).
    let cost = call_cost(&Unit::new("child", 1, None), &priced).unwrap();
    assert!((cost - 0.66).abs() < 1e-9, "{cost}");
    let unpriced = Priced {
        model: Some("x/y".into()),
        rates: None,
    };
    assert_eq!(call_cost(&Unit::new("child", 1, None), &unpriced), None);
}

#[test]
fn delegation_estimate_totals_priced_units_and_flags_unpriced_ones() {
    let host = host(UsageLimits {
        max_cost_usd: Some(5.0),
        ..UsageLimits::default()
    });
    let plan = json!([
        {"kind": "child", "calls": 3},
        {"kind": "frame", "calls": 10, "model": "test/dear", "input_tokens": 1000, "output_tokens": 100},
        {"kind": "judge", "calls": 4},
    ]);
    let estimate = host.estimate(json!({"plan": plan})).unwrap();
    assert_eq!(
        estimate["units"][0],
        json!({"kind": "child", "model": "test/cheap", "calls": 3, "requests": 60, "input_tokens": 1_800_000,
               "output_tokens": 90_000, "cost_usd": 1.98, "assumed": ["requests", "input_tokens", "output_tokens"]})
    );
    // 10 frames of 1 request: 10 * (1000 * 100 + 100 * 200) / 1e6.
    assert_eq!(estimate["units"][1]["cost_usd"], json!(1.2));
    assert_eq!(estimate["units"][1]["assumed"], json!(["requests"]));
    assert_eq!(estimate["units"][2]["cost_usd"], Value::Null);
    assert_eq!(estimate["cost_usd"], json!(3.18));
    assert_eq!(estimate["complete"], json!(false));
    assert_eq!(estimate["unpriced"], json!([{"kind": "judge", "model": null}]));
    // A unit without a price leaves the fit unknown, not guessed.
    assert_eq!(
        estimate["budget"],
        json!({"max_cost_usd": 5, "spent_usd": 0, "remaining_usd": 5, "unknown_priced_calls": 0, "fits": null})
    );
    let priced = host.estimate(json!({"plan": {"kind": "child", "calls": 3}})).unwrap();
    assert_eq!(priced["budget"]["fits"], json!(true));
    host.usage.record_turn(None, Some(1.0), Some(4.0)).unwrap();
    let after = host.estimate(json!({"plan": {"kind": "child", "calls": 3}})).unwrap();
    assert_eq!(after["budget"]["fits"], json!(false));
    assert_eq!(after["budget"]["remaining_usd"], json!(1));
    assert!(host.estimate(json!({})).is_err());
    assert!(
        host.estimate(json!({"plan": {"kind": "chat"}, "workflow": {"nodes": []}}))
            .is_err()
    );
}

#[test]
fn delegation_estimate_of_a_workflow_counts_model_nodes_and_revision_rounds() {
    let host = host(UsageLimits::default());
    let nodes = json!([
        {"id": "plain", "definition": "identity@1", "input": {"n": 1}},
        {"id": "fix", "definition": "rlm-child@1", "input": {"prompt": "fix it"}, "model": "test/dear",
         "revise": {"from": "review", "until": {"equals": "done"}, "max_rounds": 3}},
        {"id": "review", "definition": "rlm-child@1", "dependsOn": ["fix"], "inputFrom": "fix"},
    ]);
    let estimate = host.estimate(json!({"workflow": {"nodes": nodes}})).unwrap();
    let units: Vec<(Value, Value, Value)> = estimate["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| (u["node"].clone(), u["model"].clone(), u["calls"].clone()))
        .collect();
    assert_eq!(
        units,
        vec![
            (json!("fix"), json!("test/dear"), json!(3)),
            (json!("review"), json!("test/cheap"), json!(3)),
        ],
        "a deterministic node calls no model; a loop's node and reviewer run once per round at most"
    );
    assert_eq!(estimate["complete"], json!(true));
    // The graph is validated as workflows.run validates it.
    let error = host
        .estimate(
            json!({"workflow": {"nodes": [{"id": "a", "definition": "identity@1", "dependsOn": ["b"], "input": {}}]}}),
        )
        .unwrap_err();
    assert_eq!(error, "Unknown workflow dependency");
}

#[test]
fn delegation_estimate_stop_line_refuses_a_workflow_node_that_would_cross_the_cost_cap() {
    let host = host(UsageLimits {
        max_cost_usd: Some(10.0),
        ..UsageLimits::default()
    });
    // A child on test/dear is estimated at $66; on test/cheap at $0.66.
    let nodes = json!([
        {"id": "dear", "definition": "rlm-child@1", "input": {"prompt": "a"}, "model": "test/dear"},
        {"id": "cheap", "definition": "rlm-child@1", "input": {"prompt": "b"}},
    ]);
    let result = host
        .agents
        .handle("workflows.run", json!({"nodes": nodes}), "main", None)
        .unwrap();
    assert_eq!(result["dear"]["status"], json!("failed"));
    let error = result["dear"]["error"].as_str().unwrap();
    assert!(
        error.starts_with("Admission refused: Usage cost stop-line for root ")
            && error.contains("this task is estimated at $66, and $0 of the $10 cap is spent ($10 left)"),
        "{error}"
    );
    assert_eq!(result["cheap"]["status"], json!("succeeded"), "{}", result["cheap"]);
    assert_eq!(
        host.lane_runs.load(Ordering::SeqCst),
        1,
        "the refused node never started"
    );
}
