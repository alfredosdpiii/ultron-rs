//! Situational skills (session_core::situational_skills): in engineering mode, a cell that changes a file matching a
//! rule's paths ends the lane's next cell result with the rule's skill, once per lane and skill. The session test runs
//! the shipped runtime and the bundled pstack skills; the others drive the observer directly.

mod p_session_support;
mod reopen_support;

use host::file_hooks::{CellChanges, CellObserver};
use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::Op;
use session_core::situational_skills::{DEFAULT_RULES, SituationalSkills, rules};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn skill_text(name: &str) -> Option<(String, String)> {
    (name != "missing").then(|| {
        (
            format!("/skills/{name}/SKILL.md"),
            format!("# {name}\n\nThe text of {name}."),
        )
    })
}

fn changes(files: &[&str]) -> CellChanges {
    CellChanges {
        files: files.iter().map(|file| Path::new("/repo").join(file)).collect(),
        checked: Vec::new(),
        deleted: Vec::new(),
        complete: true,
    }
}

fn observer(settings: Value, on: Arc<AtomicBool>) -> SituationalSkills {
    SituationalSkills::new(
        rules(&settings),
        Box::new(skill_text),
        Box::new(move || on.load(Ordering::SeqCst)),
    )
}

#[test]
fn session_core_situational_skills_rules_come_from_the_settings() {
    let names = |settings: Value| rules(&settings).into_iter().map(|rule| rule.skill).collect::<Vec<_>>();
    let defaults: Vec<String> = DEFAULT_RULES.iter().map(|(skill, _, _)| skill.to_string()).collect();
    assert_eq!(names(json!({})), defaults, "no setting: the defaults");
    assert!(names(json!({"situationalSkills": false})).is_empty());
    assert!(names(json!({"situationalSkills": []})).is_empty());
    assert_eq!(
        names(json!({"alwaysSkills": ["principle-minimize-reader-load"]})),
        [
            "principle-test-behavior-not-implementation",
            "typescript-best-practices",
            "principle-make-operations-idempotent",
            "principle-explain-the-number",
            "principle-boundary-discipline",
            "principle-type-system-discipline"
        ],
        "an always-on skill is in the prompt already"
    );
    assert_eq!(
        names(json!({"situationalSkills": [
            {"skill": "principle-explain-the-number", "paths": ["bench/**"], "situation": "You changed a benchmark"},
            {"skill": "no-paths"},
            {"skill": "bad-path", "paths": ["/abs/**"]},
            {"paths": ["x/**"]},
        ]})),
        ["principle-explain-the-number"],
        "a rule needs a skill and a valid path"
    );
}

#[test]
fn session_core_situational_skills_give_each_skill_once_per_lane() {
    let on = Arc::new(AtomicBool::new(true));
    let skills = observer(json!({}), on.clone());
    let cwd = Path::new("/repo");
    assert!(skills.enabled());
    assert!(skills.cell_changed(&changes(&["src/app.py"]), cwd, "main").is_empty());
    let given = skills.cell_changed(&changes(&["src/app.py", "tests/test_app.py", "README.md"]), cwd, "main");
    assert_eq!(
        given,
        [
            "[skill] You changed a test file (tests/test_app.py): principle-test-behavior-not-implementation applies \
          from here on.\n# principle-test-behavior-not-implementation\n\nThe text of \
          principle-test-behavior-not-implementation.",
            "[skill] You changed 3 files: rigor applies from here on.\n# rigor\n\nThe text of rigor."
        ],
        "one path skill per cell, the first matching rule's, and the playbook at the third file"
    );
    let next = skills.cell_changed(&changes(&["tests/test_other.py", "docs/guide.md"]), cwd, "main");
    assert_eq!(next.len(), 1);
    assert!(
        next[0].starts_with("[skill] You changed documentation (docs/guide.md): principle-minimize-reader-load"),
        "the test skill is not given twice; the documentation one waited for its next change: {next:?}"
    );
    assert!(
        skills
            .cell_changed(&changes(&["tests/test_app.py"]), cwd, "main")
            .is_empty()
    );
    let child = skills.cell_changed(&changes(&["tests/test_app.py"]), cwd, "child");
    assert_eq!(child.len(), 1, "another lane gets it too");
    on.store(false, Ordering::SeqCst);
    assert!(!skills.enabled(), "off with engineering mode");
}

#[test]
fn session_core_situational_skills_bring_the_playbook_at_the_third_file_and_the_check_before_shipping() {
    let skills = observer(json!({}), Arc::new(AtomicBool::new(true)));
    let cwd = Path::new("/repo");
    let rigor = |given: &[String]| given.iter().any(|line| line.contains("rigor applies"));
    assert!(!rigor(&skills.cell_changed(&changes(&["a.txt", "b.txt"]), cwd, "main")));
    assert!(
        !rigor(&skills.cell_changed(&changes(&["a.txt"]), cwd, "main")),
        "the same file counts once"
    );
    assert!(
        !rigor(&skills.cell_changed(&changes(&["c.txt"]), cwd, "other")),
        "files count per lane"
    );
    assert!(rigor(&skills.cell_changed(&changes(&["c.txt"]), cwd, "main")));
    assert!(!rigor(&skills.cell_changed(&changes(&["d.txt"]), cwd, "main")), "once");

    assert!(skills.before_shell("git status && git diff", "main").is_empty());
    assert!(skills.before_shell("cargo test", "main").is_empty());
    for command in [
        "git commit -qm x",
        "git -C ../wt push origin main",
        "gh pr create --fill",
    ] {
        let fresh = observer(json!({}), Arc::new(AtomicBool::new(true)));
        assert_eq!(
            fresh.before_shell(command, "main"),
            [
                "[skill] You commit or push: blast-radius applies before the change ships.\n# blast-radius\n\n\
              The text of blast-radius."
            ],
            "{command}"
        );
        assert!(fresh.before_shell(command, "main").is_empty(), "once: {command}");
    }
}

#[test]
fn session_core_situational_skills_name_a_long_or_missing_skill_differently() {
    let settings = json!({"situationalSkills": [
        {"skill": "missing", "paths": ["**/*.rs"]},
        {"skill": "long", "paths": ["**/*.rs"], "situation": "You changed Rust"},
    ]});
    let skills = SituationalSkills::new(
        rules(&settings),
        Box::new(|name: &str| skill_text(name).map(|(file, _)| (file, "x".repeat(4001)))),
        Box::new(|| true),
    );
    assert_eq!(
        skills.cell_changed(&changes(&["src/lib.rs"]), Path::new("/repo"), "main"),
        ["[skill] You changed Rust (src/lib.rs): long applies from here on. Read /skills/long/SKILL.md and follow it."],
        "a skill that is not loaded is skipped; a long one is named with its file"
    );
}

const WRITE: &str = "from pathlib import Path\nPath('tests').mkdir(exist_ok=True)\nPath('tests/test_totals.py').write_text('def test_total():\\n    assert 1 + 1 == 2\\n')\nprint('WROTE')";

fn run_session(engineering: bool, cells: usize) -> Vec<String> {
    let home = Home::new(Some(json!({"engineering": engineering})));
    let w = home.open(
        Box::new(move |request, _| {
            let results: Vec<String> = tool_results(request).iter().map(Value::to_string).collect();
            if results.is_empty() {
                return tool("toolu_0", WRITE);
            }
            // The check runs in the background after the writing cell; later cells run until one carries the skill
            // (a hang guard of `cells` cells, never a timing expectation), then two more.
            let at = results.iter().position(|r| r.contains("[skill] "));
            if at.is_some_and(|at| results.len() > at + 2) || results.len() > cells {
                return text("done");
            }
            tool(&format!("toolu_{}", results.len()), "print('NEXT')")
        }),
        &[],
        None,
    );
    w.call(Op::PromptAndWait {
        message: "write a test".into(),
        images: Vec::new(),
    })
    .unwrap();
    let requests = w.requests();
    w.close();
    tool_results(requests.last().unwrap())
        .iter()
        .map(Value::to_string)
        .collect()
}

#[test]
fn session_core_situational_skills_follow_a_test_file_change_in_engineering_mode() {
    let results = run_session(true, 40);
    assert!(results[0].contains("WROTE"), "{}", results[0]);
    let carrying: Vec<&String> = results.iter().filter(|r| r.contains("[skill] ")).collect();
    assert_eq!(carrying.len(), 1, "once: {results:?}");
    assert!(
        carrying[0].contains(
            "[skill] You changed a test file (tests/test_totals.py): principle-test-behavior-not-implementation \
             applies from here on."
        ),
        "{}",
        carrying[0]
    );
    assert!(
        carrying[0].contains("# Test Behavior, Not Implementation"),
        "the bundled skill's text, whole: {}",
        carrying[0]
    );
    assert!(
        !carrying[0].contains("disable-model-invocation"),
        "without its frontmatter"
    );
}

#[test]
fn session_core_situational_skills_stay_quiet_outside_engineering_mode() {
    // Off, no cell is even compared with the directory: nothing can arrive late.
    let results = run_session(false, 5);
    assert!(results.len() > 5, "ran to the hang guard: {}", results.len());
    assert!(!results.iter().any(|r| r.contains("[skill] ")), "{results:?}");
}

/// A file a cell changed in a sibling worktree, outside the session directory, matches by its path in its own
/// repository (it used to be dropped, so sessions that worked in worktrees got no skills).
#[test]
fn session_core_situational_skills_follow_a_change_in_a_sibling_repository() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let (session, sibling) = (tmp.join("session"), tmp.join("sibling"));
    std::fs::create_dir_all(sibling.join("tests")).unwrap();
    std::fs::create_dir_all(&session).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&sibling)
        .status()
        .unwrap();
    assert!(status.success());
    let file = sibling.join("tests/test_app.py");
    std::fs::write(&file, "").unwrap();
    let skills = observer(json!({}), Arc::new(AtomicBool::new(true)));
    let given = skills.cell_changed(
        &CellChanges {
            checked: vec![file],
            complete: true,
            ..Default::default()
        },
        &session,
        "main",
    );
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(
        given[0].contains("(tests/test_app.py): principle-test-behavior-not-implementation"),
        "{given:?}"
    );
}
