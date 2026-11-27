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
        let mut violations = loki::rules::content_rule_violations(&parsed, false)
            .map_err(|error| format!("loki: {}", error.describe()))?;
        let fallback = loki::rules::content_rule_violations(&parsed, true)
            .map_err(|error| format!("loki: {}", error.describe()))?;
        for item in fallback {
            if !violations.contains(&item) {
                violations.push(item);
            }
        }
        Self::append_hygiene(&parsed, &mut violations);
        Ok(json!({ "violations": violations }))
    }

    fn append_hygiene(parsed: &[(String, String, String)], violations: &mut Vec<String>) {
        let code_files: Vec<&(String, String, String)> = parsed
            .iter()
            .filter(|(path, _, new_text)| !new_text.is_empty() && loki::rules::language(path).is_some())
            .collect();
        if code_files.is_empty() {
            return;
        }
        let texts: Vec<&str> = code_files.iter().map(|(_, _, new_text)| new_text.as_str()).collect();
        let Ok(scored) = loki::hygiene::score(&texts, loki::proc::Deadline::after(5.0)) else {
            return;
        };
        for ((path, old_text, new_text), squiggles) in code_files.iter().zip(scored) {
            let notes = loki::hygiene::notes(path, old_text, new_text, &squiggles, loki::hygiene::Level::On);
            let fresh: Vec<String> = notes.into_iter().filter(|note| !violations.contains(note)).collect();
            violations.extend(fresh);
        }
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
