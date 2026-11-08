//! `budget.estimate(plan)`: what planned model work would cost before it starts. A plan is a list of units, each
//! `calls` × requests per call × tokens per request × the price of the unit's model in the model catalog. A model
//! without a catalog price is listed as unpriced and left out of the total, never guessed. Request and token counts a
//! unit does not give come from its kind's shape below and are listed as assumed.
//!
//! The same estimate runs before a workflow (`workflows.run` returns it with the outcomes), and a node's share is the
//! cost its admission is checked against the stop-line of the usage ledger ([`crate::usage::ReserveRequest`]).

use jsrt::json;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// A model's catalog rates, USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
}

/// The model a unit runs on and its rates (None: the model is unknown or the catalog has no price for it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Priced {
    pub model: Option<String>,
    pub rates: Option<Rates>,
}

/// Prices a unit of `kind` on `model` (None: the kind's default model in this session).
pub type Pricing = Arc<dyn Fn(&str, Option<&str>) -> Priced + Send + Sync>;

/// What one call of a kind is, when the unit does not say: requests per call, input and output tokens per request.
/// Input is priced at the full input rate (no cache discount), so the estimate leans high.
struct Shape {
    kind: &'static str,
    requests: f64,
    input: f64,
    output: f64,
}

const SHAPES: [Shape; 4] = [
    // A subagent: about 20 model turns, each re-sending a transcript that grows to tens of thousands of tokens.
    Shape {
        kind: "child",
        requests: 20.0,
        input: 30_000.0,
        output: 1_500.0,
    },
    // One `rlm.infer` / `rlm.map` frame: one request over the views it is given.
    Shape {
        kind: "frame",
        requests: 1.0,
        input: 4_000.0,
        output: 500.0,
    },
    // One completion without tools (a predict task, a judge escalation).
    Shape {
        kind: "chat",
        requests: 1.0,
        input: 4_000.0,
        output: 1_000.0,
    },
    // One judge decision: a short state and closed questions.
    Shape {
        kind: "judge",
        requests: 1.0,
        input: 1_000.0,
        output: 20.0,
    },
];

/// The most calls one unit may plan.
const MAX_CALLS: f64 = 100_000.0;

/// One planned unit of model work.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub kind: String,
    pub calls: u64,
    pub model: Option<String>,
    pub requests: Option<f64>,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    /// The workflow node the unit is (workflow estimates only).
    pub node: Option<String>,
}

impl Unit {
    pub fn new(kind: &str, calls: u64, model: Option<String>) -> Unit {
        Unit {
            kind: kind.into(),
            calls,
            model,
            requests: None,
            input_tokens: None,
            output_tokens: None,
            node: None,
        }
    }
}

fn shape(kind: &str) -> Option<&'static Shape> {
    SHAPES.iter().find(|shape| shape.kind == kind)
}

fn kinds() -> String {
    SHAPES.iter().map(|s| s.kind).collect::<Vec<_>>().join(", ")
}

/// A plan as `budget.estimate` takes it: one unit or a list of them, each
/// `{kind, calls?, model?, requests?, input_tokens?, output_tokens?}`.
pub fn parse_plan(plan: &Value) -> Result<Vec<Unit>, String> {
    let items = match plan {
        Value::Array(items) if !items.is_empty() => items.clone(),
        Value::Object(_) => vec![plan.clone()],
        _ => return Err("plan must be a unit or a nonempty list of units ({kind, calls, model, ...})".into()),
    };
    items.iter().map(unit).collect()
}

fn unit(value: &Value) -> Result<Unit, String> {
    let item = value.as_object().ok_or("a plan unit must be an object")?;
    jsrt::payload::fields(
        item,
        &["kind", "calls", "model", "requests", "input_tokens", "output_tokens"],
    )?;
    let kind = item
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| shape(kind).is_some())
        .ok_or_else(|| format!("unit kind must be one of {}", kinds()))?;
    let count = |key: &str, minimum: f64, maximum: f64| -> Result<Option<f64>, String> {
        match item.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|n| n.fract() == 0.0 && (minimum..=maximum).contains(n))
                .map(Some)
                .ok_or_else(|| format!("{key} must be an integer from {minimum} to {maximum}")),
        }
    };
    let model = match item.get("model") {
        None | Some(Value::Null) => None,
        Some(model) => Some(model_scope::model_ref(model)?),
    };
    Ok(Unit {
        kind: kind.into(),
        calls: count("calls", 1.0, MAX_CALLS)?.unwrap_or(1.0) as u64,
        model,
        requests: count("requests", 1.0, 10_000.0)?,
        input_tokens: count("input_tokens", 0.0, 10_000_000.0)?,
        output_tokens: count("output_tokens", 0.0, 10_000_000.0)?,
        node: None,
    })
}

fn usd(n: f64) -> Value {
    json::number((n * 1e6).round() / 1e6)
}

/// The cost of one call of `unit` on `priced`, when its model has a price.
pub fn call_cost(unit: &Unit, priced: &Priced) -> Option<f64> {
    let shape = shape(&unit.kind)?;
    let rates = priced.rates?;
    let requests = unit.requests.unwrap_or(shape.requests);
    let input = unit.input_tokens.unwrap_or(shape.input);
    let output = unit.output_tokens.unwrap_or(shape.output);
    Some(requests * (input * rates.input + output * rates.output) / 1_000_000.0)
}

/// The estimate of `units`: each unit's model, requests, tokens and cost; the priced total; the units without a
/// price; and, from the ledger's `status` of the caller's root, whether the total fits what is left of its cost cap.
pub fn estimate(units: &[Unit], pricing: Option<&Pricing>, status: Option<&Value>) -> Value {
    let mut rows = Vec::new();
    let (mut total, mut unpriced) = (0.0, Vec::new());
    for unit in units {
        let Some(shape) = shape(&unit.kind) else { continue };
        let priced = pricing.map_or_else(
            || Priced {
                model: unit.model.clone(),
                rates: None,
            },
            |price| price(&unit.kind, unit.model.as_deref()),
        );
        let calls = unit.calls as f64;
        let requests = unit.requests.unwrap_or(shape.requests);
        let mut assumed = Vec::new();
        for (given, name) in [
            (unit.requests.is_some(), "requests"),
            (unit.input_tokens.is_some(), "input_tokens"),
            (unit.output_tokens.is_some(), "output_tokens"),
        ] {
            if !given {
                assumed.push(name);
            }
        }
        let mut row = Map::new();
        if let Some(node) = &unit.node {
            row.insert("node".into(), json!(node));
        }
        row.insert("kind".into(), json!(unit.kind));
        row.insert("model".into(), json!(priced.model));
        row.insert("calls".into(), json!(unit.calls));
        row.insert("requests".into(), json::number(calls * requests));
        row.insert(
            "input_tokens".into(),
            json::number(calls * requests * unit.input_tokens.unwrap_or(shape.input)),
        );
        row.insert(
            "output_tokens".into(),
            json::number(calls * requests * unit.output_tokens.unwrap_or(shape.output)),
        );
        match call_cost(unit, &priced) {
            Some(cost) => {
                total += cost * calls;
                row.insert("cost_usd".into(), usd(cost * calls));
            }
            None => {
                row.insert("cost_usd".into(), Value::Null);
                unpriced.push(json!({ "kind": unit.kind, "model": priced.model }));
            }
        }
        row.insert("assumed".into(), json!(assumed));
        rows.push(Value::Object(row));
    }
    let complete = unpriced.is_empty();
    let mut out = json!({
        "units": rows,
        "cost_usd": usd(total),
        "complete": complete,
        "unpriced": unpriced,
    });
    if let Some(status) = status {
        out["budget"] = budget(status, total, complete);
    }
    out
}

/// The caller's cost cap and call ceilings beside the estimate; `fits` is null without a cap or when a unit has no
/// price (the total is then a lower bound).
fn budget(status: &Value, total: f64, complete: bool) -> Value {
    let cap = status["limits"]["maxCostUsd"].as_f64();
    let spent = status["cost"]["spentUsd"].as_f64().unwrap_or(0.0);
    let fits = match cap {
        Some(cap) if complete => json!(spent + total <= cap),
        _ => Value::Null,
    };
    let mut out = json!({
        "max_cost_usd": cap.map_or(Value::Null, usd),
        "spent_usd": usd(spent),
        "remaining_usd": cap.map_or(Value::Null, |cap| usd((cap - spent).max(0.0))),
        "unknown_priced_calls": status["cost"]["unknownPricedCalls"],
        "fits": fits,
    });
    if let Some(ceilings) = status["limits"].get("maxCalls") {
        out["max_calls"] = ceilings.clone();
        out["calls_used"] = status["calls"].clone();
    }
    out
}
