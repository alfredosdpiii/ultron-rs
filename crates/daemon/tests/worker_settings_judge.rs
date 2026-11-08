//! `judge.*` from `/settings` (crate::worker_settings, Rust only; literal expectations): each key is checked, saved
//! in the global settings (never the project's) and applies to the next `judge.decide`; null clears it.

use config::settings::SettingsManager;
use daemon::worker_settings::{self, Applied};
use serde_json::{Value, json};

fn saved(agent: &std::path::Path) -> Value {
    serde_json::from_str::<Value>(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap()["judge"]
        .clone()
}

#[test]
fn daemon_worker_settings_judge_keys_are_saved_globally_and_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (tmp.path().join("agent"), tmp.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    let mut settings = SettingsManager::create(&cwd, &agent, true);
    let mut apply = |key: &str, value: Value| worker_settings::apply(&mut settings, key, &value, None);
    for (key, value) in [
        ("judge.provider", json!(["d1", "jev", "d1"])),
        ("judge.d1.url", json!(" http://mini.tail.example:18091/ ")),
        ("judge.model", json!("jev-1.13")),
        ("judge.escalateModel", json!("anthropic/claude-haiku-5-5")),
        ("judge.fallbackModel", json!("openai/gpt-5-mini")),
        ("judge.thresholds", json!({"jev": 0.8, "memory": {"d1": 0.9}})),
        ("judge.budgetUsd", json!(2.5)),
        ("judge.maxCalls", json!(200)),
    ] {
        let (applied, changes) = apply(key, value).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert_eq!((applied, changes.len()), (Applied::Live, 0), "{key}");
    }
    assert_eq!(
        saved(&agent),
        json!({"provider": ["d1", "jev"], "d1": {"url": "http://mini.tail.example:18091"}, "model": "jev-1.13",
            "escalateModel": "anthropic/claude-haiku-5-5", "fallbackModel": "openai/gpt-5-mini",
            "thresholds": {"jev": 0.8, "memory": {"d1": 0.9}}, "budgetUsd": 2.5, "maxCalls": 200})
    );
    assert!(!cwd.join(".pi/settings.json").exists());
    for (key, value, error) in [
        (
            "judge.provider",
            json!(["gpt"]),
            "judge.provider must be one of: jev, d1",
        ),
        (
            "judge.provider",
            json!("jev"),
            "judge.provider must be a list of jev, d1 (or null for both, Jev first)",
        ),
        (
            "judge.d1.url",
            json!("ftp://mini"),
            "judge.d1.url must be an http(s) URL",
        ),
        (
            "judge.escalateModel",
            json!("haiku"),
            "judge.escalateModel must be provider/model (or null to clear it)",
        ),
        (
            "judge.thresholds",
            json!({"jev": 1.5}),
            "judge.thresholds must map backends (and kinds to backends) to numbers from 0 to 1 (or null to clear it)",
        ),
        (
            "judge.budgetUsd",
            json!(0),
            "judge.budgetUsd must be a positive number of US dollars (or null to clear it)",
        ),
        (
            "judge.maxCalls",
            json!(1.5),
            "judge.maxCalls must be a positive whole number (or null to clear it)",
        ),
    ] {
        assert_eq!(apply(key, value).unwrap_err(), error);
    }
    for key in [
        "judge.provider",
        "judge.d1.url",
        "judge.model",
        "judge.escalateModel",
        "judge.fallbackModel",
        "judge.thresholds",
        "judge.budgetUsd",
        "judge.maxCalls",
    ] {
        apply(key, Value::Null).unwrap();
    }
    assert_eq!(saved(&agent), json!({}));
}
