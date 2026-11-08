//! Playbook routing (Rust-only, engineering mode): a user message that asks for engineering work opens its run with
//! the rigor playbook that fits it, whole, before the first edit.
//!
//! Problem: the prompt and the situational skills name rigor and its playbooks, and a model did not read them: not
//! from the prompt, not from a pointer, not from the playbook list given after its edits. What arrived before the
//! action it governs (the refused commit) changed what the model did; what arrived after it did not.
//!
//! Fix: the request is matched against keyword routes ([`ROUTES`], first match wins) and the run opens with an
//! `ultron.playbook` message holding that playbook's steps. A playbook already shown since the last compaction is not
//! shown again.

use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::LazyLock;

pub const PLAYBOOK_CUSTOM_TYPE: &str = "ultron.playbook";
/// A playbook longer than this is named, not added whole.
pub const MAX_PLAYBOOK_CHARS: usize = 5000;

/// (playbook file stem, title, request pattern), in priority order.
// ponytail: keyword routes, not a model call; a wrong pick costs one read of the right playbook. Ask a small
// model when misroutes show up in sessions.
pub const ROUTES: &[(&str, &str, &str)] = &[
    (
        "bug-fix",
        "Bug fix",
        r"(?i)\b(fix(es|ed)?|bug|broken|crash(es|ing)?|regression|fails?|failing|doesn'?t work|not working|wrong)\b",
    ),
    (
        "perf-issue",
        "Perf issue",
        r"(?i)\b(slow(er|ness)?|perf(ormance)?|latency|speed (it )?up|faster|memory leak)\b",
    ),
    (
        "refactoring",
        "Refactoring",
        r"(?i)\b(refactor\w*|rename\w*|extract|inline|restructure|reorgani[sz]e|clean ?up|split (up )?|move \w+ (to|into))\b",
    ),
    (
        "feature",
        "Feature",
        r"(?i)\b(add|implement|build|support|introduce|create|make it|new (option|flag|command|endpoint|page))\b",
    ),
    (
        "investigation",
        "Investigation",
        r"(?i)(\bhow (does|do|is)\b|\bwhy (does|do|is)\b|\binvestigate\b|\bexplain\b|\bare we sure\b)",
    ),
];

static COMPILED: LazyLock<Vec<(&'static str, &'static str, Regex)>> = LazyLock::new(|| {
    ROUTES
        .iter()
        .map(|(file, title, pattern)| (*file, *title, Regex::new(pattern).expect("valid route")))
        .collect()
});

/// The playbook (file stem, title) for a user message, `None` for a command, a short reply or no match.
pub fn route(text: &str) -> Option<(&'static str, &'static str)> {
    let text = text.trim();
    if text.starts_with('/') || text.split_whitespace().count() < 5 {
        return None;
    }
    COMPILED
        .iter()
        .find(|(_, _, pattern)| pattern.is_match(text))
        .map(|(file, title, _)| (*file, *title))
}

/// Playbooks shown on the branch since its last compaction.
pub fn shown(entries: &[Value]) -> Vec<String> {
    session_journal::context::custom_details_since_compaction(entries, PLAYBOOK_CUSTOM_TYPE)
        .into_iter()
        .filter_map(|details| details["playbook"].as_str().map(str::to_string))
        .collect()
}

/// The message that opens a run of `text`: the fitting playbook from `rigor_dir` (the rigor skill's directory),
/// unless `shown` has it or its file is missing.
pub fn playbook_message(text: &str, rigor_dir: &Path, shown: &[String], timestamp: u64) -> Option<Value> {
    let (file, title) = route(text)?;
    if shown.iter().any(|name| name == file) {
        return None;
    }
    let path = rigor_dir.join("playbooks").join(format!("{file}.md"));
    let body = std::fs::read_to_string(&path).ok()?;
    let body = body.trim();
    let dir = rigor_dir.display();
    let head = format!(
        "[playbook] This request reads as a {title}. Follow rigor's {title} playbook below: put its steps in \
         state[\"plan\"] before your first edit and work through them; a step you skip stays with `skip: <reason>`. \
         Name a better-fitting playbook in one line if this one is wrong. Paths are relative to {dir}."
    );
    let content = if body.chars().count() > MAX_PLAYBOOK_CHARS {
        format!("{head}\nRead {} now, before the first edit.", path.display())
    } else {
        format!("{head}\n\n{body}")
    };
    Some(json!({
        "role": "custom",
        "customType": PLAYBOOK_CUSTOM_TYPE,
        "content": content,
        "display": true,
        "details": {"playbook": file},
        "timestamp": timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playbook_route_picks_the_first_matching_route() {
        assert_eq!(
            route("the login test fails after the upgrade, please fix it").map(|r| r.0),
            Some("bug-fix")
        );
        assert_eq!(
            route("Rename formatMessageTime to formatTimestamp and update every caller").map(|r| r.0),
            Some("refactoring")
        );
        assert_eq!(
            route("add a --json flag to the usage command").map(|r| r.0),
            Some("feature")
        );
        assert_eq!(
            route("how does the session worker start up").map(|r| r.0),
            Some("investigation")
        );
        assert_eq!(
            route("the dashboard got slow since last week").map(|r| r.0),
            Some("perf-issue")
        );
        assert_eq!(route("ok thanks"), None, "short replies");
        assert_eq!(route("/model claude opus five now please"), None, "commands");
        assert_eq!(route("summarize what we did in this session today"), None, "no route");
    }

    #[test]
    fn playbook_route_opens_once_with_the_playbook_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("playbooks")).unwrap();
        std::fs::write(
            dir.path().join("playbooks/refactoring.md"),
            "### Refactoring\n\n1. Pin the contract.\n",
        )
        .unwrap();
        let text = "Rename formatMessageTime to formatTimestamp in lib/date-utils.ts";
        let message = playbook_message(text, dir.path(), &[], 7).unwrap();
        let content = message["content"].as_str().unwrap();
        assert!(
            content.starts_with("[playbook] This request reads as a Refactoring."),
            "{content}"
        );
        assert!(
            content.ends_with("### Refactoring\n\n1. Pin the contract."),
            "{content}"
        );
        let entries = [json!({"type": "message", "message": message})];
        assert_eq!(shown(&entries), ["refactoring"]);
        assert_eq!(playbook_message(text, dir.path(), &shown(&entries), 8), None, "once");
        let compacted = [entries[0].clone(), json!({"type": "compaction"})];
        assert!(
            playbook_message(text, dir.path(), &shown(&compacted), 9).is_some(),
            "again after a compaction"
        );
        assert_eq!(
            playbook_message("fix the crash in the parser module now", dir.path(), &[], 7),
            None,
            "missing file"
        );
    }
}
