//! Subagent spawning, worktree allocation, child execution, contract enforcement, and completion.

use super::*;

struct SpawnWorkspace {
    mode: WorktreeMode,
    setup: Option<WorktreeSetup>,
    cwd: Option<PathBuf>,
    spawner_dir: Option<PathBuf>,
}

struct SpawnSpec {
    prompt: String,
    name: String,
    contract: Option<Arc<SpawnContract>>,
    context: SpawnContext,
    mode: WorktreeMode,
    setup: Option<WorktreeSetup>,
    cwd: Option<PathBuf>,
    spawner_dir: Option<PathBuf>,
    child_depth: i64,
    model: Option<String>,
    timeout_ms: u64,
    fork_at: Option<String>,
}

fn parse_worktree_mode(value: Option<&Value>) -> Result<WorktreeMode, String> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(WorktreeMode::Off),
        Some(Value::Bool(true)) => Ok(WorktreeMode::On),
        Some(Value::String(s)) if s == "auto" => Ok(WorktreeMode::Auto),
        Some(_) => {
            Err("worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)".into())
        }
    }
}

fn resolve_spawn_cwd(value: Option<&Value>, spawner_dir: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => {
            let path = spawner_dir.map_or_else(|| PathBuf::from(raw), |dir| dir.join(raw));
            match std::fs::canonicalize(&path) {
                Ok(dir) if dir.is_dir() => Ok(Some(dir)),
                _ => Err(format!("cwd={raw:?} is not an existing directory ({})", path.display())),
            }
        }
        Some(_) => Err("cwd must be a directory path string".into()),
    }
}

fn parse_child_depth(value: Option<&Value>, allowance: i64, depth: i64, limit: u32) -> Result<i64, String> {
    let child_depth = match value {
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
    Ok(child_depth)
}

fn spawn_fingerprint(
    prompt: &str,
    model: Option<&str>,
    timeout_ms: u64,
    contract: Option<&Arc<SpawnContract>>,
) -> String {
    let mut fingerprinted = json!({
        "definition": CHILD,
        "input": { "prompt": prompt },
        "model": model,
        "timeout_ms": timeout_ms,
    });
    if let Some(contract) = contract {
        fingerprinted["contract"] = contract.describe();
    }
    task_fingerprint(&fingerprinted)
}

impl Delegation {
    fn check_spawn_allowance(
        &self,
        parent: Option<&str>,
        root_level: u32,
        root_allowance: Option<i64>,
    ) -> Result<(i64, i64), String> {
        let limit = self.inner.spawn_depth;
        let (depth, allowance) = {
            let state = sync::lock(&self.inner.state);
            let children: Vec<String> = state.line(parent).into_iter().filter(|id| state.is_child(id)).collect();
            let allowance = children
                .first()
                .and_then(|id| state.task(id))
                .map_or(root_allowance.unwrap_or(i64::MAX), |spawner| {
                    i64::from(spawner.spawn_depth)
                });
            (i64::from(root_level) + children.len() as i64, allowance)
        };
        if limit > 0 && depth >= i64::from(limit) {
            return Err(format!(
                "rlm.spawn refused: subagents nest at most {limit} level(s) deep (ULTRON_SPAWN_DEPTH). Do this part yourself: narrow with code, read the candidates, and use rlm.map for passages you cannot settle."
            ));
        }
        if allowance < 1 {
            return Err("rlm.spawn refused: this subagent was started with depth=0, so it does its brief itself. A parent that wants nested delegation passes rlm.spawn(brief, name=..., depth=N).".into());
        }
        Ok((depth, allowance))
    }

    fn resolve_spawn_model(&self, value: Option<&Value>) -> Result<Option<String>, String> {
        match value {
            Some(value @ Value::String(_)) => {
                let model = model_scope::model_ref(value)?;
                if let Some(check) = &self.inner.model_check {
                    check(&model)?;
                }
                Ok(Some(model))
            }
            _ => Ok(self.child_model()),
        }
    }

    fn parse_spawn_workspace(
        &self,
        kwargs: &Map<String, Value>,
        parent: Option<&str>,
    ) -> Result<SpawnWorkspace, String> {
        let mode = parse_worktree_mode(kwargs.get("worktree"))?;
        let spawner_dir = sync::lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let cwd = resolve_spawn_cwd(kwargs.get("cwd"), spawner_dir.as_deref())?;
        let setup = check_worktree_setup(kwargs.get("worktree_setup"))?;
        if setup.is_some() && mode == WorktreeMode::Off {
            return Err("worktree_setup needs worktree=True".into());
        }
        Ok(SpawnWorkspace {
            mode,
            setup,
            cwd,
            spawner_dir,
        })
    }

    fn parse_spawn_context(
        &self,
        kwargs: &Map<String, Value>,
        lane: &str,
    ) -> Result<(SpawnContext, Option<String>), String> {
        let context = SpawnContext::of(kwargs.get("context"), kwargs.get("fork"))?;
        let fork_at = self.resolve_fork_anchor(context == SpawnContext::Fork, lane)?;
        Ok((context, fork_at))
    }

    fn parse_spawn_spec(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        lane: &str,
        depth: i64,
        allowance: i64,
    ) -> Result<(SpawnSpec, Option<Value>), String> {
        let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
        let empty = Map::new();
        let kwargs = extract_spawn_kwargs(payload, &empty)?;
        let contract = SpawnContract::parse(kwargs)?.map(Arc::new);
        let (context, fork_at) = self.parse_spawn_context(kwargs, lane)?;
        let name = nonempty_string(kwargs.get("name"), "name")?;
        let ws = self.parse_spawn_workspace(kwargs, parent)?;
        let child_depth = parse_child_depth(kwargs.get("depth"), allowance, depth, self.inner.spawn_depth)?;
        let model = self.resolve_spawn_model(kwargs.get("model"))?;
        let timeout_ms = crate::timeout_ms(kwargs.get("timeout_ms").filter(|t| t.is_number()))?;
        Ok((
            SpawnSpec {
                prompt,
                name,
                contract,
                context,
                mode: ws.mode,
                setup: ws.setup,
                cwd: ws.cwd,
                spawner_dir: ws.spawner_dir,
                child_depth,
                model,
                timeout_ms,
                fork_at,
            },
            kwargs.get("thinking").cloned(),
        ))
    }

    fn resolve_fork_anchor(&self, fork: bool, lane: &str) -> Result<Option<String>, String> {
        if !fork {
            return Ok(None);
        }
        let Some(hook) = jsrt::sync::lock(&self.inner.fork_point).clone() else {
            return Err("rlm.spawn fork=True is not available when subagents run as external processes".into());
        };
        hook(lane)?
            .ok_or_else(|| "rlm.spawn fork=True: your lane has no finished conversation to fork yet".into())
            .map(Some)
    }

    fn admit_child_task(
        &self,
        spec: &SpawnSpec,
        parent: Option<&str>,
        root_level: u32,
        worktree: &Option<WorktreeRecord>,
        signal: &Signal,
    ) -> Result<(String, u32, Option<String>), String> {
        let fingerprint = spawn_fingerprint(
            &spec.prompt,
            spec.model.as_deref(),
            spec.timeout_ms,
            spec.contract.as_ref(),
        );
        let mut record_error = None;
        let mut state = sync::lock(&self.inner.state);
        if state.closed {
            return Err("Ultron task host is closed".into());
        }
        let (record, _) = state
            .journal
            .admit(CHILD, &fingerprint, &jsrt::random::uuid_v4(), parent)?;
        let id = record["id"].as_str().unwrap_or_default().to_string();
        let record = state.journal.transition(&id, "running", None)?;
        state.tasks.push(Task {
            id: id.clone(),
            parent_id: parent.map(String::from),
            record,
            outcome: None,
            spawn_depth: spec.child_depth as u32,
            lane_name: None,
            name: Some(spec.name.clone()),
            model: spec.model.clone(),
            started_at: Some((self.inner.now)()),
            ended_at: None,
            timeout_ms: Some(spec.timeout_ms),
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
            cwd: spec.cwd.clone().filter(|_| worktree.is_none()),
            worktree_baseline: None,
            outside_before: None,
            before: None,
            merge_status: None,
            contract: spec.contract.clone(),
            git_baseline: None,
        });
        let level = root_level + state.line(Some(&id)).iter().filter(|a| state.is_child(a)).count() as u32;
        Ok((id, level, record_error))
    }

    fn build_child_run_context(
        &self,
        task_id: &str,
        spec: &SpawnSpec,
        worktree: Option<&WorktreeRecord>,
    ) -> (Option<String>, Vec<(String, String)>, String) {
        let state = sync::lock(&self.inner.state);
        let inherited = state.worktree_of(Some(task_id));
        let note = worktree.map(wt::worktree_brief_note);
        let prompt = match &spec.contract {
            Some(contract) => contract.prompt(&spec.prompt),
            None => spec.prompt.clone(),
        };
        let prompt = match (&spec.fork_at, spec.context) {
            (Some(_), _) => format!("{}\n\n{prompt}", crate::fork::FORK_BRIEF_NOTE),
            (None, SpawnContext::Retrieve) => format!(
                "{}\n\n{prompt}",
                crate::fork::retrieve_brief_note(&self.inner.session_id)
            ),
            (None, _) => prompt,
        };
        let brief = match note {
            Some(note) => format!("{prompt}\n\n{note}"),
            None => prompt,
        };
        (
            state
                .workspace_of(Some(task_id), None)
                .map(|dir| dir.display().to_string()),
            inherited.map_or_default(wt::worktree_env),
            brief,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper spawning the child runner and timeout threads"
    )]
    fn launch_spawned_child(
        &self,
        task_id: &str,
        spec: &SpawnSpec,
        level: u32,
        thinking: Option<String>,
        worktree: Option<&WorktreeRecord>,
        parent: Option<&str>,
        lane: &str,
        signal: Signal,
    ) {
        let lane_name = lanes::lane_names::child_lane(task_id);
        let (cwd, env, brief) = self.build_child_run_context(task_id, spec, worktree);
        let run = ChildRun {
            task_id: task_id.to_string(),
            name: spec.name.clone(),
            lane_name: lane_name.clone(),
            prompt: brief,
            level,
            allowance: spec.child_depth as u32,
            model: spec.model.clone(),
            thinking,
            timeout_ms: spec.timeout_ms,
            cwd,
            env,
            fork_at: spec.fork_at.clone(),
            signal: signal.clone(),
            host: self.clone(),
        };
        {
            let mut state = sync::lock(&self.inner.state);
            state.lane_tasks.insert(lane_name.clone(), task_id.to_string());
            if let Some(task) = state.task_mut(task_id) {
                task.lane_name = Some(lane_name);
            }
        }
        let observer = jsrt::sync::lock(&self.inner.on_spawn).clone();
        if let Some(observer) = observer {
            observer(task_id, parent, lane);
        }
        let host = self.clone();
        std::thread::spawn(move || host.run_child(run));
        let host = self.clone();
        let (id, timeout_ms) = (task_id.to_string(), spec.timeout_ms);
        std::thread::spawn(move || {
            if !signal.wait(Duration::from_millis(timeout_ms)) {
                let _ = host.cancel(&id, &format!("Ultron task exceeded {timeout_ms}ms timeout"));
            }
        });
    }

    pub(super) fn spawn(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        lane: &str,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["prompt", "kwargs"])?;
        let root = jsrt::sync::lock(&self.inner.root).clone();
        let (depth, allowance) = self.check_spawn_allowance(parent, root.level, root.allowance)?;
        let (spec, raw_thinking) = self.parse_spawn_spec(payload, parent, lane, depth, allowance)?;
        let worktree = self.new_worktree(
            spec.mode,
            &spec.name,
            spec.setup.as_ref(),
            spec.cwd.clone().or_else(|| spec.spawner_dir.clone()),
        )?;
        let thinking = parse_thinking_or_cleanup(raw_thinking.as_ref(), &worktree)?;
        let signal = Signal::new();
        let (task_id, level, record_error) = cleanup_on_err(
            self.admit_child_task(&spec, parent, root.level, &worktree, &signal),
            &worktree,
        )?;
        self.launch_spawned_child(
            &task_id,
            &spec,
            level,
            thinking,
            worktree.as_ref(),
            parent,
            lane,
            signal,
        );
        Ok(format_spawn_reply(task_id, spec, worktree, record_error))
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

    fn concurrent_explained(&self, id: &str) -> Result<(Vec<String>, Vec<String>), String> {
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
        Ok((others, explained))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper assembling the workspace check object for a child"
    )]
    fn build_child_check(
        &self,
        id: &str,
        verdict: Option<&Value>,
        invalid: bool,
        problems: &[String],
        before: Option<&WorkspaceSnapshot>,
        after: Option<&WorkspaceSnapshot>,
        explained: &[String],
        concurrent: &[String],
    ) -> Result<Value, String> {
        let mut check = match (before, after) {
            (Some(before), Some(after)) => {
                let declared: Vec<String> = verdict
                    .and_then(|v| v["changed_files"].as_array())
                    .map_or_default(|files| files.iter().filter_map(|f| f.as_str().map(String::from)).collect());
                let diff = diff_snapshots(before, after)?;
                let mut check = check_files(&declared, &before.root, before, after, &diff, explained, concurrent);
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
        Ok(check)
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
        let value = resolve_child_reply_value(text, verdict.as_ref())?;
        let after = before.as_ref().and(workspace.as_deref()).map(snapshot_workspace);
        let (concurrent, explained) = self.concurrent_explained(id)?;
        let check = self.build_child_check(
            id,
            verdict.as_ref(),
            invalid,
            &problems,
            before.as_ref(),
            after.as_ref(),
            &explained,
            &concurrent,
        )?;
        Ok(assemble_child_result(value, verdict, check, invalid, before.is_some()))
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

    fn claim_finishing(&self, id: &str, result: &Value) -> Result<Option<Result<Value, String>>, String> {
        let mut state = sync::lock(&self.inner.state);
        loop {
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            if let Some(outcome) = &task.outcome {
                return Ok(Some(outcome.clone()));
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
        Ok(None)
    }

    fn commit_task_end(&self, id: &str, result: Value) -> Result<(Option<DetachedEnd>, Option<Value>), String> {
        let mut state = sync::lock(&self.inner.state);
        let terminal = terminal_state_name(result["status"].as_str());
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
        let owner_lane = owner_lane_of(&state, id);
        let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
        task.ended_at.get_or_insert(now);
        task.signal.abort("ended");
        let outcome = committed.map(|record| {
            task.record = record.clone();
            record["result"].clone()
        });
        task.outcome = Some(outcome.clone());
        let ended = outcome.is_ok().then(|| public_record(&task.record));
        self.inner.changed.notify_all();
        let event = build_detached_end(id, task, owner_lane, closed, outcome)?;
        if let Some(event) = &event {
            let _ = jsrt::sync::lock(&self.inner.events).send(event.clone());
        }
        Ok((event, ended))
    }

    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    ///
    /// A terminal write that fails is not an end: the journal still says `running` (and a failed write poisons it
    /// until the worker reopens it), so a restart reattaches the child and delivers its reply, and publishing the
    /// error now would give the parent two results. The task stays unfinished and the caller gets the error; the
    /// next end (a cancel, its timeout) tries again, and once the host has closed the error is published (this
    /// worker is going away).
    pub(super) fn end(&self, id: &str, result: Value) -> Result<Value, String> {
        if let Some(existing) = self.claim_finishing(id, &result)? {
            return existing;
        }
        let result = self.finalize_worktree(id, result);
        let (event, ended) = self.commit_task_end(id, result)?;
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
        let (status, summary) = worktree_commit_meta(verdict.as_ref(), &result);
        let paths = worktree_changes(&record, baseline.as_ref());
        let message = wt::commit_message(&summary, &record.name, Some(id), &status);
        let (info, empty) = commit_worktree_record(
            id,
            &mut record,
            &status,
            &message,
            paths.as_deref(),
            self.inner.keep_worktrees,
        );
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

fn worktree_commit_meta(verdict: Option<&Value>, result: &Value) -> (String, String) {
    let status = verdict
        .and_then(|v| v["status"].as_str())
        .or(result["status"].as_str())
        .unwrap_or("")
        .to_string();
    let summary = match verdict.and_then(|v| v["summary"].as_str()) {
        Some(summary) => summary.to_string(),
        None => match (&result["value"], &result["error"]) {
            (Value::String(value), _) => value.clone(),
            (_, Value::String(error)) => error.clone(),
            _ => String::new(),
        },
    };
    (status, summary)
}

fn extract_spawn_kwargs<'a>(
    payload: &'a Map<String, Value>,
    empty: &'a Map<String, Value>,
) -> Result<&'a Map<String, Value>, String> {
    let kwargs = match payload.get("kwargs") {
        None => empty,
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
    Ok(kwargs)
}

fn cleanup_on_err<T>(result: Result<T, String>, worktree: &Option<WorktreeRecord>) -> Result<T, String> {
    if result.is_err()
        && let Some(record) = worktree
    {
        wt::remove_child_worktree(record, true);
    }
    result
}

fn parse_thinking_or_cleanup(raw: Option<&Value>, worktree: &Option<WorktreeRecord>) -> Result<Option<String>, String> {
    let parsed = raw.map(model_scope::thinking_level).transpose();
    cleanup_on_err(parsed, worktree)
}

fn format_spawn_reply(
    task_id: String,
    spec: SpawnSpec,
    worktree: Option<WorktreeRecord>,
    record_error: Option<String>,
) -> Value {
    let mut worktree = worktree.map_or(
        Value::Null,
        |w| json!({ "branch": w.branch, "path": w.path, "cwd": w.cwd }),
    );
    if let Some(error) = record_error {
        worktree["record_error"] = error.into();
    }
    json!({
        "rlm_child_id": task_id,
        "name": spec.name,
        "session_dir": "",
        "model": spec.model.unwrap_or_default(),
        "timeout_ms": spec.timeout_ms,
        "parent_branch_anchor": spec.fork_at.unwrap_or_default(),
        "worktree": worktree,
    })
}

fn resolve_child_reply_value(text: &str, verdict: Option<&Value>) -> Result<Value, String> {
    if text.trim().is_empty() && verdict.is_none() {
        return Err("Agent produced no assistant result at the completed tip".into());
    }
    Ok(if text.trim().is_empty() {
        verdict.map_or(Value::Null, |v| v["summary"].clone())
    } else {
        json!(text)
    })
}

fn assemble_child_result(value: Value, verdict: Option<Value>, check: Value, invalid: bool, has_before: bool) -> Value {
    let nothing_checked = verdict.is_none() && !invalid && !has_before;
    let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                             "verdict": verdict.clone().unwrap_or(Value::Null) });
    if !nothing_checked {
        result["check"] = check;
    }
    if verdict.is_none() {
        result["unverified"] = json!(true);
    }
    result
}

fn terminal_state_name(status: Option<&str>) -> String {
    match status {
        Some("succeeded") => "completed",
        Some(other) => other,
        None => "failed",
    }
    .to_string()
}

fn owner_lane_of(state: &State, id: &str) -> String {
    state
        .task(id)
        .and_then(|t| t.parent_id.clone())
        .and_then(|p| state.task(&p).and_then(|t| t.lane_name.clone()))
        .unwrap_or_else(|| "main".into())
}

fn build_detached_end(
    id: &str,
    task: &Task,
    owner_lane: String,
    closed: bool,
    outcome: Result<Value, String>,
) -> Result<Option<DetachedEnd>, String> {
    match outcome {
        Ok(_) if !closed => Ok(Some(DetachedEnd {
            task: task.record.clone(),
            kind: "child_done",
            owner_lane,
            awaited: task.waiters > 0,
            fetch: format!("await rlm.collect([\"{id}\"])"),
        })),
        Ok(_) => Ok(None),
        Err(error) => Err(error),
    }
}

fn commit_worktree_ok(
    id: &str,
    record: &mut WorktreeRecord,
    status: &str,
    commit: wt::CommitOutcome,
    keep: bool,
) -> (Value, bool) {
    record.state = if commit.commit.is_some() && status != "passed" && status != "succeeded" {
        "failed"
    } else {
        "ended"
    }
    .into();
    record.commit = Some(commit.commit.clone());
    let empty = commit.commit.is_none();
    let saved = if empty && !keep {
        wt::remove_child_worktree(record, true);
        Ok(())
    } else {
        wt::save_record(record)
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
    (info, empty)
}

fn commit_worktree_record(
    id: &str,
    record: &mut WorktreeRecord,
    status: &str,
    message: &str,
    paths: Option<&[String]>,
    keep: bool,
) -> (Value, bool) {
    match wt::commit_child_worktree(record, message, paths) {
        Ok(commit) => commit_worktree_ok(id, record, status, commit, keep),
        Err(error) => {
            record.state = "failed".into();
            let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                "base": record.base, "commit": null, "changed_files": [], "diffstat": "",
                "error": format!("committing the worktree failed: {}", error.message) });
            if let Err(error) = wt::save_record(record) {
                info["record_error"] = error.message.into();
            }
            (info, false)
        }
    }
}
