//! `procedure.*` (Rust-only): the host side of `skills.run(name)`, which walks a skill's procedure
//! ([`crate::procedure`]) segment by segment. The REPL only asks; the host decides:
//!
//! - entering a segment checks its `requires` (files exist, the root's remaining budget) and takes a picture of the
//!   working directory;
//! - `procedure.step` runs the segment's step itself: its `check` command (under the session's sandbox, as a spawn
//!   contract's `run` check), its `judge` decision (`judge.decide`, the model's statement beside the last check's
//!   output) or its `agent` (an `rlm-child@1` task on the segment's model, its checks run when it ends), plus the
//!   segment's `writes` boundary over everything that changed since it began;
//! - a failed step counts a try; at `max_iters` the run fails; a judge that stays unsure stops it for a person;
//! - after the last segment the host runs the final check (the last `check` segment's command) again, and the run
//!   passes only if it still passes.
//!
//! Between steps the [`RunBook`] watches each cell of a lane with a run (a `host::file_hooks::CellObserver`): a cell
//! that changes files outside the segment's `writes` is told so at once, and the segment's next step fails. A cell
//! that steps is judged by the step's own picture instead.
//!
//! Every run is a record in the skills module document (`ultron.module/skills`, list `runs`): skill path and
//! version, input and output digests, each segment's tries and outcome, duration and outcome. The record is written
//! `running` when the run starts; a worker that dies mid-run leaves it so, and the next stack of the session marks
//! it `interrupted` ([`SkillRuns::interrupt_orphans`]).
//! `procedure.coverage` summarizes them; `procedure.propose` keeps a proposed procedure skill for the user's
//! approval ([`crate::procedure::proposals`]).

use crate::procedure::{self, Procedure, Step};
use crate::spawn_contract::{CheckPlace, CheckRunner, Hop, HostCheck, SpawnContract};
use crate::workspace::{WorkspaceSnapshot, diff_snapshots, snapshot_workspace};
use host::file_hooks::{CellChanges, CellObserver, inside};
use kernel::Signal;
use kernel_apis::util::Clock;
use kernel_apis::{ApiModule, PinnedSkill, Request, SkillsModule, TaskHost};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The record list of the skills module document that holds runs.
pub const RUNS: &str = "runs";
const OUTPUT_CHARS: usize = 2_000;

/// A run in progress on one lane.
struct Run {
    id: String,
    skill: PinnedSkill,
    procedure: Procedure,
    index: usize,
    tries: u32,
    baseline: WorkspaceSnapshot,
    /// Files the lane's cells changed outside the segment's boundary since its last step.
    violations: Vec<String>,
    /// A step ran during the lane's current cell: that cell is judged by the step's picture.
    stepped: bool,
    results: Vec<Value>,
    /// The last `check` step's command output, the judge's evidence.
    evidence: Value,
    started: i64,
}

/// The runs in progress, one per lane at most; also the cell observer that enforces their boundaries.
#[derive(Default)]
pub struct RunBook {
    runs: Mutex<HashMap<String, Run>>,
}

impl CellObserver for RunBook {
    fn enabled(&self) -> bool {
        jsrt::sync::lock(&self.runs)
            .values()
            .any(|run| run.procedure.segments[run.index].writes.is_some())
    }

    fn cell_changed(&self, changes: &CellChanges, cwd: &Path, lane: &str) -> Vec<String> {
        let mut runs = jsrt::sync::lock(&self.runs);
        let Some(run) = runs.get_mut(lane) else {
            return Vec::new();
        };
        if std::mem::take(&mut run.stepped) {
            return Vec::new();
        }
        let segment = &run.procedure.segments[run.index];
        let outside: Vec<String> = changes
            .files
            .iter()
            .chain(&changes.checked)
            .chain(&changes.deleted)
            .map(|path| inside(cwd, path).unwrap_or_else(|| path.display().to_string()))
            .filter(|path| !segment.may_write(path))
            .collect();
        if outside.is_empty() {
            return Vec::new();
        }
        let line = format!(
            "[skill {}] segment {} may change only {}; this cell changed {}. Its next check counts as failed: undo \
             those changes.",
            run.skill.name,
            segment.name,
            json!(segment.writes),
            outside.join(", ")
        );
        for path in outside {
            if !run.violations.contains(&path) {
                run.violations.push(path);
            }
        }
        vec![line]
    }
}

/// Serves `procedure.*` for one session.
pub struct SkillRuns {
    pub book: Arc<RunBook>,
    pub skills: Arc<SkillsModule>,
    /// Serves `judge.decide` (the session's judge seat).
    pub judge: Arc<dyn ApiModule>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// The session's check runner, set once its stack is built.
    pub runner: Arc<OnceLock<CheckRunner>>,
    /// `<agent dir>/skill-proposals`.
    pub proposals: PathBuf,
    pub session_id: Option<String>,
    pub now: Clock,
}

fn tail(text: &str) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(OUTPUT_CHARS)).collect()
}

/// What a step decided: passed, failed (a try), or the run must stop for a person.
enum Decision {
    Pass(Value),
    Fail(Value),
    Review(Value),
}

impl SkillRuns {
    fn runner(&self) -> Result<CheckRunner, String> {
        self.runner
            .get()
            .cloned()
            .ok_or_else(|| "skill procedures are not available in this host".to_string())
    }

    fn place<'a>(&'a self, diff: Option<&'a crate::workspace::WorkspaceDiff>, signal: &'a Signal) -> CheckPlace<'a> {
        CheckPlace {
            cwd: Some(&self.cwd),
            env: &self.env,
            diff,
            signal,
        }
    }

    /// Run `checks` (the segment's boundary first) against what changed since `baseline`.
    fn evaluate(
        &self,
        writes: &Option<Vec<String>>,
        checks: Vec<HostCheck>,
        baseline: &WorkspaceSnapshot,
        signal: &Signal,
    ) -> Result<Value, String> {
        let mut all = Vec::new();
        if let Some(globs) = writes {
            all.push(HostCheck::ChangedFilesWithin(globs.clone()));
        }
        all.extend(checks);
        let diff = diff_snapshots(baseline, &snapshot_workspace(&self.cwd))?;
        let contract = SpawnContract {
            role: None,
            checks: all,
            returns: None,
            hop: Hop::Report,
        };
        Ok(contract.evaluate(None, &self.place(Some(&diff), signal), &self.runner()?))
    }

    fn record(&self, run: &Run, change: Value) -> Result<(), String> {
        let base = json!({"id": run.id});
        self.skills.upsert(RUNS, &base, &change)
    }

    /// Begin segment `run.index`: its requires, then a fresh picture. `Err(reason)`: it cannot begin.
    fn enter(&self, run: &mut Run, host: &dyn TaskHost, signal: &Signal) -> Result<(), String> {
        let segment = &run.procedure.segments[run.index];
        if !segment.requires_files.is_empty() {
            let report = SpawnContract {
                role: None,
                checks: vec![HostCheck::FilesExist(segment.requires_files.clone())],
                returns: None,
                hop: Hop::Report,
            }
            .evaluate(None, &self.place(None, signal), &self.runner()?);
            if report["verdict"] != "verified" {
                let evidence = report["checks"][0]["evidence"].as_str().unwrap_or_default();
                return Err(format!("segment {} requires files: {evidence}", segment.name));
            }
        }
        if let Some(needed) = segment.budget_usd {
            let left = host.usage()?.and_then(|status| status["cost"]["remainingUsd"].as_f64());
            if let Some(left) = left.filter(|left| *left < needed) {
                return Err(format!(
                    "segment {} requires ${needed} of budget and ${left} is left",
                    segment.name
                ));
            }
        }
        run.baseline = snapshot_workspace(&self.cwd);
        run.tries = 0;
        run.violations.clear();
        Ok(())
    }

    fn status(&self, run: &Run, outcome: &str) -> Value {
        let total = run.procedure.segments.len();
        json!({
            "run": run.id,
            "name": run.skill.name,
            "version": run.skill.version,
            "outcome": outcome,
            "segment": (outcome == "running").then(|| run.procedure.segments[run.index].brief(run.index, total)),
            "segments": run.procedure.segments.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            "tries_left": (outcome == "running")
                .then(|| run.procedure.segments[run.index].max_iters.saturating_sub(run.tries)),
        })
    }

    /// End `run` with `outcome`: its record is completed and the lane is free again.
    fn finish(&self, lane: &str, mut run: Run, outcome: &str, reason: Option<String>) -> Result<Value, String> {
        let ended = (self.now)();
        let outputs = jsrt::hash::sha256_hex(serde_json::to_string(&run.results).unwrap_or_default());
        self.record(
            &run,
            json!({"outcome": outcome, "reason": reason, "segments": run.results, "output_digest": outputs,
                   "ended_at": ended, "duration_ms": ended - run.started}),
        )?;
        jsrt::sync::lock(&self.book.runs).remove(lane);
        run.stepped = false;
        let mut status = self.status(&run, outcome);
        status["reason"] = json!(reason);
        status["output_digest"] = json!(outputs);
        Ok(status)
    }

    fn start(&self, request: &Request<'_>) -> Result<Value, String> {
        let payload = request.payload;
        kernel::ops::fields("procedure.start", payload)?;
        let name = jsrt::payload::nonempty_string(payload.get("name"), "name")?;
        let version = payload.get("version").and_then(Value::as_str);
        let input = payload.get("input").cloned().unwrap_or(Value::Null);
        if let Some(run) = jsrt::sync::lock(&self.book.runs).get(request.lane) {
            return Err(format!(
                "This lane is running skill {} (run {}, segment {}): finish it or abort it first",
                run.skill.name, run.id, run.procedure.segments[run.index].name
            ));
        }
        let skill = self.skills.pinned_skill(&name, version)?;
        let raw = procedure::raw_of(&skill.content)?
            .ok_or_else(|| format!("Skill {name} has no procedure: read it with skills.load and follow its steps"))?;
        let procedure = procedure::parse(&raw, &input).map_err(|error| format!("Skill {name}: {error}"))?;
        let started = (self.now)();
        let mut run = Run {
            id: jsrt::random::uuid_v4(),
            skill,
            procedure,
            index: 0,
            tries: 0,
            // Taken when the segment begins (`enter`).
            baseline: WorkspaceSnapshot {
                root: self.cwd.clone(),
                method: "walk",
                entries: Default::default(),
                complete: false,
            },
            violations: Vec::new(),
            stepped: true,
            results: Vec::new(),
            evidence: Value::Null,
            started,
        };
        let base = json!({
            "id": run.id,
            "at": started,
            "name": run.skill.name,
            "version": run.skill.version,
            "path": run.skill.file_path,
            "lane": request.lane,
            "caller_task_id": request.caller_task,
            "input_digest": jsrt::hash::sha256_hex(serde_json::to_string(&input).unwrap_or_default()),
            "outcome": "running",
            "segments": [],
        });
        self.skills.upsert(RUNS, &base, &json!({}))?;
        if let Err(reason) = self.enter(&mut run, request.host, request.signal) {
            return self.finish(request.lane, run, "blocked", Some(reason));
        }
        let status = self.status(&run, "running");
        jsrt::sync::lock(&self.book.runs).insert(request.lane.to_string(), run);
        Ok(status)
    }

    fn judge(&self, run: &Run, request: &Request<'_>, state: Option<&Value>) -> Result<Decision, String> {
        let segment = &run.procedure.segments[run.index];
        let Step::Judge { kind, questions, pass } = &segment.step else {
            unreachable!("a judge step")
        };
        let statement = match state {
            Some(Value::String(text)) if !text.trim().is_empty() => json!(text),
            Some(value) if !value.is_null() && !value.is_string() => value.clone(),
            _ => {
                return Err(format!(
                    "segment {} is a judgment: pass state=<your statement>",
                    segment.name
                ));
            }
        };
        let state = json!({"skill": run.skill.name, "segment": segment.name, "goal": segment.goal,
                           "statement": statement, "evidence": run.evidence});
        let payload: Map<String, Value> = json!({"state": state.to_string(), "questions": questions, "kind": kind})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let verdict = self.judge.handle(&Request {
            kind: "judge.decide",
            payload: &payload,
            lane: request.lane,
            caller_task: request.caller_task,
            host: request.host,
            signal: request.signal,
        })?;
        let rung = verdict["rung"].as_str().unwrap_or("abstained");
        let summary = json!({"rung": rung, "backend": verdict["backend"], "model": verdict["model"],
                             "answers": verdict["answers"], "reason": verdict["reason"], "pass": pass});
        if rung == "review" || rung == "abstained" {
            return Ok(Decision::Review(summary));
        }
        let holds = pass
            .iter()
            .all(|(question, wanted)| &verdict["answers"][question]["value"] == wanted);
        Ok(match holds {
            true => Decision::Pass(summary),
            false => Decision::Fail(summary),
        })
    }

    fn agent(&self, run: &Run, request: &Request<'_>) -> Result<Decision, String> {
        let segment = &run.procedure.segments[run.index];
        let Step::Agent { brief, model, checks } = &segment.step else {
            unreachable!("an agent step")
        };
        let mut prompt = format!("{brief}\n\nGoal: {}", segment.goal);
        if !checks.is_empty() || segment.writes.is_some() {
            let described: Vec<Value> = checks.iter().map(HostCheck::describe).collect();
            prompt.push_str(&format!(
                "\n\nWhen you end, the host checks your work itself: {}{}",
                json!(described),
                segment
                    .writes
                    .as_ref()
                    .map(|globs| format!("; change only files matching {}", json!(globs)))
                    .unwrap_or_default()
            ));
        }
        let task = request.host.spawn(
            &json!({
                "definition": "rlm-child@1",
                "input": {"prompt": prompt},
                "model": model,
                "key": format!("skills.run:{}:{}:{}", run.id, segment.name, run.tries),
            }),
            request.caller_task,
        )?;
        let task_id = task["id"].as_str().unwrap_or_default().to_string();
        let result = request.host.result(&task_id)?;
        let report = self.evaluate(&segment.writes, checks.clone(), &run.baseline, request.signal)?;
        let summary = json!({"task_id": task_id, "model": model, "status": result["status"],
                             "value": result["value"], "contract": report});
        Ok(
            match result["status"] == "succeeded" && report["verdict"] == "verified" {
                true => Decision::Pass(summary),
                false => Decision::Fail(summary),
            },
        )
    }

    fn step(&self, request: &Request<'_>) -> Result<Value, String> {
        let payload = request.payload;
        kernel::ops::fields("procedure.step", payload)?;
        let id = jsrt::payload::nonempty_string(payload.get("run"), "run")?;
        let named = jsrt::payload::nonempty_string(payload.get("segment"), "segment")?;
        // The run leaves the book while its step runs (a check may take minutes) and is put back after.
        let mut run = {
            let mut runs = jsrt::sync::lock(&self.book.runs);
            let current = runs
                .get(request.lane)
                .filter(|run| run.id == id)
                .ok_or_else(|| format!("No skill run {id} is in progress on this lane"))?;
            let at = &current.procedure.segments[current.index].name;
            if *at != named {
                return Err(format!("Run {id} is at segment {at}, not {named}"));
            }
            runs.remove(request.lane).expect("present")
        };
        let decided = self.decide(&mut run, request, payload.get("state"));
        run.stepped = true;
        let decision = match decided {
            Ok(decision) => decision,
            Err(error) => {
                jsrt::sync::lock(&self.book.runs).insert(request.lane.to_string(), run);
                return Err(error);
            }
        };
        self.advance(request, run, decision)
    }

    /// The segment's step, then the boundary violations the lane's cells showed since its last step.
    fn decide(&self, run: &mut Run, request: &Request<'_>, state: Option<&Value>) -> Result<Decision, String> {
        let segment = run.procedure.segments[run.index].clone();
        let decision = match &segment.step {
            Step::Check(check) => {
                let report = self.evaluate(&segment.writes, vec![check.clone()], &run.baseline, request.signal)?;
                let entry = report["checks"]
                    .as_array()
                    .and_then(|checks| checks.last())
                    .cloned()
                    .unwrap_or(Value::Null);
                run.evidence = json!({"segment": segment.name, "check": entry["check"], "exit_code": entry["exit_code"],
                                      "stdout": tail(entry["stdout"].as_str().unwrap_or_default()),
                                      "stderr": tail(entry["stderr"].as_str().unwrap_or_default())});
                match report["verdict"] == "verified" {
                    true => Decision::Pass(report),
                    false => Decision::Fail(report),
                }
            }
            Step::Judge { .. } => {
                let decision = self.judge(run, request, state)?;
                // A judgment changes no files: its boundary is checked as a check's is.
                match (decision, &segment.writes) {
                    (Decision::Pass(summary), Some(_)) => {
                        let report = self.evaluate(&segment.writes, Vec::new(), &run.baseline, request.signal)?;
                        match report["verdict"] == "verified" {
                            true => Decision::Pass(summary),
                            false => Decision::Fail(json!({"judge": summary, "contract": report})),
                        }
                    }
                    (decision, _) => decision,
                }
            }
            Step::Agent { .. } => self.agent(run, request)?,
        };
        let violations = std::mem::take(&mut run.violations);
        Ok(match (decision, violations.is_empty()) {
            (Decision::Pass(evidence), false) => Decision::Fail(json!({
                "evidence": evidence,
                "boundary": format!("cells changed files outside {}: {}", json!(segment.writes), violations.join(", ")),
            })),
            (decision, _) => decision,
        })
    }

    fn advance(&self, request: &Request<'_>, mut run: Run, decision: Decision) -> Result<Value, String> {
        let lane = request.lane;
        let segment = run.procedure.segments[run.index].clone();
        run.tries += 1;
        let (outcome, evidence) = match &decision {
            Decision::Pass(evidence) => ("passed", evidence.clone()),
            Decision::Fail(evidence) => ("failed", evidence.clone()),
            Decision::Review(evidence) => ("review", evidence.clone()),
        };
        let result = json!({"name": segment.name, "kind": segment.kind(), "tries": run.tries, "outcome": outcome});
        match decision {
            Decision::Review(_) => {
                run.results.push(result.clone());
                let mut status = self.finish(lane, run, "needs_review", Some("the judge is unsure".into()))?;
                status["step"] = json!({"passed": false, "evidence": evidence});
                Ok(status)
            }
            Decision::Fail(_) if run.tries < segment.max_iters => {
                self.record(&run, json!({"at_segment": segment.name, "segments": run.results}))?;
                let mut status = self.status(&run, "running");
                status["step"] = json!({"passed": false, "evidence": evidence});
                jsrt::sync::lock(&self.book.runs).insert(lane.to_string(), run);
                Ok(status)
            }
            Decision::Fail(_) => {
                run.results.push(result);
                let reason = format!("segment {} failed {} times", segment.name, segment.max_iters);
                let mut status = self.finish(lane, run, "failed", Some(reason))?;
                status["step"] = json!({"passed": false, "evidence": evidence});
                Ok(status)
            }
            Decision::Pass(_) => {
                run.results.push(result);
                let step = json!({"passed": true, "evidence": evidence});
                if run.index + 1 == run.procedure.segments.len() {
                    let mut status = self.final_check(lane, run, request.signal)?;
                    status["step"] = step;
                    return Ok(status);
                }
                run.index += 1;
                if let Err(reason) = self.enter(&mut run, request.host, request.signal) {
                    let mut status = self.finish(lane, run, "blocked", Some(reason))?;
                    status["step"] = step;
                    return Ok(status);
                }
                self.record(
                    &run,
                    json!({"at_segment": run.procedure.segments[run.index].name, "segments": run.results}),
                )?;
                let mut status = self.status(&run, "running");
                status["step"] = step;
                jsrt::sync::lock(&self.book.runs).insert(lane.to_string(), run);
                Ok(status)
            }
        }
    }

    /// The host runs the final check again; the run passes only if it passes again.
    fn final_check(&self, lane: &str, run: Run, signal: &Signal) -> Result<Value, String> {
        let Some((index, check)) = run.procedure.final_check().map(|(index, check)| (index, check.clone())) else {
            return self.finish(lane, run, "passed", None);
        };
        let report = SpawnContract {
            role: None,
            checks: vec![check],
            returns: None,
            hop: Hop::Report,
        }
        .evaluate(None, &self.place(None, signal), &self.runner()?);
        if report["verdict"] == "verified" {
            return self.finish(lane, run, "passed", None);
        }
        let segment = run.procedure.segments[index].name.clone();
        let reason = format!(
            "segment {segment}'s check passed during the run but not when the host ran it again at the end: {}",
            report["failed"]
        );
        self.finish(lane, run, "final_check_mismatch", Some(reason))
    }

    fn abort(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("procedure.abort", request.payload)?;
        let id = jsrt::payload::nonempty_string(request.payload.get("run"), "run")?;
        let reason = request
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let run = {
            let mut runs = jsrt::sync::lock(&self.book.runs);
            match runs.get(request.lane) {
                Some(run) if run.id == id => runs.remove(request.lane).expect("present"),
                _ => return Err(format!("No skill run {id} is in progress on this lane")),
            }
        };
        self.finish(request.lane, run, "aborted", reason)
    }

    fn coverage(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("procedure.coverage", request.payload)?;
        let name = jsrt::payload::nonempty_string(request.payload.get("name"), "name")?;
        let skill = self.skills.pinned_skill(&name, None)?;
        let current = procedure::raw_of(&skill.content)?
            .map(|raw| procedure::validate(&raw))
            .transpose()?;
        Ok(procedure::coverage(
            &name,
            &skill.version,
            current.as_ref(),
            &self.skills.records(RUNS)?,
        ))
    }

    /// Marks the runs a dead worker left `running` (its book, which walked them, is gone) `interrupted`, so they
    /// do not stay running forever. Called when a session's stack opens, before any run of it starts; returns how
    /// many it marked.
    pub fn interrupt_orphans(&self) -> Result<usize, String> {
        let live: Vec<String> = jsrt::sync::lock(&self.book.runs)
            .values()
            .map(|run| run.id.clone())
            .collect();
        let ended = (self.now)();
        let mut marked = 0;
        for record in self.skills.records(RUNS)? {
            let Some(id) = record["id"].as_str().filter(|id| !live.iter().any(|live| live == id)) else {
                continue;
            };
            if record["outcome"] != "running" {
                continue;
            }
            let at = record["at"].as_i64().unwrap_or(ended);
            self.skills.upsert(
                RUNS,
                &json!({"id": id}),
                &json!({"outcome": "interrupted", "reason": "the worker running it stopped; its effects are unknown",
                        "ended_at": ended, "duration_ms": ended - at}),
            )?;
            marked += 1;
        }
        Ok(marked)
    }

    fn propose(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("procedure.propose", request.payload)?;
        let name = jsrt::payload::nonempty_string(request.payload.get("name"), "name")?;
        let content = jsrt::payload::nonempty_string(request.payload.get("content"), "content")?;
        let evidence = request.payload.get("evidence").cloned().unwrap_or(Value::Null);
        procedure::proposals::propose(
            &self.proposals,
            &name,
            &content,
            &evidence,
            self.session_id.as_deref(),
            (self.now)(),
        )
    }
}

impl ApiModule for SkillRuns {
    fn prefixes(&self) -> &[&str] {
        &["procedure."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "procedure.start" => self.start(request),
            "procedure.step" => self.step(request),
            "procedure.abort" => self.abort(request),
            "procedure.coverage" => self.coverage(request),
            "procedure.propose" => self.propose(request),
            other => Err(format!("Unsupported procedure request: {other}")),
        }
    }
}
