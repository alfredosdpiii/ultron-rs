//! `skills.run` (skills_api.py SkillRun, delegation::skill_runs, wired in session_core::compose) in the product's
//! session: a root cell starts a procedure skill from the user's skills directory, and its first segment's setup
//! runs in the REPL namespace; a cell that changes a file outside the segment's files fails its step, the host's
//! check passes once the work is there, a judgment with no judge backend stops the run for a person, and
//! `skills.coverage` reads the run's record back. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};

const SKILL: &str = "---
name: demo
description: A procedure used by the test.
procedure:
  inputs:
    target: the file the work makes
  segments:
    - name: make
      goal: Make the target file.
      setup: setup_ran = True
      writes: [\"out/**\"]
      check: \"test -f {{target}}\"
    - name: confirm
      goal: Say why it is right.
      judge: {questions: {ok: {type: yesno, instructions: Right?}}, pass: {ok: true}}
---

# demo
";

const START: &str = r#"import json, os
run = await skills.run("demo", input={"target": "out/done"})
print('start', json.dumps(run.status))
print('setup', json.dumps(setup_ran))
print('repr', json.dumps(repr(run)))"#;

const WORK: &str = r#"open('stray.txt', 'w').write('x')
print('step1', json.dumps(await run.check()))
os.remove('stray.txt')
os.makedirs('out', exist_ok=True)
open('out/done', 'w').write('1')
print('step2', json.dumps(await run.check()))
print('step3', json.dumps(await run.check(state="out/done exists")))
print('coverage', json.dumps(await skills.coverage("demo")))
try:
    await run.check()
except RuntimeError as e:
    print('ended', json.dumps(str(e)))"#;

fn printed(output: &str, prefix: &str) -> Value {
    output
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{prefix} ")))
        .map(|l| serde_json::from_str(l).unwrap())
        .unwrap_or_else(|| panic!("no {prefix} line in {output}"))
}

fn outputs(a: &p_session_support::Actor) -> String {
    a.requests()
        .iter()
        .flat_map(p_session_support::tool_results)
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn session_core_skills_run_walks_a_procedure_with_host_checks() {
    let a = actor(
        in_order(vec![tool("toolu_1", START), tool("toolu_2", WORK), text("done")]),
        Setup {
            agent_files: vec![("skills/demo/SKILL.md", SKILL.as_bytes().to_vec())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("run the demo skill");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let output = outputs(&a);
    let start = printed(&output, "start");
    assert_eq!(
        (
            start["outcome"].clone(),
            start["segment"]["name"].clone(),
            start["segment"]["check"]["run"].clone()
        ),
        (json!("running"), json!("make"), json!("test -f out/done")),
        "{output}"
    );
    assert_eq!(printed(&output, "setup"), true);
    assert_eq!(printed(&output, "repr"), "<SkillRun demo running segment=make>");
    let step1 = printed(&output, "step1");
    assert_eq!(
        (step1["step"]["passed"].clone(), step1["tries_left"].clone()),
        (json!(false), json!(2))
    );
    assert_eq!(
        step1["step"]["evidence"]["failed"],
        json!([
            "changed_files_within [\"out/**\"]: changed outside [\"out/**\"]: stray.txt",
            "run \"test -f out/done\": exit 1"
        ])
    );
    let step2 = printed(&output, "step2");
    assert_eq!(
        (step2["step"]["passed"].clone(), step2["segment"]["name"].clone()),
        (json!(true), json!("confirm")),
        "{output}"
    );
    // No judge backend is configured: the judge abstains and a person must settle it.
    let step3 = printed(&output, "step3");
    assert_eq!(
        (step3["outcome"].clone(), step3["step"]["evidence"]["rung"].clone()),
        (json!("needs_review"), json!("abstained")),
        "{output}"
    );
    let coverage = printed(&output, "coverage");
    assert_eq!(
        (coverage["runs"].clone(), coverage["outcomes"].clone()),
        (json!(1), json!({"needs_review": 1}))
    );
    assert_eq!(
        coverage["segments"][0],
        json!({"name": "make", "kind": "check", "reached": 1, "passed": 1, "failed": 0, "mean_tries": 2.0})
    );
    assert_eq!(coverage["never_passed"], json!(["confirm"]));
    let id = start["run"].as_str().unwrap();
    assert_eq!(
        printed(&output, "ended"),
        format!("skill run {id} has ended: needs_review")
    );
}
