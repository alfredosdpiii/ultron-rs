//! Subagent spawning, worktree allocation, child execution, contract enforcement, and completion.

use super::*;

impl Delegation {
    pub(super) fn spawn(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        lane: &str,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["prompt", "kwargs"])?;
        let limit = self.inner.spawn_depth;
        let root = jsrt::sync::lock(&self.inner.root).clone();
        let (depth, allowance) = {
            let state = sync::lock(&self.inner.state);
            let children: Vec<String> = state.line(parent).into_iter().filter(|id| state.is_child(id)).collect();
            let allowance = children
                .first()
                .and_then(|id| state.task(id))
                .map_or(root.allowance.unwrap_or(i64::MAX), |spawner| {
                    i64::from(spawner.spawn_depth)
                });
            (i64::from(root.level) + children.len() as i64, allowance)
        };
        if limit > 0 && depth >= i64::from(limit) {
            return Err(format!(
                "rlm.spawn refused: subagents nest at most {limit} level(s) deep (ULTRON_SPAWN_DEPTH). Do this part yourself: narrow with code, read the candidates, and use rlm.map for passages you cannot settle."
            ));
        }
        if allowance < 1 {
            return Err("rlm.spawn refused: this subagent was started with depth=0, so it does its brief itself. A parent that wants nested delegation passes rlm.spawn(brief, name=..., depth=N).".into());
        }
        let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
        let empty = Map::new();
        let kwargs = match payload.get("kwargs") {
            None => &empty,
            Some(Value::Object(map)) => map,
            Some(_) => return Err("agent input must be a JSON object".into()),
        };
        jsrt::payload::fields(
            kwargs,
            &[
                "name",
                "model",
                "thinking",
                "timeout_ms",
                "depth",
                "worktree",
                "worktree_setup",
                "cwd",
                "fork",
                "context",
                "role",
                "contract",
                "hop",
            ],
        )?;
        let contract = SpawnContract::parse(kwargs)?.map(Arc::new);
        let context = SpawnContext::of(kwargs.get("context"), kwargs.get("fork"))?;
        let fork = context == SpawnContext::Fork;
        let fork_hook = jsrt::sync::lock(&self.inner.fork_point).clone();
        if fork && fork_hook.is_none() {
            return Err("rlm.spawn fork=True is not available when subagents run as external processes".into());
        }
        let name = nonempty_string(kwargs.get("name"), "name")?;
        let mode = match kwargs.get("worktree") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => WorktreeMode::Off,
            Some(Value::Bool(true)) => WorktreeMode::On,
            Some(Value::String(s)) if s == "auto" => WorktreeMode::Auto,
            Some(_) => {
                return Err(
                    "worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
                        .into(),
                );
            }
        };
        let shared = self.inner.workspace.as_deref();
        let spawner_dir = sync::lock(&self.inner.state).workspace_of(parent, shared);
        let cwd = match kwargs.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => {
                let path = spawner_dir
                    .as_deref()
                    .map_or_else(|| PathBuf::from(raw), |dir| dir.join(raw));
                match std::fs::canonicalize(&path) {
                    Ok(dir) if dir.is_dir() => Some(dir),
                    _ => return Err(format!("cwd={raw:?} is not an existing directory ({})", path.display())),
                }
            }
            Some(_) => return Err("cwd must be a directory path string".into()),
        };
        let setup = check_worktree_setup(kwargs.get("worktree_setup"))?;
        if setup.is_some() && mode == WorktreeMode::Off {
            return Err("worktree_setup needs worktree=True".into());
        }
        let child_depth = match kwargs.get("depth") {
            None => 0,
            Some(value) => number::safe_integer(value)
                .filter(|n| *n >= 0)
                .ok_or("depth must be a non-negative integer")?,
        };
        let max_child_depth = (allowance.saturating_sub(1)).min(if limit > 0 {
            i64::from(limit) - depth - 1
        } else {
            i64::MAX
        });
        if child_depth > max_child_depth {
            return Err(format!(
                "rlm.spawn depth={child_depth} is too deep here: at most {max_child_depth} (ULTRON_SPAWN_DEPTH={limit} levels in all)."
            ));
        }
        let model = match kwargs.get("model") {
            Some(value @ Value::String(_)) => {
                let model = model_scope::model_ref(value)?;
                if let Some(check) = &self.inner.model_check {
                    check(&model)?;
                }
                Some(model)
            }
            _ => self.child_model(),
        };
        let timeout_ms = crate::timeout_ms(kwargs.get("timeout_ms").filter(|t| t.is_number()))?;
        let fork_at = match fork_hook.filter(|_| fork) {
            Some(hook) => {
                Some(hook(lane)?.ok_or("rlm.spawn fork=True: your lane has no finished conversation to fork yet")?)
            }
            None => None,
        };
        let worktree = self.new_worktree(mode, &name, setup.as_ref(), cwd.clone().or(spawner_dir))?;
        let thinking = match kwargs.get("thinking").map(model_scope::thinking_level) {
            None => None,
            Some(Ok(level)) => Some(level),
            Some(Err(error)) => {
                if let Some(record) = &worktree {
                    wt::remove_child_worktree(record, true);
                }
                return Err(error);
            }
        };
        let mut fingerprinted = json!({
            "definition": CHILD,
            "input": { "prompt": prompt },
            "model": model,
            "timeout_ms": timeout_ms,
        });
        if let Some(contract) = &contract {
            fingerprinted["contract"] = contract.describe();
        }
        let fingerprint = task_fingerprint(&fingerprinted);
        let signal = Signal::new();
        let mut record_error = None;
        let (task_id, level) = {
            let mut state = sync::lock(&self.inner.state);
            if state.closed {
                return Err("Ultron task host is closed".into());
            }
            let admitted = state
                .journal
                .admit(CHILD, &fingerprint, &jsrt::random::uuid_v4(), parent);
            let (record, _) = match admitted {
                Ok(admitted) => admitted,
                Err(error) => {
                    if let Some(record) = &worktree {
                        wt::remove_child_worktree(record, true);
                    }
                    return Err(error);
                }
            };
            let id = record["id"].as_str().unwrap_or_default().to_string();
            let record = match state.journal.transition(&id, "running", None) {
                Ok(running) => running,
                Err(error) => {
                    if let Some(record) = &worktree {
                        wt::remove_child_worktree(record, true);
                    }
                    return Err(error);
                }
            };
            state.tasks.push(Task {
                id: id.clone(),
                parent_id: parent.map(String::from),
                record,
                outcome: None,
                spawn_depth: child_depth as u32,
                lane_name: None,
                name: Some(name.clone()),
                model: model.clone(),
                started_at: Some((self.inner.now)()),
                ended_at: None,
                timeout_ms: Some(timeout_ms),
                verdict: None,
                verdict_rejections: 0,
                verdict_problems: Vec::new(),
                verdict_invalid: false,
                waiters: 0,
                signal: signal.clone(),
                finishing: false,
                worktree: worktree.clone().map(|mut record| {
                    record.task_id = Some(id.clone());
                    record_error = wt::save_record(&record).err().map(|error| error.message);
                    record
                }),
                cwd: cwd.filter(|_| worktree.is_none()),
                worktree_baseline: None,
                outside_before: None,
                before: None,
                merge_status: None,
                contract: contract.clone(),
                git_baseline: None,
            });
            let level = root.level + state.line(Some(&id)).iter().filter(|a| state.is_child(a)).count() as u32;
            (id, level)
        };
        let lane_name = lanes::lane_names::child_lane(&task_id);
        let (cwd, env, brief) = {
            let state = sync::lock(&self.inner.state);
            let inherited = state.worktree_of(Some(&task_id));
            let note = worktree.as_ref().map(wt::worktree_brief_note);
            let prompt = match &contract {
                Some(contract) => contract.prompt(&prompt),
                None => prompt,
            };
            let prompt = match (&fork_at, context) {
                (Some(_), _) => format!("{}\n\n{prompt}", crate::fork::FORK_BRIEF_NOTE),
                (None, SpawnContext::Retrieve) => format!(
                    "{}\n\n{prompt}",
                    crate::fork::retrieve_brief_note(&self.inner.session_id)
                ),
                (None, _) => prompt,
            };
            (
                state
                    .workspace_of(Some(&task_id), None)
                    .map(|dir| dir.display().to_string()),
                inherited.map_or_default(wt::worktree_env),
                match note {
                    Some(note) => format!("{prompt}\n\n{note}"),
                    None => prompt,
                },
            )
        };
        let run = ChildRun {
            task_id: task_id.clone(),
            name: name.clone(),
            lane_name: lane_name.clone(),
            prompt: brief,
            level,
            allowance: child_depth as u32,
            model: model.clone(),
            thinking,
            timeout_ms,
            cwd,
            env,
            fork_at: fork_at.clone(),
            signal: signal.clone(),
            host: self.clone(),
        };
        {
            let mut state = sync::lock(&self.inner.state);
            state.lane_tasks.insert(lane_name.clone(), task_id.clone());
            if let Some(task) = state.task_mut(&task_id) {
                task.lane_name = Some(lane_name);
            }
        }
        let observer = jsrt::sync::lock(&self.inner.on_spawn).clone();
        if let Some(observer) = observer {
            observer(&task_id, parent, lane);
        }
        let host = self.clone();
        std::thread::spawn(move || host.run_child(run));
        let host = self.clone();
        let (id, deadline) = (task_id.clone(), signal);
        std::thread::spawn(move || {
            if !deadline.wait(Duration::from_millis(timeout_ms)) {
                let _ = host.cancel(&id, &format!("Ultron task exceeded {timeout_ms}ms timeout"));
            }
        });
        let mut worktree = worktree.map_or(
            Value::Null,
            |w| json!({ "branch": w.branch, "path": w.path, "cwd": w.cwd }),
        );
        if let Some(error) = record_error {
            worktree["record_error"] = error.into();
        }
        Ok(json!({
            "rlm_child_id": task_id,
            "name": name,
            "session_dir": "",
            "model": model.unwrap_or_default(),
            "timeout_ms": timeout_ms,
            "parent_branch_anchor": fork_at.unwrap_or_default(),
            "worktree": worktree,
        }))
    }

    /// `newWorktree`: a private worktree (`worktree=True`) branched from the repository at `from`.
    pub(super) fn new_worktree(
        &self,
        mode: WorktreeMode,
        name: &str,
        setup: Option<&WorktreeSetup>,
        from: Option<PathBuf>,
    ) -> Result<Option<WorktreeRecord>, String> {
        if mode == WorktreeMode::Off {
            return Ok(None);
        }
        let Some(from) = from else {
            if mode == WorktreeMode::Auto {
                return Ok(None);
            }
            return Err("worktree=True needs a working directory, and this session has none".into());
        };
        let default_setup = (self.inner.worktree_setup)();
        let setup = merged_setup(default_setup.as_ref(), setup);
        let created = wt::create_child_worktree(&wt::CreateWorktreeOptions {
            cwd: &from,
            session_id: &self.inner.session_id,
            name,
            setup: Some(&setup),
            now: (self.inner.now)(),
        });
        match created {
            Ok(record) => Ok(Some(record)),
            Err(error) if mode == WorktreeMode::Auto && error.fallback => Ok(None),
            Err(error) => Err(format!("worktree=True: {}", error.message)),
        }
    }

    pub(super) fn run_child(&self, run: ChildRun) {
        let id = run.task_id.clone();
        let signal = run.signal.clone();
        let contract = sync::lock(&self.inner.state).task(&id).and_then(|t| t.contract.clone());
        let started = self.child_start(&id, &signal);
        let reply = match &started {
            Ok(()) => (self.inner.runner)(run.clone()),
            Err(error) => Err(error.clone()),
        };
        if signal.is_aborted() {
            return;
        }
        let result = self.reply_result(&id, reply);
        let result = match &contract {
            Some(contract) => match self.enforce_contract(&id, contract, Some(run), result, &signal) {
                Some(result) => result,
                None => return,
            },
            None => result,
        };
        let result = self.with_limit_reason(&id, result);
        let _ = self.end(&id, result);
    }

    /// A child run's reply as its result (a failed run is a failed, unverified result).
    pub(super) fn reply_result(&self, id: &str, reply: Result<String, String>) -> Value {
        let result = match reply {
            Ok(text) => self.child_result(id, &text),
            Err(error) => Err(error),
        };
        result.unwrap_or_else(|error| json!({ "status": "failed", "error": error, "verification": "unverified" }))
    }

    /// Hold a child's `result` to its contract ([`enforce`]): a retry re-runs `run` (with no run to repeat, the
    /// failure is reported as it is), a review spawns the reviewer as a sibling of the child. `None` when the task
    /// was cancelled meanwhile.
    pub(super) fn enforce_contract(
        &self,
        id: &str,
        contract: &SpawnContract,
        mut run: Option<ChildRun>,
        result: Value,
        signal: &Signal,
    ) -> Option<Value> {
        let name = run.as_ref().map_or_else(|| id.to_string(), |run| run.name.clone());
        let mut check = || {
            let report = self.check_contract(id, contract, signal);
            (!signal.is_aborted()).then_some(report)
        };
        let mut retry = |note: &str| {
            let Some(again) = run.as_mut() else {
                return Some(None);
            };
            if let Some(task) = sync::lock(&self.inner.state).task_mut(id) {
                task.verdict = None;
                task.verdict_rejections = 0;
                task.verdict_problems.clear();
                task.verdict_invalid = false;
            }
            again.prompt = format!("{}\n\n{note}", again.prompt);
            let reply = (self.inner.runner)(again.clone());
            if signal.is_aborted() {
                return None;
            }
            Some(Some(self.reply_result(id, reply)))
        };
        let mut review = |name: &str, prompt: &str| {
            let (parent, owner) = {
                let state = sync::lock(&self.inner.state);
                let parent = state.task(id).and_then(|t| t.parent_id.clone());
                let owner = parent
                    .as_deref()
                    .and_then(|p| state.task(p).and_then(|t| t.lane_name.clone()))
                    .unwrap_or_else(|| "main".into());
                (parent, owner)
            };
            let payload = json!({ "prompt": prompt, "kwargs": { "name": name } });
            let payload = payload.as_object().cloned().unwrap_or_default();
            self.spawn(&payload, parent.as_deref(), &owner)
        };
        let mut enforcer = Enforcer {
            check: &mut check,
            retry: &mut retry,
            review: &mut review,
        };
        enforce(contract, result, id, &name, &mut enforcer)
    }

    /// The contract of task `id` as [`SpawnContract::describe`] writes it, for its session to keep with the task
    /// (`None`: it was spawned without one).
    /// A contract with workspace checks carries the Git baseline of its start (`baseline`), when there is one.
    pub fn contract_of(&self, id: &str) -> Option<Value> {
        let state = sync::lock(&self.inner.state);
        let task = state.task(id)?;
        let mut described = task.contract.as_ref()?.describe();
        if let Some(baseline) = &task.git_baseline {
            described["baseline"] = baseline.describe();
        }
        Some(described)
    }

    /// Run a child's contract checks in its working directory (its worktree's, else the shared one), on the files
    /// as they are now, against the snapshot taken when it started.
    pub(super) fn check_contract(&self, id: &str, contract: &SpawnContract, signal: &Signal) -> Value {
        let (verdict, before, baseline, workspace, env) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return json!({ "verdict": "contract_failed", "checks": [], "failed": ["Unknown Ultron task"] });
            };
            (
                task.verdict.clone(),
                task.before.clone(),
                task.git_baseline.clone(),
                state.workspace_of(Some(id), self.inner.workspace.as_deref()),
                state.worktree_of(Some(id)).map_or_default(wt::worktree_env),
            )
        };
        let diff = match (&before, &baseline, &workspace) {
            (Some(before), _, Some(_)) => diff_snapshots(before, &snapshot_workspace(&before.root)).ok(),
            (None, Some(baseline), Some(workspace)) => baseline.changes(workspace, &self.inner.command_env).ok(),
            _ => None,
        };
        let place = CheckPlace {
            cwd: workspace.as_deref(),
            env: &env,
            diff: diff.as_ref(),
            signal,
        };
        contract.evaluate(verdict.as_ref(), &place, &self.inner.check_runner)
    }

    /// `childStart`: a worktree child's setup command runs, then its working directory is pictured (plus the parent's
    /// tree for a worktree child, to see writes it makes outside its worktree).
    pub(super) fn child_start(&self, id: &str, signal: &Signal) -> Result<(), String> {
        let (own, workspace, parent_workspace, needs_baseline) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let shared = self.inner.workspace.as_deref();
            (
                task.worktree.clone(),
                state.workspace_of(Some(id), shared),
                state.workspace_of(task.parent_id.as_deref(), shared),
                task.contract.as_ref().is_some_and(|c| c.needs_baseline()),
            )
        };
        if let Some(record) = &own {
            wt::run_setup_command(record, Some(signal)).map_err(|e| e.message)?;
        }
        let git_baseline = match (&workspace, needs_baseline) {
            (Some(w), true) => GitBaseline::capture(w, &self.inner.command_env),
            _ => None,
        };
        let before = workspace.map(|w| snapshot_workspace(&w));
        let outside = match (&own, &before) {
            (Some(_), Some(_)) => parent_workspace.map(|w| snapshot_workspace(&w)),
            _ => None,
        };
        let mut state = sync::lock(&self.inner.state);
        if let Some(task) = state.task_mut(id) {
            if own.is_some() {
                task.worktree_baseline = before.clone();
                task.outside_before = outside;
            }
            task.before = before;
            task.git_baseline = git_baseline;
        }
        Ok(())
    }

    /// `childResult`: the reply, the verdict (null without one), and the check of its declared files against the
    /// files that changed in its working directory while it ran.
    pub(super) fn child_result(&self, id: &str, text: &str) -> Result<Value, String> {
        let (verdict, invalid, problems, before, workspace) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            (
                task.verdict.clone(),
                task.verdict_invalid,
                task.verdict_problems.clone(),
                task.before.clone(),
                state.workspace_of(Some(id), self.inner.workspace.as_deref()),
            )
        };
        if text.trim().is_empty() && verdict.is_none() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if text.trim().is_empty() {
            verdict.as_ref().map_or(Value::Null, |v| v["summary"].clone())
        } else {
            json!(text)
        };
        let after = match (&before, &workspace) {
            (Some(_), Some(w)) => Some(snapshot_workspace(w)),
            _ => None,
        };
        let (concurrent, explained) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let others = self.concurrent_with(&state, task);
            let explained: Vec<String> = others
                .iter()
                .filter_map(|other| state.task(other))
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => None,
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (others, explained)
        };
        let mut check = match (&before, &after) {
            (Some(before), Some(after)) => {
                let declared: Vec<String> = verdict
                    .as_ref()
                    .and_then(|v| v["changed_files"].as_array())
                    .map_or_default(|files| files.iter().filter_map(|f| f.as_str().map(String::from)).collect());
                let diff = diff_snapshots(before, after)?;
                let mut check = check_files(&declared, &before.root, before, after, &diff, &explained, &concurrent);
                if verdict.is_none() {
                    check["outcome"] = json!(if invalid { "invalid" } else { "unchecked" });
                    check["reason"] = json!("the subagent ended without a verdict (rlm.finish)");
                }
                check
            }
            _ => json!({
                "outcome": if invalid { "invalid" } else { "unchecked" },
                "unobserved": [], "unreported": [], "unlisted": [],
                "concurrent": concurrent,
                "reason": "no workspace to compare",
            }),
        };
        if invalid {
            check["problems"] = json!(problems);
        }
        let outside = self.outside_writes(id);
        if !outside.is_empty() {
            check["outside"] = json!(outside);
        }
        let nothing_checked = verdict.is_none() && !invalid && before.is_none();
        let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                                 "verdict": verdict.clone().unwrap_or(Value::Null) });
        if !nothing_checked {
            result["check"] = check;
        }
        if verdict.is_none() {
            result["unverified"] = json!(true);
        }
        Ok(result)
    }

    /// `outsideWrites`: files in the parent's tree that changed while a worktree child ran, minus what other tasks
    /// working there declared.
    pub(super) fn outside_writes(&self, id: &str) -> Vec<String> {
        let (root, before, explained) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return Vec::new();
            };
            let (Some(_), Some(before)) = (&task.worktree, &task.outside_before) else {
                return Vec::new();
            };
            let shared = self.inner.workspace.as_deref();
            let target = state.workspace_of(task.parent_id.as_deref(), shared);
            let explained: Vec<String> = state
                .tasks
                .iter()
                .filter(|other| other.id != id && state.workspace_of(Some(&other.id), shared) == target)
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => other
                        .verdict
                        .as_ref()
                        .and_then(|v| v["changed_files"].as_array().cloned()),
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (before.root.clone(), before.clone(), explained)
        };
        let after = snapshot_workspace(&root);
        diff_snapshots(&before, &after).map_or_default(|diff| {
            diff.changed
                .into_iter()
                .filter(|path| !explained.contains(path))
                .take(100)
                .collect()
        })
    }

    /// Other started tasks whose runs overlapped this one in the same working directory, outside its own line
    /// (ancestors and descendants).
    pub(super) fn concurrent_with(&self, state: &State, task: &Task) -> Vec<String> {
        let start = task.started_at.unwrap_or(0.0);
        let end = (self.inner.now)();
        let ancestors = state.line(task.parent_id.as_deref());
        let shared = self.inner.workspace.as_deref();
        let workspace = state.workspace_of(Some(&task.id), shared);
        state
            .tasks
            .iter()
            .filter(|other| other.id != task.id && other.lane_name.is_some())
            .filter(|other| state.workspace_of(Some(&other.id), shared) == workspace)
            .filter(|other| {
                other.started_at.is_some_and(|s| s <= end) && other.ended_at.unwrap_or(f64::INFINITY) >= start
            })
            .filter(|other| !ancestors.contains(&other.id))
            .filter(|other| !state.line(other.parent_id.as_deref()).contains(&task.id))
            .map(|other| other.id.clone())
            .collect()
    }

    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    ///
    /// A terminal write that fails is not an end: the journal still says `running` (and a failed write poisons it
    /// until the worker reopens it), so a restart reattaches the child and delivers its reply, and publishing the
    /// error now would give the parent two results. The task stays unfinished and the caller gets the error; the
    /// next end (a cancel, its timeout) tries again, and once the host has closed the error is published (this
    /// worker is going away).
    pub(super) fn end(&self, id: &str, result: Value) -> Result<Value, String> {
        {
            let mut state = sync::lock(&self.inner.state);
            loop {
                let task = state.task(id).ok_or("Unknown Ultron task")?;
                if let Some(outcome) = &task.outcome {
                    return outcome.clone();
                }
                if !task.finishing {
                    break;
                }
                state = self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.finishing = true;
            if result["status"] == "cancelled" {
                task.signal.abort(result["error"].as_str().unwrap_or("cancelled"));
            }
        }
        let result = self.finalize_worktree(id, result);
        let mut ended = None;
        let event = {
            let mut state = sync::lock(&self.inner.state);
            let terminal = match result["status"].as_str() {
                Some("succeeded") => "completed",
                Some(other) => other,
                None => "failed",
            }
            .to_string();
            let committed = state.journal.transition(id, &terminal, Some(result));
            let now = (self.inner.now)();
            let closed = state.closed;
            if let Err(error) = &committed
                && !closed
            {
                if let Some(task) = state.task_mut(id) {
                    task.finishing = false;
                }
                self.inner.changed.notify_all();
                return Err(error.clone());
            }
            let owner_lane = {
                let parent = state.task(id).and_then(|t| t.parent_id.clone());
                parent
                    .and_then(|p| state.task(&p).and_then(|t| t.lane_name.clone()))
                    .unwrap_or_else(|| "main".into())
            };
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.ended_at.get_or_insert(now);
            task.signal.abort("ended");
            let outcome = committed.map(|record| {
                task.record = record.clone();
                record["result"].clone()
            });
            task.outcome = Some(outcome.clone());
            if outcome.is_ok() {
                ended = Some(public_record(&task.record));
            }
            self.inner.changed.notify_all();
            let event = match outcome {
                Ok(_) if !closed => Some(DetachedEnd {
                    task: task.record.clone(),
                    kind: "child_done",
                    owner_lane,
                    awaited: task.waiters > 0,
                    fetch: format!("await rlm.collect([\"{id}\"])"),
                }),
                Ok(_) => None,
                Err(error) => return Err(error),
            };
            if let Some(event) = &event {
                let _ = jsrt::sync::lock(&self.inner.events).send(event.clone());
            }
            event
        };
        let observer = jsrt::sync::lock(&self.inner.on_task_end).clone();
        if let (Some(observer), Some(record)) = (observer, &ended) {
            observer(record);
        }
        if let Some(event) = event {
            return Ok(event.task["result"].clone());
        }
        let state = sync::lock(&self.inner.state);
        state
            .task(id)
            .and_then(|t| t.outcome.clone())
            .unwrap_or(Err("Unknown Ultron task".into()))
    }

    /// `finalizeWorktree`: commit what a worktree child changed on its branch and add the worktree to its result. A
    /// child that changed nothing has its worktree and branch removed at once. Never fails the task: a Git problem is
    /// reported in `worktree.error` and the worktree is kept.
    pub(super) fn finalize_worktree(&self, id: &str, mut result: Value) -> Value {
        let (record, verdict, baseline) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return result;
            };
            (
                task.worktree.clone().filter(|w| w.state == "active"),
                task.verdict.clone(),
                task.worktree_baseline.clone(),
            )
        };
        let Some(mut record) = record else {
            return result;
        };
        let status = verdict
            .as_ref()
            .and_then(|v| v["status"].as_str())
            .or(result["status"].as_str())
            .unwrap_or("")
            .to_string();
        let summary = match verdict.as_ref().and_then(|v| v["summary"].as_str()) {
            Some(summary) => summary.to_string(),
            None => match (&result["value"], &result["error"]) {
                (Value::String(value), _) => value.clone(),
                (_, Value::String(error)) => error.clone(),
                _ => String::new(),
            },
        };
        let paths = worktree_changes(&record, baseline.as_ref());
        let message = wt::commit_message(&summary, &record.name, Some(id), &status);
        let keep = self.inner.keep_worktrees;
        let mut empty = false;
        let info = match wt::commit_child_worktree(&record, &message, paths.as_deref()) {
            Ok(commit) => {
                record.state = if commit.commit.is_some() && status != "passed" && status != "succeeded" {
                    "failed"
                } else {
                    "ended"
                }
                .into();
                record.commit = Some(commit.commit.clone());
                empty = commit.commit.is_none();
                let saved = if empty && !keep {
                    wt::remove_child_worktree(&record, true);
                    Ok(())
                } else {
                    wt::save_record(&record)
                };
                let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": commit.commit, "changed_files": commit.changed_files,
                    "diffstat": commit.diffstat });
                if empty && !keep {
                    info["removed"] = json!(true);
                }
                if let Err(error) = saved {
                    info["record_error"] = error.message.into();
                }
                info
            }
            Err(error) => {
                record.state = "failed".into();
                let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": null, "changed_files": [], "diffstat": "",
                    "error": format!("committing the worktree failed: {}", error.message) });
                if let Err(error) = wt::save_record(&record) {
                    info["record_error"] = error.message.into();
                }
                info
            }
        };
        {
            let mut state = sync::lock(&self.inner.state);
            if let Some(task) = state.task_mut(id) {
                task.worktree = Some(record);
                if empty {
                    task.merge_status = Some("empty".into());
                }
            }
        }
        if empty {
            self.merged(id, "empty");
        }
        result["worktree"] = info;
        result
    }
}
