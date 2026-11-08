//! Versioned variants, appended points, written proposals, and the Pareto frontier.

use optimize::{Outcome, Point, Store, frontier};
use serde_json::json;

fn point(variant: &str, quality: f64, penalty: f64, cost: Option<f64>, outcome: Outcome) -> Point {
    Point {
        pipeline: "judge",
        variant: variant.into(),
        version: 1,
        set_hash: "set".into(),
        mode: "offline",
        outcome,
        quality,
        penalty,
        cost_usd: cost,
        metrics: json!({"quality": "q", "penalty": "p"}),
        failures: Vec::new(),
    }
}

#[test]
fn a_changed_config_is_a_new_version_and_old_versions_stay() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("optimize"));
    let at = "2026-11-08T00:00:00.000Z";
    assert_eq!(
        store
            .variant("judge", "jev", &json!({"provider": ["jev"]}), at)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .variant("judge", "jev", &json!({"provider": ["jev"]}), at)
            .unwrap(),
        1
    );
    let fitted = json!({"provider": ["jev"], "thresholds": {"jev": 0.8}});
    assert_eq!(store.variant("judge", "jev", &fitted, at).unwrap(), 2);
    assert_eq!(store.variant("judge", "d1", &json!({}), at).unwrap(), 1);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("optimize/judge/variants.json")).unwrap())
            .unwrap();
    assert_eq!(saved["jev"].as_array().unwrap().len(), 2);
    assert_eq!(saved["jev"][1]["config"], fitted);
    let error = store.variant("judge", "Bad Name", &json!({}), at).err().unwrap();
    assert!(error.contains("lowercase"), "{error}");
}

#[test]
fn points_are_appended_with_their_failures_and_documents_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    assert!(store.points("judge").unwrap().is_empty());
    let mut failed = point("jev", 0.9, 0.0, None, Outcome::Failed);
    failed
        .failures
        .push(json!({"case": 3, "failure": "accepted a wrong answer"}));
    store.record(&failed, "t1").unwrap();
    store
        .record(&point("d1", 0.8, 0.1, Some(0.0), Outcome::Passed), "t2")
        .unwrap();
    let points = store.points("judge").unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(points[0]["variant"], "jev@v1");
    assert_eq!(points[0]["outcome"], "failed");
    assert_eq!(points[0]["fixtureHash"], "set");
    assert_eq!(points[0]["costUsd"], serde_json::Value::Null);
    assert_eq!(points[0]["failures"][0]["case"], 3);
    assert_eq!(points[1]["recordedAt"], "t2");
    let path = store.write("judge", "proposals", "123", &json!({"x": 1})).unwrap();
    assert!(path.ends_with("judge/proposals/123.json"));
}

#[test]
fn the_frontier_skips_failed_and_dominated_points() {
    let points = [
        point("cheap", 0.80, 0.10, Some(0.0), Outcome::Passed),
        point("better", 0.90, 0.10, Some(0.5), Outcome::Passed),
        point("dominated", 0.80, 0.20, Some(0.5), Outcome::Passed),
        point("wrong", 0.99, 0.00, Some(0.0), Outcome::Failed),
        point("unpriced", 0.95, 0.15, None, Outcome::Incomplete),
    ];
    assert_eq!(frontier(&points), [true, true, false, false, true]);
    // An unknown cost is neither better nor worse: a point no worse in quality and penalty still dominates it.
    let points = [
        point("unpriced", 0.80, 0.10, None, Outcome::Passed),
        point("free", 0.85, 0.10, Some(0.0), Outcome::Passed),
    ];
    assert_eq!(frontier(&points), [false, true]);
    // Points of another set never compare.
    let mut other = point("other", 0.1, 0.9, Some(9.0), Outcome::Passed);
    other.set_hash = "another".into();
    assert_eq!(
        frontier(&[point("a", 0.9, 0.0, Some(0.0), Outcome::Passed), other]),
        [true, true]
    );
    assert!(points[1].line(true).starts_with("* free@v1"));
}
