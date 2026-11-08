//! Paths no cell may write (`FileHooks::protect`): the profile's settings, keys and skills are the user's. A
//! `write()`/`edit()` into one, or a shell command that may write files and names one, is blocked before any guard
//! runs and recorded; everything else goes on as before. New in Rust: literal expectations.

use host::Guard;
use host::file_hooks::{FileHooks, GuardRecord};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[test]
fn host_a_cell_cannot_write_the_profiles_protected_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (home, cwd) = (root.join("home"), root.join("work"));
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(agent.join("skills")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    // The agent directory through a symlink in the project is still the agent directory.
    std::os::unix::fs::symlink(&agent, cwd.join("profile")).unwrap();
    let hooks = FileHooks::new(&cwd);
    let records: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = records.clone();
    hooks.on_record(Arc::new(move |record: &GuardRecord| {
        sink.lock().unwrap().push(record.to_json())
    }));
    let protected = ["settings.json", "auth.json", "skills", "skill-proposals"].map(|name| agent.join(name));
    hooks.protect(protected.to_vec(), &home);
    let settings = agent.join("settings.json").display().to_string();
    let refusal = |path: &std::path::Path| {
        format!(
            "[Ultron] {} is the user's to change (the profile's settings, keys and skills): a cell may not write it. \
             Ask the user to make the change.",
            path.display()
        )
    };
    for target in [
        settings.clone(),
        agent.join("auth.json").display().to_string(),
        agent.join("skills/flaky/SKILL.md").display().to_string(),
        "profile/skill-proposals/x/SKILL.md".to_string(),
    ] {
        let answer = hooks.before_write("root", &target, "x");
        assert_eq!(answer["blocked"], true, "{target}: {answer}");
    }
    assert_eq!(
        hooks.before_write("root", &settings, "{}")["reason"],
        refusal(&agent.join("settings.json"))
    );
    assert_eq!(hooks.before_write("root", "src/app.py", "x = 1\n")["blocked"], false);
    assert_eq!(
        hooks.before_write("root", &agent.join("sessions/s.jsonl").display().to_string(), "{}")["blocked"],
        false
    );
    // A shell command that may write and names a protected path, in any of the forms a shell reaches it by.
    for command in [
        "echo '{}' > ~/.ultron/agent/settings.json",
        "cp evil.md $HOME/.ultron/agent/skills/x/SKILL.md",
        &format!("tee {settings} < /dev/stdin"),
    ] {
        assert_eq!(
            hooks.before_shell("root", command),
            Some(refusal(&agent.join(if command.contains("skills") {
                "skills"
            } else {
                "settings.json"
            }))),
            "{command}"
        );
    }
    assert_eq!(hooks.before_shell("root", "cat ~/.ultron/agent/settings.json"), None);
    assert_eq!(hooks.before_shell("root", "echo hi > notes.txt"), None);
    let records = records.lock().unwrap();
    assert!(
        records
            .iter()
            .all(|r| r["guard"] == "Ultron" && r["outcome"] == "blocked"),
        "{records:?}"
    );
    assert_eq!(records.len(), 8);
}
