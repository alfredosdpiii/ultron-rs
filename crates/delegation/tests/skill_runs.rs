//! `procedure.*` (delegation::skill_runs): the host walks a skill's procedure and decides every step itself — its
//! check commands, its judge decisions, its subagents, its `writes` boundary (by picture at each step, and per cell
//! through the run book's cell observer) — then runs the final check again. Runs are records in the skills module
//! document. Fakes: the judge seat (an ApiModule), the task host; check commands run for real (`/bin/sh`) in a
//! temporary directory. New in Rust: literal expectations.

use delegation::skill_runs::{RUNS, RunBook, SkillRuns};
use delegation::spawn_contract::plain_check_runner;
use host::file_hooks::{CellChanges, CellObserver};
use kernel::Signal;
use kernel::store::MemoryDocumentStore;
use kernel_apis::{ApiModule, Request, SkillSource, SkillsModule, TaskHost};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The judge seat: answers every decision with `verdict` and keeps the payloads.
struct FakeJudge {
    verdict: Value,
    seen: Mutex<Vec<Value>>,
}

impl ApiModule for FakeJudge {
    fn prefixes(&self) -> &[&str] {
        &["judge."]
    }
    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.seen.lock().unwrap().push(Value::Object(request.payload.clone()));
        Ok(self.verdict.clone())
    }
}

/// A task host whose `rlm-child@1` tasks "run" by writing `writes` into `cwd`; its root has `remaining_usd` left.
struct FakeHost {
    cwd: PathBuf,
    writes: Vec<(&'static str, &'static str)>,
    status: &'static str,
    remaining_usd: Option<f64>,
    spawned: Mutex<Vec<Value>>,
}

impl TaskHost for FakeHost {
    fn caller_task_id(&self, _lane: &str) -> Option<String> {
        None
    }
    fn task_lane(&self, _task_id: &str) -> Option<String> {
        None
    }
    fn strategy(&self, _definition: &str) -> Result<String, String> {
        Ok("rlm".into())
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
    fn spawn(&self, request: &Value, _parent: Option<&str>) -> Result<Value, String> {
        self.spawned.lock().unwrap().push(request.clone());
        for (path, text) in &self.writes {
            std::fs::write(self.cwd.join(path), text).unwrap();
        }
        Ok(json!({"id": "task-1"}))
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({"id": task_id, "status": self.status, "value": "done"}))
    }
    fn cancel(&self, _task_id: &str, _reason: &str) -> Result<Value, String> {
        Ok(Value::Null)
    }
    fn steer(&self, _task_id: &str, _message: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn usage(&self) -> Result<Option<Value>, String> {
        Ok(Some(json!({"cost": {"remainingUsd": self.remaining_usd}})))
    }
    fn now(&self) -> i64 {
        0
    }
}

struct Fixture {
    runs: SkillRuns,
    skills: Arc<SkillsModule>,
    judge: Arc<FakeJudge>,
    host: FakeHost,
    cwd: PathBuf,
    _tmp: tempfile::TempDir,
}

fn skill(name: &str, procedure: &str) -> String {
    format!("---\nname: {name}\ndescription: A test skill.\nprocedure:\n{procedure}---\n\n# {name}\n")
}

fn fixture(content: String, verdict: Value) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let loader: kernel_apis::SkillLoader = Arc::new(move || {
        Ok(vec![SkillSource {
            name: "demo".into(),
            description: "A test skill.".into(),
            file_path: "/skills/demo/SKILL.md".into(),
            content: content.clone(),
            disable_model_invocation: true,
        }])
    });
    let clock: kernel_apis::util::Clock = Arc::new(|| 1_000);
    let skills = Arc::new(SkillsModule::new(
        Arc::new(MemoryDocumentStore::default()),
        loader,
        clock.clone(),
        None,
    ));
    let judge = Arc::new(FakeJudge {
        verdict,
        seen: Mutex::default(),
    });
    let runner = Arc::new(OnceLock::new());
    let _ = runner.set(plain_check_runner());
    let runs = SkillRuns {
        book: Arc::new(RunBook::default()),
        skills: skills.clone(),
        judge: judge.clone(),
        cwd: cwd.clone(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        runner,
        proposals: tmp.path().join("skill-proposals"),
        session_id: Some("sess-1".into()),
        now: clock,
    };
    let host = FakeHost {
        cwd: cwd.clone(),
        writes: Vec::new(),
        status: "succeeded",
        remaining_usd: None,
        spawned: Mutex::default(),
    };
    Fixture {
        runs,
        skills,
        judge,
        host,
        cwd,
        _tmp: tmp,
    }
}

impl Fixture {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let payload: Map<String, Value> = payload.as_object().cloned().unwrap();
        self.runs.handle(&Request {
            kind,
            payload: &payload,
            lane: "main",
            caller_task: None,
            host: &self.host,
            signal: &Signal::new(),
        })
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.cwd.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn step(&self, run: &Value, state: Option<&str>) -> Value {
        let mut payload = json!({"run": run["run"], "segment": run["segment"]["name"]});
        if let Some(state) = state {
            payload["state"] = json!(state);
        }
        self.call("procedure.step", payload).unwrap()
    }

    fn records(&self) -> Vec<Value> {
        self.skills.records(RUNS).unwrap()
    }
}

/// tdd's shape: a test that must fail first, a judgment on why, a fix whose check is the final check.
const TDD: &str = "  inputs:\n    test: the test command\n  segments:\n    - name: failing-test\n      goal: Write a failing test.\n      writes: [\"tests/**\"]\n      check: {run: \"{{test}}\", expect: fail}\n    - name: right-reason\n      goal: Why it fails.\n      writes: []\n      judge: {kind: skill.demo, questions: {why: {type: yesno, instructions: Right reason?}}, pass: {why: true}}\n    - name: fix\n      goal: Fix it.\n      writes: [\"src/**\"]\n      check: \"{{test}}\"\n";

fn yes() -> Value {
    json!({"rung": "first", "backend": "d1", "model": "d1-2b", "answers": {"why": {"value": true, "confidence": 0.95}}})
}

#[test]
fn skill_runs_walk_a_procedure_and_the_host_runs_every_check_then_the_final_check_again() {
    let f = fixture(skill("demo", TDD), yes());
    // The test passes once src/fixed exists.
    let test = "test -f src/fixed && echo green || { echo 'assert failed: empty input'; exit 1; }";
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": test}}))
        .unwrap();
    assert_eq!(
        (
            run["outcome"].clone(),
            run["segment"]["name"].clone(),
            run["segments"].clone(),
            run["tries_left"].clone()
        ),
        (
            json!("running"),
            json!("failing-test"),
            json!(["failing-test", "right-reason", "fix"]),
            json!(3)
        )
    );
    // A second run on the lane is refused while this one runs.
    assert!(
        f.call("procedure.start", json!({"name": "demo", "input": {"test": test}}))
            .unwrap_err()
            .starts_with("This lane is running skill demo")
    );
    f.write("tests/empty.rs", "#[test] fn t() {}");
    let judged = f.step(&run, None);
    assert_eq!(judged["step"]["passed"], true, "{judged}");
    assert_eq!(judged["segment"]["name"], "right-reason");
    // A judgment needs the model's statement; asking without one is an error, not a try.
    assert_eq!(
        f.call("procedure.step", json!({"run": run["run"], "segment": "right-reason"}))
            .unwrap_err(),
        "segment right-reason is a judgment: pass state=<your statement>"
    );
    let fixing = f.step(&judged, Some("empty input should give []"));
    assert_eq!(fixing["segment"]["name"], "fix", "{fixing}");
    let seen = f.judge.seen.lock().unwrap().clone();
    let state: Value = serde_json::from_str(seen[0]["state"].as_str().unwrap()).unwrap();
    assert_eq!(
        (
            state["statement"].clone(),
            state["evidence"]["exit_code"].clone(),
            seen[0]["kind"].clone()
        ),
        (json!("empty input should give []"), json!(1), json!("skill.demo"))
    );
    assert!(
        state["evidence"]["stdout"]
            .as_str()
            .unwrap()
            .contains("assert failed: empty input")
    );
    // The fix check fails until the fix exists: one try used, same segment.
    let again = f.step(&fixing, None);
    assert_eq!(
        (
            again["step"]["passed"].clone(),
            again["segment"]["name"].clone(),
            again["tries_left"].clone()
        ),
        (json!(false), json!("fix"), json!(2))
    );
    f.write("src/fixed", "1");
    let done = f.step(&again, None);
    assert_eq!(
        (done["outcome"].clone(), done["segment"].clone()),
        (json!("passed"), Value::Null),
        "{done}"
    );
    let records = f.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        (
            record["outcome"].clone(),
            record["path"].clone(),
            record["lane"].clone(),
            record["duration_ms"].clone()
        ),
        (json!("passed"), json!("/skills/demo/SKILL.md"), json!("main"), json!(0))
    );
    let tries: Vec<(Value, Value)> = record["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].clone(), s["tries"].clone()))
        .collect();
    assert_eq!(
        tries,
        [
            (json!("failing-test"), json!(1)),
            (json!("right-reason"), json!(1)),
            (json!("fix"), json!(2))
        ]
    );
    assert_eq!(record["input_digest"].as_str().unwrap().len(), 64);
    assert_eq!(record["output_digest"], done["output_digest"]);
    let coverage = f.call("procedure.coverage", json!({"name": "demo"})).unwrap();
    assert_eq!(
        (coverage["runs"].clone(), coverage["outcomes"].clone()),
        (json!(1), json!({"passed": 1}))
    );
    assert_eq!(coverage["segments"][2]["mean_tries"], 2.0);
    // The lane is free again.
    assert!(
        f.call("procedure.start", json!({"name": "demo", "input": {"test": test}}))
            .is_ok()
    );
}

#[test]
fn skill_runs_a_change_outside_the_segment_files_fails_the_step_and_a_failing_test_must_fail() {
    let f = fixture(skill("demo", TDD), yes());
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "true"}}))
        .unwrap();
    // The production file is outside the test segment's boundary, and the "failing" test passes.
    f.write("src/lib.rs", "fn f() {}");
    let step = f.step(&run, None);
    assert_eq!(
        (step["step"]["passed"].clone(), step["tries_left"].clone()),
        (json!(false), json!(2))
    );
    let failed: Vec<&str> = step["step"]["evidence"]["failed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(
        failed,
        [
            "changed_files_within [\"tests/**\"]: changed outside [\"tests/**\"]: src/lib.rs",
            "run \"true\": exit 0 (expected to fail)"
        ]
    );
    // Out of tries: the run fails, and its record says where.
    f.step(&step, None);
    let last = f.step(&step, None);
    assert_eq!(
        (last["outcome"].clone(), last["reason"].clone()),
        (json!("failed"), json!("segment failing-test failed 3 times"))
    );
    assert_eq!(
        f.records()[0]["segments"],
        json!([{"name": "failing-test", "kind": "check", "tries": 3, "outcome": "failed"}])
    );
}

#[test]
fn skill_runs_the_cell_observer_reports_changes_outside_the_boundary_and_fails_the_next_step() {
    let f = fixture(skill("demo", TDD), yes());
    let book = f.runs.book.clone();
    assert!(!book.enabled(), "no run: no snapshots");
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "false"}}))
        .unwrap();
    assert!(book.enabled());
    let changes = |paths: &[&str]| CellChanges {
        files: paths.iter().map(|p| f.cwd.join(p)).collect(),
        complete: true,
        ..CellChanges::default()
    };
    // The cell that started the run is judged by the start's picture.
    assert!(book.cell_changed(&changes(&["src/a.rs"]), &f.cwd, "main").is_empty());
    assert!(book.cell_changed(&changes(&["tests/a.rs"]), &f.cwd, "main").is_empty());
    assert_eq!(
        book.cell_changed(&changes(&["src/a.rs", "tests/b.rs"]), &f.cwd, "main"),
        [
            "[skill demo] segment failing-test may change only [\"tests/**\"]; this cell changed src/a.rs. Its next check counts as failed: undo those changes."
        ]
    );
    assert!(
        book.cell_changed(&changes(&["src/a.rs"]), &f.cwd, "other-lane")
            .is_empty()
    );
    // The file was changed back before the step (nothing differs now), but the cell changed it: the step fails.
    let step = f.step(&run, None);
    assert_eq!(step["step"]["passed"], false, "{step}");
    assert_eq!(
        step["step"]["evidence"]["boundary"],
        "cells changed files outside [\"tests/**\"]: src/a.rs"
    );
    // The violation counted once; the next step passes on its own merits.
    let next = f.step(&step, None);
    assert_eq!(next["segment"]["name"], "right-reason", "{next}");
}

#[test]
fn skill_runs_an_unsure_judge_stops_the_run_for_a_person() {
    let unsure = json!({"rung": "review", "backend": "d1", "answers": {"why": {"value": true, "confidence": 0.5}}, "reason": "below threshold"});
    let f = fixture(skill("demo", TDD), unsure);
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "false"}}))
        .unwrap();
    let judged = f.step(&run, None);
    let stopped = f.step(&judged, Some("because"));
    assert_eq!(
        (
            stopped["outcome"].clone(),
            stopped["reason"].clone(),
            stopped["step"]["evidence"]["rung"].clone()
        ),
        (json!("needs_review"), json!("the judge is unsure"), json!("review"))
    );
    assert_eq!(f.records()[0]["outcome"], "needs_review");
    // A "no" from a sure judge is a failed try instead.
    let no = json!({"rung": "first", "answers": {"why": {"value": false, "confidence": 0.9}}});
    let f = fixture(skill("demo", TDD), no);
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "false"}}))
        .unwrap();
    let judged = f.step(&run, None);
    let tried = f.step(&judged, Some("because"));
    assert_eq!(
        (tried["outcome"].clone(), tried["step"]["passed"].clone()),
        (json!("running"), json!(false))
    );
}

#[test]
fn skill_runs_the_final_check_must_pass_again_when_the_host_reruns_it() {
    // Passes the first time only.
    let flaky = "  segments:\n    - name: once\n      goal: g\n      check: \"test ! -f ran && touch ran\"\n";
    let f = fixture(skill("demo", flaky), yes());
    let run = f.call("procedure.start", json!({"name": "demo"})).unwrap();
    let done = f.step(&run, None);
    assert_eq!(done["step"]["passed"], true);
    assert_eq!(done["outcome"], "final_check_mismatch", "{done}");
    assert!(
        done["reason"]
            .as_str()
            .unwrap()
            .starts_with("segment once's check passed during the run but not when the host ran it again at the end")
    );
}

#[test]
fn skill_runs_requires_files_and_budget_before_a_segment_begins() {
    let needs = "  segments:\n    - name: a\n      goal: g\n      requires: {files: [plan.md]}\n      check: \"true\"\n    - name: b\n      goal: g\n      requires: {budget_usd: 2}\n      check: \"true\"\n";
    let mut f = fixture(skill("demo", needs), yes());
    let blocked = f.call("procedure.start", json!({"name": "demo"})).unwrap();
    assert_eq!(blocked["outcome"], "blocked");
    assert_eq!(blocked["reason"], "segment a requires files: missing: plan.md");
    f.write("plan.md", "p");
    f.host.remaining_usd = Some(0.5);
    let run = f.call("procedure.start", json!({"name": "demo"})).unwrap();
    let next = f.step(&run, None);
    assert_eq!(
        (next["outcome"].clone(), next["reason"].clone()),
        (
            json!("blocked"),
            json!("segment b requires $2 of budget and $0.5 is left")
        )
    );
    let outcomes: Vec<Value> = f.records().iter().map(|r| r["outcome"].clone()).collect();
    assert_eq!(outcomes, [json!("blocked"), json!("blocked")]);
}

#[test]
fn skill_runs_an_agent_step_spawns_on_the_segment_model_and_the_host_checks_its_work() {
    let agent = "  segments:\n    - name: review\n      goal: Write the review.\n      writes: [\"notes/**\"]\n      agent: {brief: Review the change., model: anthropic/claude-haiku-5-5, checks: [{files_exist: [notes/review.md]}]}\n";
    let mut f = fixture(skill("demo", agent), yes());
    std::fs::create_dir_all(f.cwd.join("notes")).unwrap();
    f.host.writes = vec![("notes/review.md", "ok"), ("stray.txt", "x")];
    let run = f.call("procedure.start", json!({"name": "demo"})).unwrap();
    let step = f.step(&run, None);
    let spawned = f.host.spawned.lock().unwrap().clone();
    assert_eq!(
        (spawned[0]["definition"].clone(), spawned[0]["model"].clone()),
        (json!("rlm-child@1"), json!("anthropic/claude-haiku-5-5"))
    );
    let prompt = spawned[0]["input"]["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with("Review the change.\n\nGoal: Write the review.\n\nWhen you end, the host checks"),
        "{prompt}"
    );
    // The child wrote outside the segment's files: not a pass.
    assert_eq!(step["step"]["passed"], false, "{step}");
    assert_eq!(
        step["step"]["evidence"]["contract"]["failed"][0],
        "changed_files_within [\"notes/**\"]: changed outside [\"notes/**\"]: stray.txt"
    );
    std::fs::remove_file(f.cwd.join("stray.txt")).unwrap();
    f.host.writes = vec![("notes/review.md", "better")];
    let done = f.step(&step, None);
    assert_eq!(done["outcome"], "passed", "{done}");
}

#[test]
fn skill_runs_refuse_wrong_segments_unknown_runs_and_skills_without_a_procedure() {
    let f = fixture("---\nname: demo\ndescription: d\n---\nJust prose.".into(), yes());
    assert_eq!(
        f.call("procedure.start", json!({"name": "demo"})).unwrap_err(),
        "Skill demo has no procedure: read it with skills.load and follow its steps"
    );
    assert_eq!(
        f.call("procedure.start", json!({"name": "nope"})).unwrap_err(),
        "Unknown skill: nope"
    );
    let f = fixture(skill("demo", TDD), yes());
    assert_eq!(
        f.call("procedure.start", json!({"name": "demo"})).unwrap_err(),
        "Skill demo: missing input: test (the test command)"
    );
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "false"}}))
        .unwrap();
    assert_eq!(
        f.call("procedure.step", json!({"run": run["run"], "segment": "fix"}))
            .unwrap_err(),
        "Run ".to_string() + run["run"].as_str().unwrap() + " is at segment failing-test, not fix"
    );
    assert_eq!(
        f.call("procedure.step", json!({"run": "r0", "segment": "fix"}))
            .unwrap_err(),
        "No skill run r0 is in progress on this lane"
    );
    let aborted = f
        .call("procedure.abort", json!({"run": run["run"], "reason": "wrong bug"}))
        .unwrap();
    assert_eq!(
        (aborted["outcome"].clone(), aborted["reason"].clone()),
        (json!("aborted"), json!("wrong bug"))
    );
    assert_eq!(f.records()[0]["outcome"], "aborted");
}

#[test]
fn skill_runs_propose_keeps_a_procedure_skill_with_its_session_for_approval() {
    let f = fixture(skill("demo", TDD), yes());
    let content = skill(
        "flaky",
        "  segments:\n    - name: rerun\n      goal: g\n      check: \"true\"\n",
    );
    let kept = f
        .call(
            "procedure.propose",
            json!({"name": "flaky", "content": content, "evidence": [{"ref": "skill-run:r1"}]}),
        )
        .unwrap();
    assert_eq!(
        (kept["state"].clone(), kept["session"].clone()),
        (json!("proposed"), json!("sess-1"))
    );
    let dir: &Path = &f.runs.proposals;
    assert!(dir.join(kept["id"].as_str().unwrap()).join("SKILL.md").is_file());
}

/// A run in progress is recorded `running`; the worker that walks it dies, and the session's next stack (a fresh
/// book over the same document) marks it `interrupted` instead of leaving it running forever. A live run is left.
#[test]
fn skill_runs_a_run_a_dead_worker_left_running_is_marked_interrupted() {
    let f = fixture(skill("demo", TDD), yes());
    let run = f
        .call("procedure.start", json!({"name": "demo", "input": {"test": "true"}}))
        .unwrap();
    assert_eq!(f.records()[0]["outcome"], "running");
    assert_eq!(f.runs.interrupt_orphans(), Ok(0), "the live book still walks it");
    assert_eq!(f.records()[0]["outcome"], "running");
    let restarted = SkillRuns {
        book: Arc::new(RunBook::default()),
        skills: f.skills.clone(),
        judge: f.judge.clone(),
        cwd: f.cwd.clone(),
        env: Vec::new(),
        runner: Arc::new(OnceLock::new()),
        proposals: f.cwd.join("skill-proposals"),
        session_id: Some("sess-1".into()),
        now: Arc::new(|| 4_000),
    };
    assert_eq!(restarted.interrupt_orphans(), Ok(1));
    let record = &f.records()[0];
    assert_eq!(
        (
            record["id"].clone(),
            record["outcome"].clone(),
            record["ended_at"].clone(),
            record["duration_ms"].clone()
        ),
        (run["run"].clone(), json!("interrupted"), json!(4_000), json!(3_000))
    );
    assert!(record["reason"].as_str().unwrap().contains("worker"), "{record}");
    assert_eq!(restarted.interrupt_orphans(), Ok(0), "marked once");
}
