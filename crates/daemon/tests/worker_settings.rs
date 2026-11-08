//! Replays `fixtures/worker-settings.json` (recorded by `fixtures/generate-worker-settings.mjs` from the TypeScript
//! worker-settings.ts and session-control-provider.ts) against `daemon::worker_settings`, in temp profile directories.

use config::settings::SettingsManager;
use daemon::worker_settings::{self, Live};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/worker-settings.json")).unwrap()
}

/// A case's profile and project directories with its files (as the generator lays them out).
fn dirs(root: &Path, case: usize, files: &Value) -> (PathBuf, PathBuf) {
    let base = root.join(format!("case-{case}"));
    let (agent, cwd) = (base.join("agent"), base.join("project"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    if let Some(raw) = files["raw"].as_str() {
        std::fs::write(agent.join("settings.json"), raw).unwrap();
    } else if !files["global"].is_null() {
        std::fs::write(agent.join("settings.json"), files["global"].to_string()).unwrap();
    }
    if !files["project"].is_null() {
        std::fs::write(cwd.join(".pi/settings.json"), files["project"].to_string()).unwrap();
    }
    if let Some(trust) = files["trust"].as_array() {
        let updates: Vec<config::trust::TrustUpdate> = trust
            .iter()
            .map(|t| config::trust::TrustUpdate {
                path: match t["path"].as_str().unwrap() {
                    "." => cwd.clone(),
                    ".." => base.clone(),
                    other => PathBuf::from(other),
                },
                decision: t["decision"].as_bool(),
            })
            .collect();
        config::trust::ProjectTrustStore::new(&agent)
            .set_many(&updates)
            .unwrap();
    }
    (agent, cwd)
}

fn normalized(value: Value, root: &Path) -> Value {
    let text = value.to_string().replace(&root.display().to_string(), "<root>");
    serde_json::from_str(&text).unwrap()
}

fn env_of(case: &Value) -> HashMap<String, String> {
    case.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Rust-only (wave 47): a claude-code sub-agent model is kept (its tool lanes run on the Claude subscription), where
/// TypeScript refused or dropped it. Whether the fixture's `key = value` is such a model.
fn claude_code_child(key: &str, value: &Value) -> bool {
    matches!(key, "rlm.childModel" | "claudeCode.ultronChildModel")
        && value.as_str().is_some_and(|v| v.trim().starts_with("claude-code/"))
}

#[test]
fn daemon_worker_settings_read_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    for (index, case) in fixture()["reads"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let (agent, cwd) = dirs(&root, index + 1, &case["files"]);
        let settings = SettingsManager::create(&cwd, &agent, case["projectTrusted"] == true);
        let live = Live {
            compaction: case["live"]["compaction"] == true,
            retry: case["live"]["retry"] == true,
            steering_mode: case["live"]["steeringMode"].as_str().unwrap().into(),
            follow_up_mode: case["live"]["followUpMode"].as_str().unwrap().into(),
        };
        let env = env_of(&case["env"]);
        let read = worker_settings::read(&settings, &live, &cwd, &agent, &|name| env.get(name).cloned()).unwrap();
        let mut expected = case["read"].clone();
        let child = &case["files"]["global"]["rlm"]["childModel"];
        if claude_code_child("rlm.childModel", child) {
            expected["values"]["rlm.childModel"] = json!(child.as_str().unwrap().trim());
        }
        assert_eq!(normalized(read, &root), expected, "{name}");
    }
}

#[test]
fn daemon_worker_settings_apply_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    for (index, case) in fixture()["applies"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let (agent, cwd) = dirs(&root, index + 1, &case["files"]);
        let mut settings = SettingsManager::create(&cwd, &agent, false);
        let model = (
            case["model"]["provider"].as_str().unwrap(),
            case["model"]["id"].as_str().unwrap(),
        );
        for step in case["steps"].as_array().unwrap() {
            let key = step["key"].as_str().unwrap();
            let label = format!("{name}: {key} = {}", step["value"]);
            if claude_code_child(key, &step["value"]) {
                // Saved, as any provider/model; then the fixture's file (which kept the old value) is put back so its
                // later steps start where TypeScript's did.
                worker_settings::apply(&mut settings, key, &step["value"], Some(model))
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                let text = std::fs::read_to_string(agent.join("settings.json")).unwrap();
                let (section, field) = key.split_once('.').unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap()[section][field],
                    step["value"],
                    "{label}"
                );
                let previous = &step["file"][section][field];
                worker_settings::apply(&mut settings, key, previous, Some(model)).unwrap();
                let text = std::fs::read_to_string(agent.join("settings.json")).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap(),
                    step["file"],
                    "{label}: restored"
                );
                continue;
            }
            let outcome = worker_settings::apply(&mut settings, key, &step["value"], Some(model));
            match outcome {
                Ok((applied, changes)) => {
                    assert_eq!(json!({"applied": applied.name()}), step["result"], "{label}");
                    let live: Vec<String> = changes.iter().map(|c| c.describe()).collect();
                    assert_eq!(json!(live), step["live"], "{label}");
                }
                Err(error) => {
                    assert_eq!(Some(error.as_str()), step["error"].as_str(), "{label}");
                    assert_eq!(step["live"], json!([]), "{label}");
                }
            }
            let text = std::fs::read_to_string(agent.join("settings.json")).unwrap();
            let file = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
            assert_eq!(normalized(file, &root), step["file"], "{label}: settings.json");
        }
    }
}

#[test]
fn daemon_worker_settings_model_env_overrides_match_typescript() {
    for case in fixture()["envOverrides"].as_array().unwrap() {
        let env = env_of(&case["env"]);
        let overrides = worker_settings::model_env_overrides(&|name| env.get(name).cloned());
        assert_eq!(Value::Object(overrides), case["overrides"], "{}", case["env"]);
    }
}

#[test]
fn daemon_worker_settings_debug_environment_redacts_like_typescript() {
    for case in fixture()["debugEnvironments"].as_array().unwrap() {
        let env = env_of(&case["env"]);
        let shown = worker_settings::debug_environment(env);
        // Key order is part of the contract (`/debug` prints the object as is).
        let keys: Vec<&String> = shown.keys().collect();
        let expected: Vec<&String> = case["environment"].as_object().unwrap().keys().collect();
        assert_eq!(keys, expected);
        assert_eq!(Value::Object(shown), case["environment"]);
    }
}

#[test]
fn daemon_worker_settings_debug_environment_sorts_like_locale_compare() {
    // `["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"].sort((a, b) => a.localeCompare(b))` in Node.
    let vars = ["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"].map(|n| (n.to_string(), "v".to_string()));
    let shown = worker_settings::debug_environment(vars);
    let keys: Vec<&str> = shown.keys().map(String::as_str).collect();
    assert_eq!(keys, ["PI_A_B", "PI_A1", "PI_ab", "PI_Ab", "PI_AB"]);
}

#[test]
fn daemon_worker_settings_invalid_idle_timeout_prints_like_string_of_value() {
    // `String([1, null])` is "1," and `String(-1.0)` is "-1" (serde printed "[1,null]"-style "1,null" and "-1.0").
    for (setting, shown) in [(json!([1, null]), "1,"), (json!(-1.0), "-1")] {
        let tmp = tempfile::tempdir().unwrap();
        let (cwd, agent) = (tmp.path().join("cwd"), tmp.path().join("agent"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({ "httpIdleTimeoutMs": setting }).to_string(),
        )
        .unwrap();
        let settings = SettingsManager::create(&cwd, &agent, false);
        assert_eq!(
            worker_settings::http_idle_timeout_ms(&settings),
            Err(format!("Invalid httpIdleTimeoutMs setting: {shown}"))
        );
    }
}

#[test]
fn daemon_worker_settings_debug_environment_sorts_punctuation_like_icu() {
    // `["PI_A-B", "PI_A_B", "PI_A~", "PI_A$"].sort((a, b) => a.localeCompare(b))` in Node: CLDR punctuation order.
    let vars = ["PI_A-B", "PI_A_B", "PI_A~", "PI_A$"].map(|n| (n.to_string(), "v".to_string()));
    let shown = worker_settings::debug_environment(vars);
    let keys: Vec<&str> = shown.keys().map(String::as_str).collect();
    assert_eq!(keys, ["PI_A_B", "PI_A-B", "PI_A~", "PI_A$"]);
}

#[test]
fn daemon_session_command_trims_like_js_trim() {
    // `" /goal ship it\u0085 ".trim()` keeps the U+0085 (NEL is not JavaScript whitespace).
    let (name, op) = daemon::session_commands::command(" /goal ship it\u{85} ").unwrap();
    assert_eq!(name, "goal");
    assert_eq!(
        op,
        session_core::core::Op::Goal {
            args: "ship it\u{85}".into()
        }
    );
}
