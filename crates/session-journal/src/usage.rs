//! `harness/utils/usage.ts`: usage totals as JSON values, with JavaScript number arithmetic.

use serde_json::{Map, Value, json};

// ponytail: kept as a re-export for session-core's op_journal and run.rs; new code calls `jsrt::json::number`.
pub use jsrt::json::number as num;

fn field(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `emptyUsage()`.
pub fn empty_usage() -> Value {
    json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
    })
}

/// `addUsage(left, right)`.
pub fn add_usage(left: &Value, right: &Value) -> Value {
    let mut out = Map::new();
    for key in ["input", "output", "cacheRead", "cacheWrite"] {
        out.insert(key.into(), num(field(left, key) + field(right, key)));
    }
    for key in ["cacheWrite1h", "reasoning"] {
        if left.get(key).is_some() || right.get(key).is_some() {
            out.insert(key.into(), num(field(left, key) + field(right, key)));
        }
    }
    out.insert(
        "totalTokens".into(),
        num(field(left, "totalTokens") + field(right, "totalTokens")),
    );
    let (lc, rc) = (&left["cost"], &right["cost"]);
    let mut cost = Map::new();
    for key in ["input", "output", "cacheRead", "cacheWrite", "total"] {
        cost.insert(key.into(), num(field(lc, key) + field(rc, key)));
    }
    out.insert("cost".into(), Value::Object(cost));
    Value::Object(out)
}
