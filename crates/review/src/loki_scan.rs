//! `review.loki`: Loki's core rule pack over a review's changed files, for the deterministic stage of the engine
//! (`autoreview_static.py`). Each change is `[path, old text, new text]`; the reply is Loki's net-new findings
//! (`path:line: rule: message`), so a credential or conflict marker the base commit already had is not this
//! change's. The repository's own `.loki/` policy is not read: a review checks every repository with the same rules.

use kernel_apis::{ApiModule, Request};
use serde_json::{Value, json};

/// Changes one scan takes at most.
pub const MAX_CHANGES: usize = 200;

pub struct LokiScan;

impl LokiScan {
    fn scan(payload: &serde_json::Map<String, Value>) -> Result<Value, String> {
        let changes = payload
            .get("changes")
            .and_then(Value::as_array)
            .ok_or("review.loki needs changes: [[path, old, new], ...]")?;
        let mut parsed = Vec::new();
        for change in changes.iter().take(MAX_CHANGES) {
            let triple = change.as_array().filter(|items| items.len() == 3);
            let text = |index: usize| triple.and_then(|items| items[index].as_str()).map(str::to_string);
            match (text(0), text(1), text(2)) {
                (Some(path), Some(old), Some(new)) if !path.is_empty() => parsed.push((path, old, new)),
                _ => return Err("review.loki: each change is [path, old text, new text]".into()),
            }
        }
        let violations = loki::rules::content_rule_violations(&parsed, false)
            .map_err(|error| format!("loki: {}", error.describe()))?;
        Ok(json!({ "violations": violations }))
    }
}

impl ApiModule for LokiScan {
    fn prefixes(&self) -> &[&str] {
        &["review.loki"]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "review.loki" => Self::scan(request.payload),
            other => Err(format!("Unknown review request: {other}")),
        }
    }
}
