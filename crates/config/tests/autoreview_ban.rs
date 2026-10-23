//! `autoreview.ban` (Rust only) in `SettingsManager::autoreview_settings`: read from the global settings only, so a
//! repository under review cannot ban or unban anyone; keys and logins trimmed, anything but lists of logins dropped.

use std::fs;

use config::settings::SettingsManager;
use serde_json::json;

#[test]
fn config_autoreview_ban_is_global_only_and_cleaned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (agent, project) = (dir.path().join("agent"), dir.path().join("project"));
    fs::create_dir_all(&agent).expect("agent");
    fs::create_dir_all(project.join(".pi")).expect("project");
    let project_settings = json!({ "autoreview": { "ban": { "*": ["victim"], "bryandlp-boost": ["friend"] } } });
    fs::write(project.join(".pi/settings.json"), project_settings.to_string()).expect("project settings");

    // Only the project sets it: nothing is banned, even when the project is trusted.
    let trusted = SettingsManager::create(&project, &agent, true);
    assert_eq!(trusted.project_settings()["autoreview"], project_settings["autoreview"]);
    assert_eq!(trusted.autoreview_settings().get("ban"), None);

    let global = json!({ "autoreview": { "ban": {
        " bryandlp-boost ": [" lepmoran-boostkh ", "", 3, "@x"],
        "*": "not a list",
        "": ["nobody"],
        "empty": [],
        "github.com/work": ["intern"],
    } } });
    fs::write(agent.join("settings.json"), global.to_string()).expect("global settings");
    let trusted = SettingsManager::create(&project, &agent, true);
    assert_eq!(
        trusted.autoreview_settings().get("ban"),
        Some(&json!({ "bryandlp-boost": ["lepmoran-boostkh", "@x"], "github.com/work": ["intern"] }))
    );
}
