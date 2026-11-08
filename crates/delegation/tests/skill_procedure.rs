//! Skill procedures (delegation::procedure): the `procedure:` frontmatter format, its inputs, the bundled `tdd`
//! skill as the worked example, coverage over run records, and proposals waiting for the user's approval. New in
//! Rust: literal expectations.

use delegation::procedure::{self, Step, proposals};
use delegation::spawn_contract::{Expect, HostCheck};
use serde_json::{Value, json};
use std::path::Path;

fn tdd() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/skills/pstack/tdd/SKILL.md");
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn procedure_the_bundled_tdd_skill_binds_its_inputs_into_checks_and_boundaries() {
    let raw = procedure::raw_of(&tdd()).unwrap().expect("tdd has a procedure");
    let input = json!({"test_command": "cargo test -q parses_empty", "test_files": ["tests/parse.rs"],
                       "fix_files": ["src/parse.rs", "src/lib.rs"]});
    let parsed = procedure::parse(&raw, &input).unwrap();
    let names: Vec<&str> = parsed.segments.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["failing-test", "right-reason", "fix"]);
    let first = &parsed.segments[0];
    assert_eq!(first.writes, Some(vec!["tests/parse.rs".to_string()]));
    assert_eq!(
        first.step,
        Step::Check(HostCheck::Run {
            command: "cargo test -q parses_empty".into(),
            expect: Expect::Fail,
            timeout_ms: 120_000
        })
    );
    assert!(first.may_write("tests/parse.rs") && !first.may_write("src/parse.rs"));
    assert_eq!(parsed.segments[1].writes, Some(vec![]));
    assert!(matches!(&parsed.segments[1].step, Step::Judge { kind, pass, .. }
        if kind == "skill.tdd" && pass["right_reason"] == true));
    // A list input fills a list; the fix may change both production files, and its check is the final check.
    assert_eq!(
        parsed.segments[2].writes,
        Some(vec!["src/parse.rs".to_string(), "src/lib.rs".to_string()])
    );
    assert_eq!(parsed.final_check().map(|(index, _)| index), Some(2));
    let brief = parsed.segments[0].brief(0, 3);
    assert_eq!(
        (
            brief["kind"].clone(),
            brief["check"]["expect"].clone(),
            brief["of"].clone()
        ),
        (json!("check"), json!("fail"), json!(3))
    );
    assert_eq!(
        procedure::parse(&raw, &json!({"test_command": "x"})).unwrap_err(),
        "missing input: fix_files (globs of the production files the fix may change), test_files (globs of the test files the regression test may add or change)"
    );
    assert_eq!(
        procedure::parse(
            &raw,
            &json!({"test_command": "x", "test_files": [], "fix_files": [], "extra": 1})
        )
        .unwrap_err(),
        "unknown input extra (the procedure takes: fix_files, test_command, test_files)"
    );
    assert!(procedure::validate(&raw).is_ok());
}

fn one(segment: Value) -> Result<procedure::Procedure, String> {
    procedure::parse(&json!({"segments": [segment]}), &Value::Null)
}

#[test]
fn procedure_parse_refuses_malformed_segments() {
    let cases = [
        (
            json!({"name": "a", "goal": "g"}),
            "procedure segment a needs exactly one of check, judge or agent",
        ),
        (
            json!({"name": "a", "goal": "g", "check": "x", "judge": {}}),
            "procedure segment a needs exactly one of check, judge or agent",
        ),
        (
            json!({"name": "A b", "goal": "g", "check": "x"}),
            "procedure.segments[0].name must match [a-z][a-z0-9-]* (at most 40 characters)",
        ),
        (
            json!({"name": "a", "check": "x"}),
            "procedure segment a.goal must be a nonempty string of at most 4000 characters",
        ),
        (
            json!({"name": "a", "goal": "g", "check": "x", "max_iters": 0}),
            "procedure segment a.max_iters must be an integer from 1 to 10",
        ),
        (
            json!({"name": "a", "goal": "g", "check": "x", "writes": ["../up"]}),
            "procedure segment a.writes has an invalid glob: ../up",
        ),
        (
            json!({"name": "a", "goal": "g", "check": "x", "colour": 1}),
            "procedure.segments[0]: unknown field colour",
        ),
        (
            json!({"name": "a", "goal": "g", "judge": {"questions": {"q": {"type": "yesno"}}, "pass": {"r": true}}}),
            "procedure segment a.judge.pass names r, which is not a question",
        ),
        (
            json!({"name": "a", "goal": "g", "agent": {"brief": "b"}}),
            "procedure segment a.agent.model (its model tier) must be a nonempty string of at most 200 characters",
        ),
        (
            json!({"name": "a", "goal": "g", "check": {"run": "x", "expect": "maybe"}}),
            "procedure segment a.check: contract.checks[0].expect must be \"exit 0\" (or \"exit N\"), {\"exit\": N}, \"fail\" or {\"stdout_contains\": \"text\"}",
        ),
    ];
    for (segment, error) in cases {
        assert_eq!(one(segment.clone()).unwrap_err(), error, "{segment}");
    }
    assert_eq!(
        procedure::parse(
            &json!({"segments": [{"name": "a", "goal": "g", "check": "{{cmd}}"}]}),
            &Value::Null
        )
        .unwrap_err(),
        "procedure uses {{cmd}}, which procedure.inputs does not declare"
    );
    let twice =
        json!({"segments": [{"name": "a", "goal": "g", "check": "x"}, {"name": "a", "goal": "g", "check": "y"}]});
    assert_eq!(
        procedure::parse(&twice, &Value::Null).unwrap_err(),
        "procedure has two segments named a"
    );
    let agent = one(
        json!({"name": "a", "goal": "g", "agent": {"brief": "b", "model": "anthropic/claude-haiku-5-5",
                                                                 "checks": [{"files_exist": ["x"]}]}}),
    )
    .unwrap();
    assert!(matches!(&agent.segments[0].step, Step::Agent { checks, .. } if checks.len() == 1));
    assert_eq!(agent.segments[0].max_iters, 3);
}

#[test]
fn procedure_coverage_counts_runs_of_the_current_version_per_segment() {
    let raw = json!({"segments": [{"name": "a", "goal": "g", "check": "x"}, {"name": "b", "goal": "g", "check": "y"}]});
    let current = procedure::validate(&raw).unwrap();
    let runs = [
        json!({"name": "s", "version": "v2", "outcome": "passed",
               "segments": [{"name": "a", "tries": 2, "outcome": "passed"}, {"name": "b", "tries": 1, "outcome": "passed"}]}),
        json!({"name": "s", "version": "v2", "outcome": "failed",
               "segments": [{"name": "a", "tries": 3, "outcome": "failed"}]}),
        json!({"name": "s", "version": "v1", "outcome": "passed", "segments": []}),
        json!({"name": "other", "version": "v2", "outcome": "passed", "segments": []}),
    ];
    let coverage = procedure::coverage("s", "v2", Some(&current), &runs);
    assert_eq!(
        coverage,
        json!({
            "name": "s", "version": "v2", "runs": 2, "outcomes": {"passed": 1, "failed": 1},
            "segments": [
                {"name": "a", "kind": "check", "reached": 2, "passed": 1, "failed": 1, "mean_tries": 2.5},
                {"name": "b", "kind": "check", "reached": 1, "passed": 1, "failed": 0, "mean_tries": 1.0},
            ],
            "never_passed": [], "other_versions": 1,
        })
    );
}

fn skill_md(name: &str, check: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: Re-run a flaky test in isolation.\nprocedure:\n  segments:\n    - name: rerun\n      goal: Re-run it.\n      check: \"{check}\"\n---\n\n# {name}\n"
    )
}

#[test]
fn procedure_proposals_wait_for_the_users_approval_and_are_checked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, skills) = (tmp.path().join("skill-proposals"), tmp.path().join("skills"));
    let evidence = json!([{"ref": "session://s1#a..b", "quote": "ran it three times"}]);
    assert_eq!(
        proposals::propose(&dir, "flaky", &skill_md("flaky", "true"), &json!([]), None, 1).unwrap_err(),
        "evidence must be a list of 1 to 20 {ref, quote}"
    );
    assert!(
        proposals::propose(
            &dir,
            "flaky",
            &skill_md("flaky", "true"),
            &json!([{"ref": "/tmp/x"}]),
            None,
            1
        )
        .unwrap_err()
        .starts_with("evidence ref must be a session span")
    );
    assert_eq!(
        proposals::propose(&dir, "flaky", &skill_md("other", "true"), &evidence, None, 1).unwrap_err(),
        "the frontmatter's name must be flaky"
    );
    assert_eq!(
        proposals::propose(
            &dir,
            "flaky",
            "---\nname: flaky\ndescription: d\n---\nbody",
            &evidence,
            None,
            1
        )
        .unwrap_err(),
        "a proposed skill needs a procedure: in its frontmatter"
    );
    let kept = proposals::propose(&dir, "flaky", &skill_md("flaky", "true"), &evidence, Some("sess-1"), 5).unwrap();
    let id = kept["id"].as_str().unwrap().to_string();
    assert_eq!(
        (kept["state"].clone(), kept["session"].clone(), kept["segments"].clone()),
        (json!("proposed"), json!("sess-1"), json!(["rerun"]))
    );
    assert_eq!(
        proposals::propose(&dir, "flaky", &skill_md("flaky", "true"), &evidence, None, 6).unwrap_err(),
        format!("Duplicate skill proposal: {id} is already waiting")
    );
    // Nothing is published until approval; an edit that breaks the procedure is refused then.
    assert!(!skills.exists());
    let sha = |text: &str| jsrt::hash::sha256_hex(text.as_bytes());
    let broken = "---\nname: flaky\ndescription: d\n---\n";
    std::fs::write(dir.join(&id).join("SKILL.md"), broken).unwrap();
    assert_eq!(
        proposals::approve(&dir, &id, &skills, 7, &sha(broken)).unwrap_err(),
        "a proposed skill needs a procedure: in its frontmatter"
    );
    // The approval is of the content the user was shown: an edit after that is refused.
    let edited = skill_md("flaky", "cargo test -q");
    std::fs::write(dir.join(&id).join("SKILL.md"), &edited).unwrap();
    let now = sha(&edited);
    assert_eq!(
        proposals::approve(&dir, &id, &skills, 8, &sha(broken)).unwrap_err(),
        format!(
            "Skill proposal {id} changed after it was shown (sha256 now {}); run approve again to see it",
            &now[..12]
        )
    );
    assert!(!skills.exists());
    let published = proposals::approve(&dir, &id, &skills, 8, &now).unwrap();
    assert_eq!(published["state"], "published");
    assert_eq!(
        std::fs::read_to_string(skills.join("flaky/SKILL.md")).unwrap(),
        skill_md("flaky", "cargo test -q")
    );
    assert_eq!(
        proposals::approve(&dir, &id, &skills, 9, &now).unwrap_err(),
        format!("Skill proposal {id} is published")
    );
    // A second proposal of the same name never overwrites the published skill.
    let again = proposals::propose(&dir, "flaky", &skill_md("flaky", "false"), &evidence, None, 10).unwrap();
    let again = again["id"].as_str().unwrap();
    assert!(
        proposals::approve(&dir, again, &skills, 11, &sha(&skill_md("flaky", "false")))
            .unwrap_err()
            .ends_with("exists; remove it or rename the proposal first")
    );
    let rejected = proposals::reject(&dir, again, Some("not useful"), 12).unwrap();
    assert_eq!(
        (rejected["state"].clone(), rejected["reason"].clone()),
        (json!("rejected"), json!("not useful"))
    );
    let states: Vec<Value> = proposals::list(&dir)
        .unwrap()
        .iter()
        .map(|r| r["state"].clone())
        .collect();
    assert_eq!(states, [json!("published"), json!("rejected")]);
    assert_eq!(
        proposals::show(&dir, "../x").unwrap_err(),
        "Unknown skill proposal: ../x"
    );
}
