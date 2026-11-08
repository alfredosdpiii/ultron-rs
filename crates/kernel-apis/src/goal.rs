//! The session goal (`/goal`, port of `ultron/goal.ts` GoalDriver): a person sets one objective and a background job
//! (its own REPL, under the usage root `goal:<id>`) works on it until the goal ends.
//!
//! - Only a person sets, edits, pauses, resumes or clears the goal ([`GoalDriver::command`], the worker's
//!   `goal.command` request). The model reads it (`goal.get`), tests with the host's check (`goal.check`) and ends it
//!   (`goal.complete`, `goal.blocked`), naming the revision it worked on.
//! - `goal.complete` with a check command completes only if the host's own run of it exits 0; without one the
//!   completion is recorded as unverified. `goal.blocked` needs BLOCK_MIN_CHECKS checks when there is a check command.
//!   STUCK_CHECKS identical failing checks in a row pause the goal (and stop its job shortly after the reply).
//! - A job that ends while the goal is active starts again only if its run changed the check result
//!   ([`GoalDriver::task_ended`]); otherwise the goal pauses.
//! - Durable (Rust): a goal's job is a task on the task journal, started with the idempotent key `<goal id>:<run>`.
//!   When the worker reopens ([`GoalDriver::recover`]) the goal is reconciled with the journal through
//!   [`GoalJobs::job_state`] and [`GoalJobs::job_with_key`]: a run admitted but not yet recorded is adopted, a job
//!   still running is kept, a job the restart interrupted is followed by the next run (the goal stays active), and a
//!   job that ended while no worker ran is settled as [`GoalDriver::task_ended`] would have. Checks, the stuck count
//!   and progress are in the goal document, written with every change. A worker whose `GoalJobs` keeps no journal
//!   (`job_state` is `None`) behaves as TypeScript: an active goal whose job was running loads as paused.
//!
//! The document `{version: 1, goal}` is the session value `ultron.module/goal`; a failed write is the error of the
//! command, request or task end that made it. Each change of the goal is made under one lock, and the job calls it
//! decides (start, stop) and a check's run are made with the lock released: stopping a job ends it on the calling
//! thread and its end comes back through [`GoalDriver::task_ended`]. A start in flight is named by its run key; a
//! job whose goal moved on meanwhile is stopped, and a job that ended before its start returned is settled then.

use crate::store::read_versioned;
use crate::util::Clock;
use crate::{ApiModule, Request, TaskHost};
use fancy_regex::Regex;
use kernel::Signal;
use kernel::store::DocumentStore;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Failing checks in a row with the same result that pause the goal.
pub const STUCK_CHECKS: i64 = 10;
/// Checks needed before the model may declare the goal blocked (when there is a check command).
pub const BLOCK_MIN_CHECKS: i64 = 3;
/// A hanging check command is stopped after this long.
pub const CHECK_TIMEOUT_SECONDS: u64 = 600;
const CHECK_OUTPUT_CHARS: usize = 2000;
const TEXT_MAX_CHARS: usize = 4000;
const EVIDENCE_MAX: usize = 20;
/// Usage root prefix of goal jobs (`GOAL_USAGE_ROOT_PREFIX` of usage.ts).
pub const GOAL_USAGE_ROOT_PREFIX: &str = "goal:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheckResult {
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub output: String,
}

/// What the task journal says about a goal job.
#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    /// Admitted and not ended (it runs under this worker).
    Running,
    /// Ended, with its durable result (`NativeResult`; `status: "interrupted"` when a restart cut it off).
    Ended(Value),
}

/// The task host's error for work it stopped because it closed (`HOST_CLOSED` of native-host.ts).
pub const HOST_CLOSED: &str = "Ultron task host closed";

/// A job result that says the worker went away under the run (a restart interrupted it, or its host closed), not
/// that the run itself ended.
fn cut_off(result: &Value) -> bool {
    result["status"] == "interrupted" || (result["status"] == "cancelled" && result["error"] == HOST_CLOSED)
}

/// What the driver needs from its worker: run the check in the session's working directory, start and stop the
/// goal's background job, and name the lane a job runs on.
pub trait GoalJobs: Send + Sync {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String>;
    /// Starts the goal's background job with `prompt`; returns its task id.
    fn start_job(&self, prompt: &str, key: &str, usage_root: &str) -> Result<String, String>;
    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String>;
    fn lane_of_job(&self, task_id: &str) -> Option<String>;
    /// The journal's state of a job task; `None` when the worker keeps no task journal.
    fn job_state(&self, _task_id: &str) -> Option<JobState> {
        None
    }
    /// The task admitted with `key`, if any.
    fn job_with_key(&self, _key: &str) -> Option<String> {
        None
    }
}

fn regexes() -> &'static [(Regex, &'static str); 3] {
    static CELL: OnceLock<[(Regex, &'static str); 3]> = OnceLock::new();
    CELL.get_or_init(|| {
        [
            (Regex::new(r"(?i)0x[0-9a-f]+").expect("regex"), "0x"),
            (
                Regex::new(r"(?<![A-Za-z0-9_])[0-9]+(?:\.[0-9]+)?\s?(?:ms|s|sec|secs|seconds)(?![A-Za-z0-9_])")
                    .expect("regex"),
                "<t>",
            ),
            (Regex::new(r"\s+").expect("regex"), " "),
        ]
    })
}

/// A check result's fingerprint: exit status and output, with durations and addresses left out.
pub fn check_signature(result: &GoalCheckResult) -> String {
    let mut output = jsrt::utf16::tail(&result.output, CHECK_OUTPUT_CHARS).to_string();
    for (pattern, replacement) in regexes() {
        output = pattern.replace_all(&output, *replacement).into_owned();
    }
    let status = if result.timed_out {
        "timeout".to_string()
    } else {
        result.exit_code.map_or("null".to_string(), |code| code.to_string())
    };
    jsrt::hash::sha256_hex(format!("{status}\n{}", jsrt::string::trim(&output)).as_bytes())[..16].to_string()
}

fn text_of(goal: &Value, key: &str) -> String {
    goal[key].as_str().unwrap_or_default().to_string()
}

fn int_of(goal: &Value, key: &str) -> i64 {
    goal[key].as_i64().unwrap_or(0)
}

/// The brief of the goal's background job.
pub fn goal_brief(goal: &Value) -> String {
    let revision = &goal["revision"];
    let check = match goal["check"].as_str() {
        None => "There is no check command: when you call goal.complete, give concrete evidence (it is recorded as unverified).".to_string(),
        Some(command) => format!(
            "Check: `{command}`. `await goal.check()` runs it on the host and returns {{passed, exit_code, output}}; goal.complete runs it again and succeeds only if it exits 0."
        ),
    };
    let mut parts = vec![
        format!(
            "You are working on the session goal (revision {revision}) on your own, as a background job: nobody is waiting on your turns, and nothing limits your time."
        ),
        format!("Objective: {}", text_of(goal, "objective")),
        check,
        "Work in your REPL: keep what you tried and learned in Python (`state['goal']`), change the code, test, and use the result to choose the next step. Delegate independent attempts with `rlm.spawn(brief, name=..., worktree=True)` and `await rlm.merge(...)` the one that works.".to_string(),
        format!(
            "Keep going until `await goal.complete({revision}, summary, evidence=[...])` succeeds. Only if something outside you blocks the goal after real attempts: `await goal.blocked({revision}, reason)`. Ten identical failing checks in a row pause the goal, so change your approach when a check keeps failing the same way. The user may change the goal: `await goal.get()` shows the current revision."
        ),
    ];
    if let Some(answer) = goal["last_answer"].as_str() {
        parts.push(format!("The previous run on this goal ended with: {answer}"));
    }
    parts.join("\n\n")
}

/// A short status for `/goal`.
pub fn goal_status_text(goal: Option<&Value>) -> String {
    let Some(goal) = goal else {
        return "No goal. Set one with /goal <objective>.".into();
    };
    let reason = goal["reason"]
        .as_str()
        .filter(|r| !r.is_empty())
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    let stuck = int_of(goal, "stuck");
    let mut lines = vec![
        format!(
            "Goal (revision {}, {}{reason}): {}",
            goal["revision"],
            text_of(goal, "status"),
            text_of(goal, "objective")
        ),
        format!(
            "Check: {}. Runs: {}. Checks: {}{}.",
            goal["check"].as_str().unwrap_or("none (completion is unverified)"),
            goal["runs"],
            goal["checks"],
            if stuck > 1 {
                format!(" ({stuck} identical failures in a row)")
            } else {
                String::new()
            }
        ),
    ];
    if let Some(job) = goal["job"].as_str() {
        lines.push(format!("Job: {job} (running)."));
    }
    let last = &goal["last_check"];
    if last.is_object() {
        let outcome = if last["passed"] == true {
            "passed".to_string()
        } else if last["timed_out"] == true {
            "timed out".to_string()
        } else {
            format!("exit {}", last["exit_code"])
        };
        lines.push(format!("Last check: {outcome} ({}).", text_of(last, "command")));
    }
    let completion = &goal["completion"];
    if completion.is_object() {
        lines.push(format!(
            "Completed{}: {}",
            if completion["verified"] == true {
                " (check passed)"
            } else {
                " (unverified)"
            },
            text_of(completion, "summary")
        ));
    }
    lines.join("\n")
}

fn public_goal(goal: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "revision",
        "objective",
        "check",
        "status",
        "reason",
        "runs",
        "checks",
        "stuck",
        "last_check",
        "completion",
    ] {
        out.insert(key.into(), goal[key].clone());
    }
    Value::Object(out)
}

fn text(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !jsrt::string::trim(text).is_empty() => Ok(jsrt::utf16::head(text, TEXT_MAX_CHARS).to_string()),
        _ => Err(format!("{name} must be a non-empty string")),
    }
}

fn evidence_list(value: Option<&Value>) -> Result<Vec<Value>, String> {
    let items = value.and_then(Value::as_array).filter(|items| !items.is_empty()).ok_or(
        "evidence must be a non-empty list of strings: commands with their outcome (\"npm test: exit 0\") or files with lines",
    )?;
    if !items
        .iter()
        .all(|item| item.as_str().is_some_and(|text| !jsrt::string::trim(text).is_empty()))
    {
        return Err("every evidence item must be a non-empty string".into());
    }
    Ok(items
        .iter()
        .take(EVIDENCE_MAX)
        .map(|item| Value::from(jsrt::utf16::head(item.as_str().unwrap_or_default(), TEXT_MAX_CHARS)))
        .collect())
}

fn tail(output: &str) -> String {
    if jsrt::utf16::len(output) > CHECK_OUTPUT_CHARS {
        format!("…{}", jsrt::utf16::tail(output, CHECK_OUTPUT_CHARS - 1))
    } else {
        output.to_string()
    }
}

/// A [`GoalJobs`] call decided under the driver's lock and made after it is released: stopping a running job ends it
/// on the calling thread, and its end comes back through [`GoalDriver::task_ended`], which takes the lock.
enum Effect {
    Stop {
        job: String,
        reason: String,
    },
    Start {
        key: String,
        brief: String,
        usage_root: String,
    },
}

struct State {
    goal: Option<Value>,
    loaded: bool,
    closed: bool,
    /// Calls to make once the lock is released.
    effects: Vec<Effect>,
    /// Run keys (`<goal id>:<run>`) whose job is being started, outside the lock.
    starting: Vec<String>,
    /// Task ends that arrived while a start was in flight: one may be the job being started, which ended before its
    /// id was recorded.
    early_ends: Vec<(String, Option<Value>)>,
}

/// What `/goal` shows: a fixed text, or the goal's status (after the calls it made), with a note.
enum Reply {
    Text(String),
    Status(&'static str),
}

/// A `goal.*` request that needs the check run: what it was decided against, re-checked once the run returns.
struct PendingCheck {
    id: String,
    command: String,
}

fn run_key(goal: &Value) -> String {
    format!("{}:{}", text_of(goal, "id"), goal["runs"])
}

/// Takes the goal's job and stops it after the lock is released.
fn stop_job(goal: &mut Value, effects: &mut Vec<Effect>, reason: &str) {
    if let Some(job) = goal["job"].as_str().map(str::to_string) {
        effects.push(Effect::Stop {
            job,
            reason: reason.into(),
        });
    }
    goal["job"] = Value::Null;
}

pub struct GoalDriver {
    store: Arc<dyn DocumentStore>,
    jobs: Arc<dyn GoalJobs>,
    now: Clock,
    state: Mutex<State>,
}

impl GoalDriver {
    pub fn new(store: Arc<dyn DocumentStore>, jobs: Arc<dyn GoalJobs>, now: Clock) -> Self {
        Self {
            store,
            jobs,
            now,
            state: Mutex::new(State {
                goal: None,
                loaded: false,
                closed: false,
                effects: Vec::new(),
                starting: Vec::new(),
                early_ends: Vec::new(),
            }),
        }
    }

    fn now(&self) -> i64 {
        (self.now)()
    }

    /// Runs `step` on the loaded goal under the lock, then makes the [`GoalJobs`] calls it decided with the lock
    /// released. A failed write of the document is the call's error, even when the step itself succeeded.
    fn locked<R>(&self, step: impl FnOnce(&Self, &mut State) -> Result<R, String>) -> Result<R, String> {
        let (result, effects) = {
            let mut state = jsrt::sync::lock(&self.state);
            let result = self.load(&mut state).and_then(|()| step(self, &mut state));
            (result, std::mem::take(&mut state.effects))
        };
        let applied = self.apply(effects);
        let result = result?;
        applied.map(|()| result)
    }

    /// Makes the calls, then records what each start returned (which may decide more calls).
    fn apply(&self, mut effects: Vec<Effect>) -> Result<(), String> {
        let mut outcome = Ok(());
        while !effects.is_empty() {
            for effect in std::mem::take(&mut effects) {
                match effect {
                    Effect::Stop { job, reason } => {
                        let _ = self.jobs.stop_job(&job, &reason);
                    }
                    Effect::Start { key, brief, usage_root } => {
                        let started = self.jobs.start_job(&brief, &key, &usage_root);
                        let mut state = jsrt::sync::lock(&self.state);
                        let recorded = self.started(&mut state, &key, started);
                        effects.extend(std::mem::take(&mut state.effects));
                        outcome = outcome.and(recorded);
                    }
                }
            }
        }
        outcome
    }

    /// A start returned: its job is the goal's when the goal still waits on that run, and is stopped otherwise (the
    /// goal was paused, cleared or replaced while it started).
    fn started(&self, state: &mut State, key: &str, started: Result<String, String>) -> Result<(), String> {
        state.starting.retain(|starting| starting != key);
        let mut early = if state.starting.is_empty() {
            std::mem::take(&mut state.early_ends)
        } else {
            Vec::new()
        };
        let waiting = state
            .goal
            .as_ref()
            .is_some_and(|goal| goal["status"] == "active" && goal["job"].is_null() && run_key(goal) == key);
        let result = match started {
            Ok(job) if !waiting => {
                state.effects.push(Effect::Stop {
                    job,
                    reason: "goal changed".into(),
                });
                Ok(())
            }
            Ok(job) => {
                state.goal.as_mut().expect("a goal")["job"] = job.as_str().into();
                let persisted = self.persist(state);
                let ended = early.iter().position(|(id, _)| *id == job).map(|at| early.remove(at));
                match ended {
                    Some((id, result)) => persisted.and(self.job_ended(state, &id, result.as_ref())),
                    None => persisted,
                }
            }
            Err(error) if waiting => {
                let goal = state.goal.as_mut().expect("a goal");
                goal["status"] = "paused".into();
                goal["reason"] = format!("the goal job could not start: {error}").into();
                self.persist(state)
            }
            Err(_) => Ok(()),
        };
        if !state.starting.is_empty() {
            // Kept for the starts still in flight.
            state.early_ends.extend(early);
        }
        result
    }

    fn load(&self, state: &mut State) -> Result<(), String> {
        if state.loaded {
            return Ok(());
        }
        state.loaded = true;
        let saved = read_versioned("ultron.module/goal", self.store.read()?, "version")?;
        let goal = saved.and_then(|saved| saved.get("goal").filter(|goal| goal.is_object()).cloned());
        if state.goal.is_none() {
            state.goal = goal;
        }
        self.reconcile(state)
    }

    /// Reconciles an active goal with the task journal after a reopen (see the module docs). The journal lookups
    /// only read it and never call back into the driver, so they run under the lock.
    fn reconcile(&self, state: &mut State) -> Result<(), String> {
        let Some(goal) = state.goal.as_mut().filter(|goal| goal["status"] == "active") else {
            return Ok(());
        };
        let mut changed = false;
        if goal["job"].is_null() {
            // An active goal without a job was starting a run when the worker died: the run is written before it is
            // admitted, so its key names the task if the admission happened.
            match self.jobs.job_with_key(&run_key(goal)) {
                Some(task) => {
                    goal["job"] = task.into();
                    changed = true;
                }
                None => return self.start_job(state),
            }
        }
        let Some(job) = goal["job"].as_str().map(str::to_string) else {
            return Ok(());
        };
        match self.jobs.job_state(&job) {
            Some(JobState::Running) if changed => self.persist(state),
            Some(JobState::Running) => Ok(()),
            Some(JobState::Ended(result)) if cut_off(&result) => {
                // The worker going away cut the run off, not the job's own work: the goal goes on with its next run.
                goal["job"] = Value::Null;
                goal["last_answer"] = "The previous run was interrupted when Ultron restarted; continue from the goal's state and the files.".into();
                self.start_job(state)
            }
            Some(JobState::Ended(result)) => self.job_ended(state, &job, Some(&result)),
            None => {
                // No journal: a job does not outlive the worker that ran it, so the goal pauses (TypeScript).
                goal["job"] = Value::Null;
                goal["status"] = "paused".into();
                goal["reason"] = "Ultron restarted while the goal job ran; /goal resume continues".into();
                Ok(())
            }
        }
    }

    /// Loads the goal and reconciles it with the task journal (the worker's open); returns it.
    pub fn recover(&self) -> Result<Option<Value>, String> {
        self.get()
    }

    /// Writes the document.
    fn persist(&self, state: &State) -> Result<(), String> {
        self.store
            .write(&json!({"version": 1, "goal": state.goal}))
            .map_err(|error| format!("the session goal could not be saved: {error}"))
    }

    /// The goal now (a copy), after the document loads.
    pub fn get(&self) -> Result<Option<Value>, String> {
        self.locked(|_, _| Ok(()))?;
        Ok(jsrt::sync::lock(&self.state).goal.clone())
    }

    /// Stops starting jobs (shutdown).
    pub fn close(&self) {
        jsrt::sync::lock(&self.state).closed = true;
    }

    /// Writes the goal's next run, then has its job started once the lock is released.
    fn start_job(&self, state: &mut State) -> Result<(), String> {
        if state.closed {
            return Ok(());
        }
        let now = self.now();
        let goal = state.goal.as_mut().expect("a goal");
        goal["runs"] = (int_of(goal, "runs") + 1).into();
        goal["run_progress"] = false.into();
        goal["updated_at"] = now.into();
        goal["job"] = Value::Null;
        let key = run_key(goal);
        // The run is durable before its task is admitted, so a reopen finds the task by this key; a run that could
        // not be written is not started.
        if let Err(error) = self.persist(state) {
            let goal = state.goal.as_mut().expect("a goal");
            goal["status"] = "paused".into();
            goal["reason"] = format!("{error}; /goal resume retries").into();
            return Err(error);
        }
        let goal = state.goal.as_ref().expect("a goal");
        let effect = Effect::Start {
            key: key.clone(),
            brief: goal_brief(goal),
            usage_root: format!("{GOAL_USAGE_ROOT_PREFIX}{}", text_of(goal, "id")),
        };
        state.starting.push(key);
        state.effects.push(effect);
        Ok(())
    }

    fn set_status(&self, state: &mut State, status: &str, reason: Option<&str>) -> Result<(), String> {
        let now = self.now();
        let goal = state.goal.as_mut().expect("a goal");
        goal["status"] = status.into();
        goal["reason"] = reason.map_or(Value::Null, Value::from);
        goal["updated_at"] = now.into();
        self.persist(state)
    }

    /// `/goal [objective | pause | resume | clear | check [command] | status]`; returns the text to show.
    pub fn command(&self, args: &str) -> Result<String, String> {
        let text = jsrt::string::trim(args);
        let reply = self.locked(|this, state| this.command_step(state, text))?;
        Ok(match reply {
            Reply::Text(text) => text,
            Reply::Status(note) => goal_status_text(jsrt::sync::lock(&self.state).goal.as_ref()) + note,
        })
    }

    fn command_step(&self, state: &mut State, text: &str) -> Result<Reply, String> {
        if text.is_empty() || text == "status" {
            return Ok(Reply::Status(""));
        }
        if text == "clear" {
            let Some(goal) = state.goal.as_mut() else {
                return Ok(Reply::Text("No goal to clear.".into()));
            };
            stop_job(goal, &mut state.effects, "goal cleared");
            state.goal = None;
            self.persist(state)?;
            return Ok(Reply::Text("Goal cleared.".into()));
        }
        if text == "pause" {
            match state.goal.as_mut() {
                Some(goal) if goal["status"] == "active" => stop_job(goal, &mut state.effects, "goal paused"),
                _ => return Ok(Reply::Status("")),
            }
            self.set_status(state, "paused", Some("paused by you; /goal resume continues"))?;
            return Ok(Reply::Status(""));
        }
        if text == "resume" {
            match state.goal.as_ref() {
                Some(goal) if goal["status"] != "complete" && goal["status"] != "active" => {}
                _ => return Ok(Reply::Status("")),
            }
            self.set_status(state, "active", None)?;
            state.goal.as_mut().expect("a goal")["stuck"] = 0.into();
            self.start_job(state)?;
            return Ok(Reply::Status(""));
        }
        if text == "check" || text.starts_with("check ") {
            let now = self.now();
            let Some(goal) = state.goal.as_mut() else {
                return Ok(Reply::Text("Set a goal first: /goal <objective>.".into()));
            };
            let command = jsrt::string::trim(&text["check".len()..]);
            goal["check"] = if command.is_empty() {
                Value::Null
            } else {
                jsrt::utf16::head(command, TEXT_MAX_CHARS).into()
            };
            goal["revision"] = (int_of(goal, "revision") + 1).into();
            goal["stuck"] = 0.into();
            goal["last_signature"] = Value::Null;
            goal["updated_at"] = now.into();
            self.persist(state)?;
            return Ok(Reply::Status(""));
        }
        let mut revision = 1;
        if let Some(goal) = state.goal.as_mut() {
            stop_job(goal, &mut state.effects, "goal replaced");
            revision = int_of(goal, "revision") + 1;
        }
        let now = self.now();
        state.goal = Some(json!({
            "id": format!("goal-{}", &jsrt::random::uuid_v4()[..8]),
            "revision": revision,
            "objective": jsrt::utf16::head(text, TEXT_MAX_CHARS),
            "check": null,
            "status": "active",
            "reason": null,
            "job": null,
            "runs": 0,
            "checks": 0,
            "stuck": 0,
            "last_signature": null,
            "run_progress": false,
            "last_check": null,
            "last_answer": null,
            "created_at": now,
            "updated_at": now,
            "completion": null,
        }));
        self.start_job(state)?;
        Ok(Reply::Status(
            "\nAdd the check that decides when it is done: /goal check <command>.",
        ))
    }

    /// A task ended (the worker's task-end observer; `result` is its `NativeResult`): a goal job that ended early is
    /// started again when its run made progress, otherwise the goal pauses.
    pub fn task_ended(&self, task_id: &str, result: Option<&Value>) -> Result<(), String> {
        self.locked(|this, state| this.job_ended(state, task_id, result))
    }

    fn job_ended(&self, state: &mut State, task_id: &str, result: Option<&Value>) -> Result<(), String> {
        let now = self.now();
        let Some(goal) = state.goal.as_mut() else {
            return Ok(());
        };
        if state.closed {
            return Ok(());
        }
        if goal["job"] != task_id {
            if !state.starting.is_empty() {
                state.early_ends.push((task_id.to_string(), result.cloned()));
            }
            return Ok(());
        }
        goal["job"] = Value::Null;
        let answer = match result.map(|result| (&result["value"], result.get("error"))) {
            Some((Value::String(value), _)) => value.clone(),
            Some((_, Some(error))) if !error.is_null() => jsrt::value::string(Some(error)),
            _ => String::new(),
        };
        goal["last_answer"] = if jsrt::utf16::len(&answer) > 1000 {
            format!("{}…", jsrt::utf16::head(&answer, 999)).into()
        } else if answer.is_empty() {
            Value::Null
        } else {
            answer.into()
        };
        goal["updated_at"] = now.into();
        if goal["status"] != "active" {
            return self.persist(state);
        }
        if goal["run_progress"] == true {
            return self.start_job(state);
        }
        self.set_status(
            state,
            "paused",
            Some("the goal job ended without progress; /goal resume continues"),
        )
    }

    /// Records a check's result on the goal it ran for.
    fn record_check(&self, state: &mut State, command: String, result: GoalCheckResult) -> Result<Value, String> {
        let goal = state.goal.as_mut().expect("a goal");
        let passed = result.exit_code == Some(0) && !result.timed_out;
        let signature = check_signature(&result);
        let same = goal["last_signature"] == signature.as_str();
        if !same {
            goal["run_progress"] = true.into();
        }
        goal["stuck"] = (if passed {
            0
        } else if same {
            int_of(goal, "stuck") + 1
        } else {
            1
        })
        .into();
        goal["last_signature"] = signature.into();
        goal["checks"] = (int_of(goal, "checks") + 1).into();
        let now = self.now();
        goal["last_check"] = json!({
            "command": command,
            "passed": passed,
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
            "output": tail(&result.output),
            "at": now,
        });
        goal["updated_at"] = now.into();
        let run = goal["last_check"].clone();
        self.persist(state)?;
        Ok(run)
    }

    /// The model's call must name the active goal's current revision.
    fn current(goal: &Value, revision: Option<&Value>) -> Result<(), String> {
        if goal["status"] != "active" {
            return Err(format!("The session goal is {}, not active", text_of(goal, "status")));
        }
        let matches = revision
            .and_then(Value::as_f64)
            .zip(goal["revision"].as_f64())
            .is_some_and(|(a, b)| a == b);
        if !matches {
            return Err(format!(
                "The goal is at revision {} (the user changed it): read it with goal.get() and work on that",
                goal["revision"]
            ));
        }
        Ok(())
    }

    /// One `goal.*` request from `lane`. A check runs with the lock released (it can take CHECK_TIMEOUT_SECONDS),
    /// and its result is recorded only on the goal it ran for.
    pub fn handle_request(&self, kind: &str, payload: &Map<String, Value>, lane: &str) -> Result<Value, String> {
        let check = match self.locked(|this, state| this.request_step(state, kind, payload, lane))? {
            Ok(done) => return Ok(done),
            Err(check) => check,
        };
        let result = self.jobs.run_check(&check.command, CHECK_TIMEOUT_SECONDS)?;
        self.locked(|this, state| {
            let same = state
                .goal
                .as_ref()
                .is_some_and(|goal| text_of(goal, "id") == check.id && goal["check"] == check.command.as_str());
            if !same {
                return Err("The session goal changed while its check ran: read it with goal.get()".into());
            }
            if kind == "goal.complete" {
                Self::current(state.goal.as_ref().expect("a goal"), payload.get("revision"))?;
            }
            let run = this.record_check(state, check.command, result)?;
            this.checked(state, kind, payload, run)
        })
    }

    /// The part of a request before its check: `Ok` is the reply when no check is needed.
    fn request_step(
        &self,
        state: &mut State,
        kind: &str,
        payload: &Map<String, Value>,
        lane: &str,
    ) -> Result<Result<Value, PendingCheck>, String> {
        if kind == "goal.get" {
            kernel::ops::fields("goal.get", payload)?;
            return Ok(Ok(state.goal.as_ref().map_or(Value::Null, public_goal)));
        }
        let goal = state.goal.as_ref().ok_or("There is no session goal")?;
        let job = goal["job"].as_str().and_then(|job| self.jobs.lane_of_job(job));
        if lane != kernel::ROOT_LANE && Some(lane) != job.as_deref() {
            return Err("Only the goal's job or the root agent works the session goal; subagents report to it".into());
        }
        let pending = || PendingCheck {
            id: text_of(goal, "id"),
            command: text_of(goal, "check"),
        };
        match kind {
            "goal.check" => {
                kernel::ops::fields("goal.check", payload)?;
                if goal["check"].is_null() {
                    return Err("This goal has no check command (the user sets one with /goal check)".into());
                }
                Ok(Err(pending()))
            }
            "goal.complete" => {
                kernel::ops::fields("goal.complete", payload)?;
                Self::current(goal, payload.get("revision"))?;
                text(payload.get("summary"), "summary")?;
                evidence_list(payload.get("evidence"))?;
                if goal["check"].is_null() {
                    return self.complete(state, payload, false).map(Ok);
                }
                Ok(Err(pending()))
            }
            "goal.blocked" => {
                kernel::ops::fields("goal.blocked", payload)?;
                Self::current(goal, payload.get("revision"))?;
                let reason = text(payload.get("reason"), "reason")?;
                let checks = int_of(goal, "checks");
                if !goal["check"].is_null() && checks < BLOCK_MIN_CHECKS {
                    return Err(format!(
                        "goal.blocked is accepted after {BLOCK_MIN_CHECKS} checks ({checks} so far): keep working, test with goal.check(), and try another approach"
                    ));
                }
                self.set_status(state, "blocked", Some(&reason))?;
                Ok(Ok(json!({"status": "blocked", "reason": reason})))
            }
            _ => Err(kernel::ops::not_wired(kind)),
        }
    }

    /// The part of a request after its check ran (`run` is the recorded check).
    fn checked(
        &self,
        state: &mut State,
        kind: &str,
        payload: &Map<String, Value>,
        mut run: Value,
    ) -> Result<Value, String> {
        if kind == "goal.complete" {
            if run["passed"] != true {
                return Ok(json!({
                    "complete": false,
                    "status": state.goal.as_ref().expect("a goal")["status"],
                    "check": run,
                    "message": "The check failed, so the goal is not complete: fix the cause and call goal.complete again.",
                }));
            }
            return self.complete(state, payload, true);
        }
        let goal = state.goal.as_ref().expect("a goal");
        if goal["status"] == "active" && int_of(goal, "stuck") >= STUCK_CHECKS {
            let stuck = int_of(goal, "stuck");
            self.set_status(
                state,
                "paused",
                Some(&format!(
                    "{stuck} failing checks in a row gave the same result; /goal resume continues"
                )),
            )?;
            let goal = state.goal.as_mut().expect("a goal");
            // Stopped after this reply reaches the job's cell.
            if let Some(stopping) = goal["job"].as_str().map(str::to_string) {
                let jobs = self.jobs.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    let _ = jobs.stop_job(&stopping, "goal stuck");
                });
            }
            goal["job"] = Value::Null;
            run["paused"] = true.into();
            run["reason"] = goal["reason"].clone();
            return Ok(run);
        }
        run["paused"] = false.into();
        Ok(run)
    }

    fn complete(&self, state: &mut State, payload: &Map<String, Value>, verified: bool) -> Result<Value, String> {
        let summary = text(payload.get("summary"), "summary")?;
        let evidence = evidence_list(payload.get("evidence"))?;
        let now = self.now();
        state.goal.as_mut().expect("a goal")["completion"] =
            json!({"summary": summary, "evidence": evidence, "verified": verified, "at": now});
        self.set_status(state, "complete", None)?;
        let goal = state.goal.as_ref().expect("a goal");
        Ok(json!({"complete": true, "status": goal["status"], "verified": verified, "check": goal["last_check"]}))
    }
}

impl ApiModule for GoalDriver {
    fn prefixes(&self) -> &[&str] {
        &["goal."]
    }

    fn start(&self, _host: &dyn TaskHost) -> Result<(), String> {
        self.locked(|_, _| Ok(()))
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.handle_request(request.kind, request.payload, request.lane)
    }
}

/// [`GoalJobs`] over the worker's task journal and shell: a goal job is a `background-job@1` task admitted through
/// [`TaskHost::spawn`] with the goal's run key, and a check is the host's blocking `bash`.
pub struct TaskGoalJobs {
    tasks: Arc<dyn crate::TaskHost>,
    shell: Arc<dyn kernel::HostHandler>,
}

impl TaskGoalJobs {
    pub fn new(tasks: Arc<dyn crate::TaskHost>, shell: Arc<dyn kernel::HostHandler>) -> TaskGoalJobs {
        TaskGoalJobs { tasks, shell }
    }

    fn task(&self, matches: impl Fn(&Value) -> bool) -> Option<Value> {
        self.tasks.tasks().ok()?.into_iter().find(matches)
    }
}

impl GoalJobs for TaskGoalJobs {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String> {
        let mut payload = Map::new();
        payload.insert("command".into(), command.into());
        payload.insert("timeout".into(), timeout_seconds.into());
        let reply = self.shell.handle("bash", payload, &Signal::new())?;
        Ok(GoalCheckResult {
            exit_code: reply["exit_code"].as_i64(),
            timed_out: reply["timed_out"] == true,
            output: reply["output"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// The usage root is not passed: [`TaskHost::spawn`] charges the job to its admission root.
    fn start_job(&self, prompt: &str, key: &str, _usage_root: &str) -> Result<String, String> {
        let request = json!({"definition": "background-job@1", "input": {"prompt": prompt}, "key": key});
        let task = self.tasks.spawn(&request, None)?;
        task["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "the task host returned no task id".into())
    }

    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        self.tasks.cancel(task_id, reason).map(|_| ())
    }

    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        self.tasks.task_lane(task_id)
    }

    fn job_state(&self, task_id: &str) -> Option<JobState> {
        let task = self.task(|task| task["id"] == task_id)?;
        Some(match task.get("result").filter(|result| !result.is_null()) {
            Some(result) => JobState::Ended(result.clone()),
            None => JobState::Running,
        })
    }

    fn job_with_key(&self, key: &str) -> Option<String> {
        self.task(|task| task["key"] == key)
            .and_then(|task| task["id"].as_str().map(str::to_string))
    }
}
