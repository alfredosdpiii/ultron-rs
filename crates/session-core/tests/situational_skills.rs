//! Situational skills (session_core::situational_skills): in engineering mode, a cell that changes a file matching a
//! rule's paths ends the lane's next cell result with the rule's skill, once per lane and skill. The session test runs
//! the shipped runtime and the bundled pstack skills; the others drive the observer directly.

use crate::p_session_support;
use crate::reopen_support;

use host::file_hooks::{CellChanges, CellObserver};
use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::Op;
use session_core::situational_skills::{SelfReview, SituationalSkills, rules, self_review};
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

/// Files a cell wrote with `edit()` or `write()`.
fn changes(files: &[&str]) -> CellChanges {
    CellChanges {
        files: Vec::new(),
        checked: files.iter().map(|file| Path::new("/repo").join(file)).collect(),
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
    assert_eq!(
        names(json!({})),
        [
            "principle-test-behavior-not-implementation",
            "principle-minimize-reader-load",
            "typescript-best-practices",
            "principle-make-operations-idempotent",
            "principle-explain-the-number",
            "principle-boundary-discipline",
            "principle-type-system-discipline",
            "maintain-formal-verification-skill"
        ],
        "no setting: the defaults"
    );
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
            "principle-type-system-discipline",
            "maintain-formal-verification-skill"
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

/// The after-cell comparison covers the whole session directory, where other sessions, builds and checkouts write
/// too: a change the host did not make is not known to be the cell's.
#[test]
fn session_core_situational_skills_follow_only_files_written_with_edit_or_write() {
    let skills = observer(json!({}), Arc::new(AtomicBool::new(true)));
    let cwd = Path::new("/repo");
    let elsewhere = CellChanges {
        files: [
            "target/debug/build/sys/out/bindgen.rs",
            "other/tests/gate.rs",
            "other/README.md",
        ]
        .iter()
        .map(|file| cwd.join(file))
        .collect(),
        complete: true,
        ..Default::default()
    };
    assert_eq!(skills.cell_changed(&elsewhere, cwd, "main"), Vec::<String>::new());
    let given = skills.cell_changed(&changes(&["tests/test_app.py"]), cwd, "main");
    assert_eq!(
        given.len(),
        1,
        "those files neither used up the test skill nor counted toward the playbook: {given:?}"
    );
    assert!(
        given[0].starts_with("[skill] You changed a test file (tests/test_app.py)"),
        "{given:?}"
    );
}

#[test]
fn session_core_situational_skills_bring_the_playbook_at_the_second_file_and_the_check_before_shipping() {
    let skills = observer(json!({}), Arc::new(AtomicBool::new(true)));
    let cwd = Path::new("/repo");
    let rigor = |given: &[String]| given.iter().any(|line| line.contains("rigor applies"));
    assert!(!rigor(&skills.cell_changed(&changes(&["a.txt"]), cwd, "main")));
    assert!(
        !rigor(&skills.cell_changed(&changes(&["a.txt"]), cwd, "main")),
        "the same file counts once"
    );
    assert!(
        !rigor(&skills.cell_changed(&changes(&["b.txt"]), cwd, "other")),
        "files count per lane"
    );
    assert!(rigor(&skills.cell_changed(&changes(&["b.txt"]), cwd, "main")));
    assert!(!rigor(&skills.cell_changed(&changes(&["c.txt"]), cwd, "main")), "once");

    assert_eq!(skills.before_shell("git status && git diff", "main"), None);
    assert_eq!(skills.before_shell("cargo test", "main"), None);
    for command in [
        "git commit -qm x",
        "git -C ../wt push origin main",
        "gh pr create --fill",
    ] {
        let fresh = observer(json!({}), Arc::new(AtomicBool::new(true)));
        let refused = fresh.before_shell(command, "main").unwrap_or_default();
        assert!(
            refused.starts_with("blast-radius comes first") && refused.contains("The text of blast-radius."),
            "{command}: {refused}"
        );
        assert_eq!(fresh.before_shell(command, "main"), None, "refused once: {command}");
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn session_core_self_review_refuses_a_pull_request_until_the_committed_tree_is_reviewed() {
    assert_eq!(self_review(&json!({})), SelfReview::Require);
    assert_eq!(self_review(&json!({"selfReview": "advise"})), SelfReview::Advise);
    assert_eq!(self_review(&json!({"selfReview": "off"})), SelfReview::Off);

    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-qm", "one"]);
    let receipt = dir.join(".git/ultron-review/receipt.json");
    std::fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    let write = |tree: &str, confirmed: u64, incomplete: bool, waived: Value| {
        let body = json!({"tree": tree, "confirmed": confirmed, "incomplete": incomplete, "waived": waived});
        std::fs::write(&receipt, body.to_string()).unwrap();
    };
    let gate = |mode| {
        let skills = observer(json!({}), Arc::new(AtomicBool::new(true))).with_self_review(mode, dir);
        // The reminder, once; what follows is the receipt's answer.
        assert!(skills.before_shell("git commit -qm x", "main").is_some());
        skills
    };
    // The description `spec_api.description` wrote for the committed tree, as the pull request's body.
    let describe = |tree: &str| {
        let body = format!("<!-- ultron-description tree={tree} -->\n## Why\n\nx\n");
        std::fs::write(dir.join(".git/ultron-review/description.md"), body).unwrap();
    };
    let create = "gh pr create --title t --body-file .git/ultron-review/description.md";
    let skills = gate(SelfReview::Require);

    let refused = skills.before_shell(create, "main").unwrap_or_default();
    assert!(refused.contains("has not been reviewed"), "{refused}");
    assert!(
        skills.before_shell(create, "main").is_some(),
        "refused until reviewed, not once"
    );
    assert_eq!(
        skills.before_shell("git push origin main", "main"),
        None,
        "a push stays advisory"
    );
    assert_eq!(
        skills.before_shell("rg -n 'gh pr create' docs", "main"),
        None,
        "quoted text ships nothing"
    );

    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]);
    write(&tree, 0, false, Value::Null);
    let refused = skills.before_shell(create, "main").unwrap_or_default();
    assert!(
        refused.contains("spec_api.description"),
        "reviewed, not yet described: {refused}"
    );
    describe("0000");
    assert!(
        skills.before_shell(create, "main").is_some(),
        "a description of another tree is stale"
    );
    describe(&tree);
    let filled = skills.before_shell("gh pr create --fill", "main").unwrap_or_default();
    assert!(
        filled.contains("--body-file"),
        "the body is that file, not the commit messages: {filled}"
    );
    assert_eq!(skills.before_shell(create, "main"), None, "reviewed and described");
    let quoted = "gh pr create -t t -F '.git/ultron-review/description.md'";
    assert_eq!(skills.before_shell(quoted, "main"), None, "-F, quoted");

    write(&tree, 2, false, Value::Null);
    let refused = skills.before_shell(create, "main").unwrap_or_default();
    assert!(refused.contains("confirmed 2 finding(s)"), "{refused}");
    write(&tree, 2, false, json!("  "));
    assert!(
        skills.before_shell(create, "main").is_some(),
        "an empty reason waives nothing"
    );
    write(&tree, 2, false, json!("both are follow-ups, listed under Scope"));
    assert_eq!(skills.before_shell(create, "main"), None, "waived with a reason");
    let round = |round: u64| {
        let body = json!({"tree": tree, "confirmed": 2, "incomplete": false, "waived": null, "round": round});
        std::fs::write(&receipt, body.to_string()).unwrap();
        skills.before_shell(create, "main").unwrap_or_default()
    };
    let first = round(1);
    assert!(first.contains("review again"), "{first}");
    let second = round(2);
    assert!(
        second.contains("review rounds are used up")
            && second.contains("review_api.waive")
            && !second.contains("review again"),
        "a second round with findings ships them, waived: {second}"
    );

    write(&tree, 0, true, Value::Null);
    let refused = skills.before_shell(create, "main").unwrap_or_default();
    assert!(
        refused.contains("did not finish"),
        "an unfinished review is not a pass: {refused}"
    );
    std::fs::write(&receipt, "not json").unwrap();
    assert!(
        skills.before_shell(create, "main").is_some(),
        "an unreadable receipt refuses"
    );

    write(&tree, 0, false, Value::Null);
    std::fs::write(dir.join("a.txt"), "two\n").unwrap();
    git(dir, &["commit", "-qam", "two"]);
    let refused = skills.before_shell(create, "main").unwrap_or_default();
    assert!(
        refused.contains("has not been reviewed"),
        "a new commit needs a new review: {refused}"
    );
    let refused = skills.before_shell("git push origin main", "main").unwrap_or_default();
    assert!(
        refused.contains("gh pr edit --body-file"),
        "a push of another tree than the one described says so: {refused}"
    );
    assert_eq!(
        skills.before_shell("git push origin main", "main"),
        None,
        "once per tree"
    );

    // The pull request is opened where the command goes, not where the session started.
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    let elsewhere = format!("cd {} && gh pr create --fill", other.path().display());
    let refused = skills.before_shell(&elsewhere, "main").unwrap_or_default();
    assert!(refused.contains("no commit to review"), "{refused}");

    assert_eq!(
        gate(SelfReview::Advise).before_shell(create, "main"),
        None,
        "advise: the reminder only"
    );
    let off = observer(json!({}), Arc::new(AtomicBool::new(true))).with_self_review(SelfReview::Off, dir);
    assert_eq!(off.before_shell(create, "main"), None, "off: nothing is refused");
}

#[test]
fn session_core_situational_skills_bring_the_playbooks_section_of_rigor() {
    let skills = SituationalSkills::new(
        rules(&json!({})),
        Box::new(|name: &str| {
            (name == "rigor").then(|| {
                (
                    "/skills/rigor/SKILL.md".to_string(),
                    format!(
                        "# Rigor\n\n{}\n\n## Playbooks\n\n- **Bug fix.** Reproduce.\n\n## After\n\nx",
                        "i".repeat(5000)
                    ),
                )
            })
        }),
        Box::new(|| true),
    );
    let given = skills.cell_changed(&changes(&["a.txt", "b.txt", "c.txt"]), Path::new("/repo"), "main");
    assert_eq!(
        given,
        [
            "[skill] You changed 3 files: rigor applies from here on.\n## Playbooks\n\n- **Bug fix.** Reproduce.\n\n\
          [Paths are relative to /skills/rigor. Pick the playbook now and read it before the next edit; the whole skill \
          is /skills/rigor/SKILL.md.]"
        ]
    );
}

#[test]
fn session_core_situational_skills_excerpt_a_long_skill_and_skip_a_missing_one() {
    let settings = json!({"situationalSkills": [
        {"skill": "missing", "paths": ["**/*.rs"]},
        {"skill": "long", "paths": ["**/*.rs"], "situation": "You changed Rust"},
    ]});
    let long = format!(
        "# Long\n\nRead the request, pick a playbook.\n\n## Rules\n\n{}\n\n## Playbooks\n\n{}\n\n## Comments\n\nnone",
        "r".repeat(3000),
        "p".repeat(3000)
    );
    let skills = SituationalSkills::new(
        rules(&settings),
        Box::new(move |name: &str| skill_text(name).map(|(file, _)| (file, long.clone()))),
        Box::new(|| true),
    );
    let given = skills.cell_changed(&changes(&["src/lib.rs"]), Path::new("/repo"), "main");
    assert_eq!(given.len(), 1, "a skill that is not loaded is skipped: {given:?}");
    let text = &given[0];
    assert!(
        text.starts_with(
            "[skill] You changed Rust (src/lib.rs): long applies from here on.\n# Long\n\nRead the request"
        )
    );
    assert!(text.contains(&"r".repeat(3000)), "whole paragraphs up to the bound");
    assert!(
        !text.contains('p'.to_string().repeat(10).as_str()),
        "not the paragraph past it"
    );
    assert!(
        text.ends_with("[The rest, Playbooks · Comments, is in /skills/long/SKILL.md: read it before you act on it.]"),
        "{}",
        &text[text.len() - 120..]
    );
}

const WRITE: &str =
    "await write('tests/test_totals.py', 'def test_total():\\n    assert 1 + 1 == 2\\n')\nprint('WROTE')";
/// A change the host did not make: to it, the same as another session writing in the directory.
const ELSEWHERE: &str = "from pathlib import Path\nPath('tests').mkdir(exist_ok=True)\nPath('tests/test_elsewhere.py').write_text('')\nprint('ELSEWHERE')";

/// A session whose first cells are `first`, in order.
fn run_session(first: &'static [&'static str], engineering: bool, cells: usize) -> Vec<String> {
    let home = Home::new(Some(json!({"engineering": engineering})));
    let w = home.open(
        Box::new(move |request, _| {
            let results: Vec<String> = tool_results(request).iter().map(Value::to_string).collect();
            if let Some(code) = first.get(results.len()) {
                return tool(&format!("toolu_{}", results.len()), code);
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
    let results = run_session(&[WRITE], true, 40);
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
fn session_core_situational_skills_name_the_file_the_cell_wrote_not_one_changed_by_other_means() {
    let results = run_session(&[ELSEWHERE, WRITE], true, 40);
    assert!(
        results[0].contains("ELSEWHERE") && results[1].contains("WROTE"),
        "{results:?}"
    );
    let carrying: Vec<&String> = results.iter().filter(|r| r.contains("[skill] ")).collect();
    assert_eq!(carrying.len(), 1, "once: {results:?}");
    assert!(
        carrying[0].contains("[skill] You changed a test file (tests/test_totals.py)"),
        "{}",
        carrying[0]
    );
}

#[test]
fn session_core_situational_skills_stay_quiet_outside_engineering_mode() {
    // Off, no cell is even compared with the directory: nothing can arrive late.
    let results = run_session(&[WRITE], false, 5);
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

#[test]
fn session_core_first_ship_of_a_lane_carries_hygiene_notes_for_the_whole_change() {
    use loki::hygiene::{self, Level};
    // Without a Python that has NumPy the model cannot run, as in Loki's own hygiene tests.
    if hygiene::score(&["x = 1\n"], loki::proc::Deadline::after(60.0)).is_err() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("store.py"), "LIMIT = 10\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "base"]);
    let swallowed =
        "def save(path, text):\n    try:\n        open(path, 'w').write(text)\n    except Exception:\n        pass\n";
    std::fs::write(dir.join("store.py"), format!("LIMIT = 10\n\n\n{swallowed}")).unwrap();
    let skills = |level| {
        observer(json!({}), Arc::new(AtomicBool::new(true)))
            .with_self_review(SelfReview::Advise, dir)
            .with_hygiene(level)
    };
    let on = skills(Level::On);
    let told = on.before_shell("git commit -qm x", "main").unwrap_or_default();
    assert!(told.contains("store.py:7: loki/hygiene-swallowed-error"), "{told}");
    assert_eq!(on.before_shell("git commit -qm x", "main"), None, "said once per lane");
    let off = skills(Level::Off)
        .before_shell("git commit -qm x", "main")
        .unwrap_or_default();
    assert!(
        off.contains("blast-radius") && off.contains("pstack/no-comments/SKILL.md") && !off.contains("hygiene"),
        "{off}"
    );
}

#[test]
fn session_core_situational_skills_rearm_tla_specs_and_many_files_two() {
    let on = Arc::new(AtomicBool::new(true));
    let skills = observer(json!({}), on);
    let cwd = Path::new("/repo");
    // Editing a TLA+ spec triggers maintain-formal-verification-skill.
    let spec = skills.cell_changed(&changes(&["specs/spawn-collect/spawn_collect.tla"]), cwd, "main");
    assert_eq!(spec.len(), 1, "{spec:?}");
    assert!(
        spec[0].contains("maintain-formal-verification-skill"),
        "expected formal verification skill: {:?}",
        spec
    );
    // Second distinct file hits MANY_FILES (2) and triggers rigor.
    let second = skills.cell_changed(&changes(&["src/plain.txt"]), cwd, "main");
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(
        second[0].starts_with("[skill] You changed 2 files: rigor applies from here on."),
        "{second:?}"
    );
    // Already given before compaction.
    assert!(
        skills
            .cell_changed(&changes(&["specs/spawn-collect/spawn_collect.tla"]), cwd, "main")
            .is_empty()
    );
    // Rearm after compaction allows the situational skill to fire again.
    skills.rearm("main");
    let rearmed = skills.cell_changed(&changes(&["specs/spawn-collect/spawn_collect.tla"]), cwd, "main");
    assert!(
        rearmed
            .iter()
            .any(|msg| msg.contains("maintain-formal-verification-skill")),
        "expected rearmed skill after compaction: {rearmed:?}"
    );
}

#[test]
fn session_core_ship_gate_resolves_nested_git_c_repo_for_hygiene() {
    use loki::hygiene::{self, Level};
    if hygiene::score(&["x = 1\n"], loki::proc::Deadline::after(60.0)).is_err() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let nested = parent.path().join("repo");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    std::fs::write(nested.join("store.py"), "LIMIT = 10\n").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-qm", "base"]);
    let swallowed =
        "def save(path, text):\n    try:\n        open(path, 'w').write(text)\n    except Exception:\n        pass\n";
    std::fs::write(nested.join("store.py"), format!("LIMIT = 10\n\n\n{swallowed}")).unwrap();
    // Session cwd is the non-git parent directory; command targets nested repo via `git -C repo`.
    let skills = observer(json!({}), Arc::new(AtomicBool::new(true)))
        .with_self_review(SelfReview::Advise, parent.path())
        .with_hygiene(Level::On);
    let told = skills
        .before_shell("git -C repo commit -qm x", "main")
        .unwrap_or_default();
    assert!(
        told.contains("store.py:7: loki/hygiene-swallowed-error"),
        "expected nested repo hygiene via git -C: {told}"
    );
}

#[test]
fn session_core_subagent_prompt_includes_rigor_worker_rules_in_engineering_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let bundled =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent")).unwrap();
    let env = session_core::RunEnv {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().to_path_buf(),
        var: Arc::new(|name| match name {
            "ULTRON_HINDSIGHT_URL" | "ULTRON_MEMORANTADO_URL" | "ULTRON_LSP" => Some("off".into()),
            _ => None,
        }),
        package: bundled,
    };
    let cwd = tmp.path().display().to_string();
    let extra = session_core::session_resources::SessionResources::default();
    let subagent_guide = resources::runtime_guide::RuntimeGuideOptions {
        durable_apis: true,
        reader: resources::runtime_guide::GuideReader::Subagent { swarm: true },
        ..Default::default()
    };
    let root_guide = resources::runtime_guide::RuntimeGuideOptions {
        durable_apis: true,
        ..Default::default()
    };
    let eng = session_core::lane::PromptMode {
        engineering: true,
        ..Default::default()
    };
    let off = session_core::lane::PromptMode {
        engineering: false,
        ..Default::default()
    };
    let (child_eng, _) =
        session_core::lane::system_prompt_with_resources(&env, &cwd, None, &subagent_guide, &eng, &extra).unwrap();
    assert!(
        child_eng.contains("You are working for the operator."),
        "expected rigor/worker.md in engineering subagent prompt"
    );
    let (child_off, _) =
        session_core::lane::system_prompt_with_resources(&env, &cwd, None, &subagent_guide, &off, &extra).unwrap();
    assert!(
        !child_off.contains("You are working for the operator."),
        "worker.md should not be injected outside engineering mode"
    );
    let (root_eng, _) =
        session_core::lane::system_prompt_with_resources(&env, &cwd, None, &root_guide, &eng, &extra).unwrap();
    assert!(
        !root_eng.contains("You are working for the operator."),
        "worker.md is for subagents, not root"
    );
}

#[test]
fn session_core_every_playbook_route_and_default_skill_exists_in_shipped_package() {
    let skills_root =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/skills"))
            .unwrap();
    let playbooks_dir = skills_root.join("pstack/rigor/playbooks");
    for (stem, _, _) in session_core::playbook_route::ROUTES {
        let path = playbooks_dir.join(format!("{stem}.md"));
        assert!(
            path.is_file(),
            "playbook_route::ROUTES names `{stem}` but {} does not exist",
            path.display()
        );
    }
    let skills_dir = skills_root.display().to_string();
    let paths = [skills_dir.clone()];
    let loaded = resources::skills::load_skills(&resources::skills::LoadSkillsOptions {
        cwd: &skills_dir,
        agent_dir: &skills_dir,
        home: &skills_dir,
        skill_paths: &paths,
        include_defaults: false,
    })
    .skills;
    let names: std::collections::HashSet<&str> = loaded.iter().map(|s| s.name.as_str()).collect();
    for rule in rules(&json!({})) {
        assert!(
            names.contains(rule.skill.as_str()),
            "situational_skills::DEFAULT_RULES names `{}` which is not in packages/coding-agent/skills",
            rule.skill
        );
    }
    for skill in [
        session_core::situational_skills::MANY_FILES_SKILL,
        session_core::situational_skills::SHIP_SKILL,
    ] {
        assert!(
            names.contains(skill),
            "situational_skills names `{skill}` which is not in packages/coding-agent/skills"
        );
    }
}
