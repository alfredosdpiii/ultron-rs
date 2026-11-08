//! The autoreview pipeline: a recorded `autoreview eval` report (tests/data/autoreview-report.json, written by hand)
//! re-scored at each blockAt level with the eval's own scorer.

use optimize::Outcome;
use optimize::autoreview::{parse_report, point};
use serde_json::Value;

const REPORT: &str = include_str!("data/autoreview-report.json");

#[test]
fn a_report_is_rescored_at_each_block_level() {
    let report = parse_report(REPORT, "fallback").unwrap();
    assert_eq!(report.name, "anthropic/claude-sonnet-5");
    assert_eq!(report.block_at, "medium");
    assert_eq!(report.rows.len(), 7);
    let points: Vec<_> = review::plan::LEVELS.iter().map(|level| point(&report, level)).collect();
    let got: Vec<(f64, f64, Outcome)> = points.iter().map(|p| (p.quality, p.penalty, p.outcome)).collect();
    assert_eq!(
        got,
        [
            (0.0, 0.0, Outcome::Failed),      // critical: nothing blocked, bugs no more than controls
            (0.25, 0.0, Outcome::Incomplete), // high: one bug, no control; a review errored
            (0.5, 0.5, Outcome::Failed),      // medium: controls blocked as often as bugs
            (0.75, 0.5, Outcome::Incomplete), // low
            (0.75, 1.0, Outcome::Failed),     // nit: every control blocked
        ]
    );
    let medium = &points[2];
    assert_eq!(medium.metrics["fixInducing"]["located"], 1);
    assert_eq!(medium.metrics["blockAt"], "medium");
    assert_eq!(medium.cost_usd, Some(0.6));
    let failures: Vec<&str> = medium.failures.iter().filter_map(|f| f["failure"].as_str()).collect();
    assert_eq!(
        failures,
        ["review failed", "bug not blocked", "bug not blocked", "control blocked"],
        "{:?}",
        medium.failures
    );
}

#[test]
fn reports_compare_only_on_the_same_cases() {
    let report = parse_report(REPORT, "x").unwrap();
    let mut file: Value = serde_json::from_str(REPORT).unwrap();
    file["model"] = Value::Null;
    file["cases"].as_array_mut().unwrap().reverse();
    let reordered = parse_report(&file.to_string(), "other").unwrap();
    assert_eq!(reordered.name, "other");
    assert_eq!(
        reordered.hash, report.hash,
        "order and model do not change the case set"
    );
    file["cases"].as_array_mut().unwrap().pop();
    assert_ne!(parse_report(&file.to_string(), "x").unwrap().hash, report.hash);
    for (text, expected) in [
        ("{", "not JSON"),
        (r#"{"cases": []}"#, "nonempty"),
        (
            r#"{"cases": [{"kind": "other", "sha": "a", "parent": "b"}]}"#,
            "cases[0]",
        ),
    ] {
        let error = parse_report(text, "x").err().unwrap();
        assert!(error.contains(expected), "{text}: {error}");
    }
}
