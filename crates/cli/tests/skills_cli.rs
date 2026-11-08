//! `ultron skills proposals|show|approve|reject` (crate::skills_cli): procedure skills proposed by dreaming or a
//! session wait in the agent directory until the user publishes one into the user's skills. New in Rust: literal
//! expectations.

use delegation::procedure::proposals;
use serde_json::json;
use std::path::Path;
use std::process::{Command, Output};

fn ultron(home: &Path, args: &[&str]) -> Output {
    ultron_with(home, args, &[])
}

fn ultron_with(home: &Path, args: &[&str], vars: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env(config::paths::ENV_AGENT_DIR, home.join("agent"))
        .env(config::paths::ENV_SERVER_DIR, home.join("server"))
        .envs(vars.iter().copied())
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const SKILL: &str = "---\nname: flaky\ndescription: Re-run a flaky test alone.\nprocedure:\n  segments:\n    - name: rerun\n      goal: Re-run it.\n      check: \"true\"\n---\n\n# flaky\n";

#[test]
fn cli_skills_lists_shows_and_publishes_a_proposal_only_on_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let empty = ultron(home, &["skills", "proposals"]);
    assert_eq!(
        (empty.status.code(), stdout(&empty)),
        (Some(0), "No skill proposals.\n".to_string())
    );
    let paths = config::paths::AgentPaths::new(home.join("agent"));
    let evidence = json!([{"ref": "session://s1#a..b"}]);
    let kept = proposals::propose(&paths.skill_proposals(), "flaky", SKILL, &evidence, Some("s1"), 1).unwrap();
    let id = kept["id"].as_str().unwrap();
    let listed = ultron(home, &["skills", "proposals"]);
    assert_eq!(stdout(&listed), format!("{id}\tproposed\tflaky\t1\t1\n"));
    let shown = stdout(&ultron(home, &["skills", "show", id]));
    assert!(
        shown.contains("\"session\": \"s1\"") && shown.ends_with("# flaky\n\n"),
        "{shown}"
    );
    assert!(!paths.skills_dir().join("flaky/SKILL.md").exists());
    // From a cell (the kernel's marker), the model cannot approve or reject its own proposal; it can still look.
    let in_kernel = [(kernel::env::IN_KERNEL_ENV, "1")];
    for verb in ["approve", "reject"] {
        let refused = ultron_with(home, &["skills", verb, id], &in_kernel);
        assert_eq!(
            (
                refused.status.code(),
                String::from_utf8_lossy(&refused.stderr).into_owned()
            ),
            (
                Some(1),
                format!("Error: skills {verb} is the user's decision; it does not run inside an Ultron kernel.\n")
            )
        );
    }
    assert_eq!(
        stdout(&ultron_with(home, &["skills", "proposals"], &in_kernel)),
        format!("{id}\tproposed\tflaky\t1\t1\n")
    );
    assert!(!paths.skills_dir().join("flaky/SKILL.md").exists());
    let approved = ultron(home, &["skills", "approve", id]);
    let published = paths.skills_dir().join("flaky/SKILL.md");
    assert_eq!(
        (approved.status.code(), stdout(&approved)),
        (Some(0), format!("Published {}\n", published.display()))
    );
    assert_eq!(std::fs::read_to_string(&published).unwrap(), SKILL);
    let again = ultron(home, &["skills", "reject", id, "too", "late"]);
    assert_eq!(
        (again.status.code(), String::from_utf8_lossy(&again.stderr).into_owned()),
        (Some(1), format!("Error: Skill proposal {id} is published\n"))
    );
    let usage = ultron(home, &["skills", "approve"]);
    assert_eq!(usage.status.code(), Some(2));
    let help = stdout(&ultron(home, &["skills", "--help"]));
    assert!(help.starts_with("Usage: ultron skills <command>"), "{help}");
}
