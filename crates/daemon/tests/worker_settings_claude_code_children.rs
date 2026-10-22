//! `/settings` saves a claude-code model as a sub-agent model (crate::worker_settings; user requirement, literal
//! expectations): `rlm.childModel` and `claudeCode.ultronChildModel` accept `claude-code/<id>`, whose tool lanes run
//! on the Claude subscription, and the worker reports `rlm.childModel` back as saved.

use config::settings::SettingsManager;
use daemon::worker_settings::{self, Live};
use serde_json::{Value, json};

fn live() -> Live {
    Live {
        compaction: true,
        retry: true,
        steering_mode: "one-at-a-time".into(),
        follow_up_mode: "one-at-a-time".into(),
    }
}

#[test]
fn daemon_worker_settings_save_claude_code_sub_agent_models() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (tmp.path().join("agent"), tmp.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let mut settings = SettingsManager::create(&cwd, &agent, false);
    for key in ["rlm.childModel", "claudeCode.ultronChildModel"] {
        worker_settings::apply(&mut settings, key, &json!("claude-code/claude-sonnet-5-5"), None)
            .unwrap_or_else(|e| panic!("{key}: {e}"));
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        file,
        json!({
            "rlm": {"childModel": "claude-code/claude-sonnet-5-5"},
            "claudeCode": {"ultronChildModel": "claude-code/claude-sonnet-5-5"},
        })
    );
    let values = worker_settings::values(&settings, &live(), &|_: &str| None).unwrap();
    assert_eq!(values["rlm.childModel"], "claude-code/claude-sonnet-5-5");
    assert_eq!(
        settings.rlm_model_settings().child_model.as_deref(),
        Some("claude-code/claude-sonnet-5-5")
    );
}
