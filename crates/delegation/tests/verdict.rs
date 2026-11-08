//! Replays fixtures/verdict.json (recorded by fixtures/generate-verdict.mjs from verdict.ts): validateVerdict,
//! isConcreteEvidence, workspacePath and verdictTag give the same verdicts, problems, paths and tags.

use delegation::verdict::{is_concrete_evidence, validate_verdict, verdict_tag, workspace_path};
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/verdict.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn delegation_validate_verdict_matches_typescript() {
    for case in fixture()["validate"].as_array().unwrap() {
        let actual = match validate_verdict(case["payload"].as_object().unwrap(), case["workspace"].as_str()) {
            Ok(verdict) => json!({ "verdict": verdict }),
            Err(problems) => json!({ "problems": problems }),
        };
        assert_eq!(actual.to_string(), case["result"].to_string(), "{}", case["payload"]);
    }
}

#[test]
fn delegation_concrete_evidence_matches_typescript() {
    for case in fixture()["concrete"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(is_concrete_evidence(text), case["concrete"] == true, "{text:?}");
    }
}

#[test]
fn delegation_workspace_path_matches_typescript() {
    for case in fixture()["paths"].as_array().unwrap() {
        let actual = workspace_path(case["path"].as_str().unwrap(), case["workspace"].as_str());
        assert_eq!(actual.as_deref(), case["result"].as_str(), "{case}");
    }
}

#[test]
fn delegation_verdict_tag_matches_typescript() {
    for case in fixture()["tags"].as_array().unwrap() {
        assert_eq!(verdict_tag(&case["result"]).as_deref(), case["tag"].as_str(), "{case}");
    }
}

/// `bounded` flattens with `replace(/\s+/g, " ").trim()`: U+FEFF is whitespace there and U+0085 is not (node 26:
/// `JSON.stringify("a\u{85}b\u{feff}\u{feff}c").replace(/\s+/g, " ")` keeps the U+0085 and joins the U+FEFF run).
#[test]
fn delegation_verdict_bounded_flattens_javascript_whitespace() {
    let payload = json!({"status": "a\u{85}b\u{feff}\u{feff}c"});
    let problems = validate_verdict(payload.as_object().unwrap(), None).unwrap_err();
    assert!(
        problems.contains(&"status must be \"passed\", \"failed\" or \"blocked\", not \"a\u{85}b c\"".to_string()),
        "{problems:?}"
    );
}
