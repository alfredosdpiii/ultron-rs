//! Reads and updates the dedicated `~/.ultron/learning/` topic store for the TUI `/learning` picker.

use serde_json::Value;
use std::path::{Path, PathBuf};

const ACTIVE_FILE: &str = "_active.json";
const NEW_TOPIC_VALUE: &str = "__new__";

/// Summary of one saved learning topic for the interactive TUI picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSummary {
    pub topic: String,
    pub slug: String,
    pub active: bool,
    pub mastery_pct: u64,
    pub due_count: usize,
    pub updated_at: String,
}

/// Dedicated learning directory (`ULTRON_LEARNING_DIR` or `~/.ultron/learning`).
pub fn learning_dir() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("ULTRON_LEARNING_DIR") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".ultron").join("learning"))
}

/// Sentinel value representing "+ Start a new topic" in the TUI picker.
pub const fn new_topic_sentinel() -> &'static str {
    NEW_TOPIC_VALUE
}

/// Filesystem-safe slug matching `learning_api.topic_slug`.
pub fn topic_slug(topic: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = true;
    for ch in topic.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_end_matches('-').chars().take(80).collect()
}

/// Returns true if a saved topic JSON for `topic` already exists in `dir`.
pub fn has_saved_topic(dir: &Path, topic: &str) -> bool {
    let slug = topic_slug(topic);
    !slug.is_empty() && dir.join(format!("{slug}.json")).is_file()
}

/// Reads the persisted active topic name from `_active.json`, if present.
pub fn active_topic(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(ACTIVE_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["topic"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn mastery_and_due(value: &Value) -> (u64, usize) {
    let Some(subskills) = value["subskills"].as_array() else {
        return (0, 0);
    };
    if subskills.is_empty() {
        return (0, 0);
    }
    let target = value["target_level"].as_u64().unwrap_or(4).max(1);
    let turn = value["turn"].as_i64().unwrap_or(0);
    let mut sum = 0u64;
    let mut due = 0usize;
    for item in subskills {
        let lvl = item["level"].as_u64().unwrap_or(0);
        sum += lvl.min(target);
        let last = item["last_turn"].as_i64().unwrap_or(0);
        let interval = item["interval_turns"].as_i64().unwrap_or(3);
        if lvl > 0 && last > 0 && (turn - last) >= interval {
            due += 1;
        }
    }
    let pct = ((sum as f64) / ((subskills.len() as u64 * target) as f64) * 100.0).round() as u64;
    (pct, due)
}

fn parse_topic_file(path: &Path, active: Option<&str>) -> Option<TopicSummary> {
    let name = path.file_name().and_then(|n| n.to_str())?;
    if !name.ends_with(".json") || name == ACTIVE_FILE {
        return None;
    }

    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let topic = value["topic"].as_str().map(str::trim).filter(|s| !s.is_empty())?;
    let slug = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let (mastery_pct, due_count) = mastery_and_due(&value);
    let updated_at = value["updated_at"].as_str().unwrap_or("").to_string();

    Some(TopicSummary {
        topic: topic.to_string(),
        slug,
        active: active == Some(topic),
        mastery_pct,
        due_count,
        updated_at,
    })
}

/// Lists all saved learning topics in `dir`, active topic first, then most recently updated.
pub fn list_topics(dir: &Path) -> Vec<TopicSummary> {
    let active = active_topic(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<TopicSummary> = entries
        .flatten()
        .filter_map(|entry| parse_topic_file(&entry.path(), active.as_deref()))
        .collect();
    items.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| b.updated_at.cmp(&a.updated_at)));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_saved_topics_with_active_first() {
        let dir = std::env::temp_dir().join(format!("ultron-learning-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rust_doc = json!({
            "topic": "Rust Lifetimes",
            "target_level": 4,
            "turn": 6,
            "updated_at": "2026-11-28T10:00:00Z",
            "subskills": [{"name": "borrows", "level": 2, "last_turn": 2, "interval_turns": 3}],
        });
        let raft_doc = json!({
            "topic": "Raft Consensus",
            "target_level": 4,
            "turn": 1,
            "updated_at": "2026-11-28T11:00:00Z",
            "subskills": [{"name": "election", "level": 4, "last_turn": 1, "interval_turns": 6}],
        });
        std::fs::write(dir.join("rust-lifetimes.json"), rust_doc.to_string()).unwrap();
        std::fs::write(dir.join("raft.json"), raft_doc.to_string()).unwrap();
        std::fs::write(
            dir.join(ACTIVE_FILE),
            r#"{"topic":"Rust Lifetimes","slug":"rust-lifetimes"}"#,
        )
        .unwrap();

        let topics = list_topics(&dir);
        assert!(has_saved_topic(&dir, "Rust Lifetimes"));
        assert!(!has_saved_topic(&dir, "CUDA Programming"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].topic, "Rust Lifetimes");
        assert!(topics[0].active);
        assert_eq!(topics[0].mastery_pct, 50);
        assert_eq!(topics[0].due_count, 1);
        assert_eq!(topics[1].topic, "Raft Consensus");
        assert_eq!(topics[1].mastery_pct, 100);
    }
}
