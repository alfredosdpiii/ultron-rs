//! Subagent worktree merging, waiting, collection, progress, cancellation, and finish requests.

use super::*;

fn parse_on_conflict(value: Option<&Value>) -> Result<&'static str, String> {
    match value {
        None => Ok("stop"),
        Some(Value::String(s)) if s == "stop" => Ok("stop"),
        Some(Value::String(s)) if s == "markers" => Ok("markers"),
        Some(Value::String(s)) if s == "skip" => Ok("skip"),
        Some(_) => Err("on_conflict must be \"stop\", \"skip\" or \"markers\"".into()),
    }
}

fn parse_merge_selectors(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value.and_then(Value::as_array) {
        Some(items) => items.iter().map(|s| nonempty_string(Some(s), "selector")).collect(),
        None => Ok(Vec::new()),
    }
}

fn merge_item(base: &Value, extra: Value) -> Value {
    let mut item = base.clone();
    if let (Some(item), Value::Object(extra)) = (item.as_object_mut(), extra) {
        item.extend(extra);
    }
    item
}

impl Delegation {
    fn resolve_merge_children(&self, selectors: Vec<String>, parent: Option<&str>) -> Result<Vec<String>, String> {
        let state = sync::lock(&self.inner.state);
        if selectors.is_empty() {
            return Ok(state
                .tasks
                .iter()
                .filter(|t| t.record["definition"] == CHILD && t.parent_id.as_deref() == parent)
                .filter(|t| {
                    t.worktree.is_some() || matches!(&t.outcome, Some(Ok(result)) if !result["worktree"].is_null())
                })
                .map(|t| t.id.clone())
                .collect());
        }
        for id in &selectors {
            if !state.is_child(id) || Some(id.as_str()) == parent || !state.visible(id, parent) {
                return Err(format!("Unknown RLM child: {id}"));
            }
        }
        Ok(selectors)
    }

    fn wait_for_merge_children(&self, children: &[String], lane: &str, cell: &CellContext) -> Result<(), String> {
        let running = self.unfinished(children);
        let park = cell
            .park
            .as_ref()
            .filter(|_| !running.is_empty() && self.collect_mode(lane) != CollectMode::InCell);
        if let Some(park) = park {
            park(true);
        }
        let ended = self.wait_until(children, None, cell.cancel.as_ref());
        if let Some(park) = park {
            park(false);
        }
        if ended.len() < children.len() {
            return Err(cell
                .cancel
                .as_ref()
                .and_then(Signal::reason)
                .map_or("rlm.merge was cancelled".into(), |r| {
                    format!("rlm.merge was cancelled: {r}")
                }));
        }
        Ok(())
    }

    pub(super) fn merge_request(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        lane: &str,
        cell: &CellContext,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["selectors", "on_conflict", "include_failed"])?;
        let on_conflict = parse_on_conflict(payload.get("on_conflict"))?;
        let selectors = parse_merge_selectors(payload.get("selectors"))?;
        let children = self.resolve_merge_children(selectors, parent)?;
        if (1..children.len()).any(|i| children[..i].contains(&children[i])) {
            return Err("rlm.merge: a child is listed twice".into());
        }
        self.wait_for_merge_children(&children, lane, cell)?;
        let target = sync::lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let Some(target) = target else {
            return Err("rlm.merge needs a working directory, and this session has none".into());
        };
        let _serial = jsrt::sync::lock(&self.inner.merging);
        Ok(self.merge_children(
            &children,
            &target,
            on_conflict,
            payload.get("include_failed") == Some(&Value::Bool(true)),
        ))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper formatting a single conflicted child merge"
    )]
    fn finish_conflict_merge(
        &self,
        id: &str,
        name: &str,
        branch: &str,
        info: &Value,
        base: &Value,
        record: Option<WorktreeRecord>,
        files: Vec<String>,
        conflicts: Vec<wt::ConflictFile>,
        markers: bool,
        message: String,
    ) -> Value {
        let record_error = record.and_then(|mut record| {
            record.state = "conflict".into();
            self.store_record(id, record).err()
        });
        let text = if markers {
            format!(
                "conflict markers written in {}: resolve them, or restore those files",
                files.join(", ")
            )
        } else {
            format!(
                "nothing of {name} was written: {}. Its branch {branch} keeps the work (worktree {})",
                if message.is_empty() {
                    "the merge conflicts"
                } else {
                    message.as_str()
                },
                info["path"].as_str().unwrap_or_default()
            )
        };
        let mut item = merge_item(
            base,
            json!({ "status": "conflict", "files": files,
                "conflicts": conflicts.iter().map(wt::ConflictFile::to_json).collect::<Vec<_>>(),
                "markers": markers, "message": text, "commit": info["commit"] }),
        );
        if let Some(error) = record_error {
            item["record_error"] = error.into();
        }
        item
    }

    fn finish_clean_merge(
        &self,
        id: &str,
        base: &Value,
        record: Option<WorktreeRecord>,
        outcome: wt::MergeOutcome,
    ) -> Value {
        let record_error = record.and_then(|mut record| {
            if !self.inner.keep_worktrees {
                wt::remove_child_worktree(&record, true);
                return None;
            }
            record.state = "merged".into();
            self.store_record(id, record).err()
        });
        let mut item = match outcome {
            wt::MergeOutcome::Merged { files, diffstat } => merge_item(
                base,
                json!({ "status": "merged", "files": files, "diffstat": diffstat }),
            ),
            _ => merge_item(base, json!({ "status": "empty" })),
        };
        if let Some(error) = record_error {
            item["record_error"] = error.into();
        }
        item
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper executing one child worktree merge"
    )]
    fn run_child_merge(
        &self,
        id: &str,
        name: &str,
        info: &Value,
        base: &Value,
        record: Option<WorktreeRecord>,
        target: &Path,
        on_conflict: &str,
    ) -> (Value, bool) {
        let source = wt::MergeSource {
            name: name.to_string(),
            branch: info["branch"].as_str().unwrap_or_default().to_string(),
            base: info["base"].as_str().unwrap_or_default().to_string(),
            commit: info["commit"].as_str().map(String::from),
        };
        let target = info["repo"].as_str().map_or(target, Path::new);
        let outcome = match wt::merge_child_worktree(target, &source, on_conflict == "markers") {
            Ok(outcome) => outcome,
            Err(error) => {
                return (
                    merge_item(base, json!({ "status": "failed", "reason": error.message })),
                    on_conflict != "skip",
                );
            }
        };
        let record = record.or_else(|| {
            wt::list_records(target)
                .into_iter()
                .find(|r| r.task_id.as_deref() == Some(id))
        });
        let status = outcome.status();
        {
            let mut state = sync::lock(&self.inner.state);
            if let Some(task) = state.task_mut(id) {
                task.merge_status = Some(status.to_string());
            }
        }
        self.merged(id, status);
        match outcome {
            wt::MergeOutcome::Conflict {
                files,
                conflicts,
                markers,
                message,
            } => (
                self.finish_conflict_merge(
                    id,
                    name,
                    &source.branch,
                    info,
                    base,
                    record,
                    files,
                    conflicts,
                    markers,
                    message,
                ),
                on_conflict != "skip",
            ),
            other => (self.finish_clean_merge(id, base, record, other), false),
        }
    }

    fn merge_one_child(
        &self,
        id: &str,
        target: &Path,
        on_conflict: &str,
        include_failed: bool,
        stopped_at: Option<&str>,
    ) -> (Value, bool) {
        let (result, record, merge_status) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id);
            (
                task.and_then(|t| t.outcome.clone()).and_then(Result::ok),
                task.and_then(|t| t.worktree.clone()),
                task.and_then(|t| t.merge_status.clone()),
            )
        };
        let result = result.unwrap_or(Value::Null);
        let info = &result["worktree"];
        let name = record
            .as_ref()
            .map(|r| r.name.clone())
            .or_else(|| {
                info["branch"]
                    .as_str()
                    .and_then(|b| b.rsplit('/').next())
                    .map(String::from)
            })
            .unwrap_or_else(|| id.to_string());
        let mut base = json!({ "id": id, "name": name });
        if info.is_object() {
            base["branch"] = info["branch"].clone();
        }
        if let Some(at) = stopped_at {
            return (
                merge_item(
                    &base,
                    json!({ "status": "not_attempted", "reason": format!("stopped at the conflict of {at}") }),
                ),
                false,
            );
        }
        if let Some(early) = check_merge_preconditions(&result, info, merge_status.as_deref(), include_failed, &base) {
            return early;
        }
        if let Some(error) = info["error"].as_str() {
            return (
                merge_item(&base, json!({ "status": "failed", "reason": error })),
                on_conflict != "skip",
            );
        }
        self.run_child_merge(id, &name, info, &base, record, target, on_conflict)
    }

    /// `mergeChildren`: the first conflict stops the rest (`skip` goes on); nothing of a conflicting child is written
    /// unless `markers`. A merged or empty child's worktree and branch are removed; a conflicting one is kept.
    pub(super) fn merge_children(
        &self,
        children: &[String],
        target: &Path,
        on_conflict: &str,
        include_failed: bool,
    ) -> Value {
        let mut results: Vec<Value> = Vec::new();
        let mut stopped_at: Option<String> = None;
        for id in children {
            let (item, stop) = self.merge_one_child(id, target, on_conflict, include_failed, stopped_at.as_deref());
            results.push(item);
            if stop {
                stopped_at = Some(id.clone());
            }
        }
        let ok = results
            .iter()
            .all(|item| ["merged", "empty", "no_worktree"].contains(&item["status"].as_str().unwrap_or("")));
        let merged: Vec<Value> = results
            .iter()
            .filter(|item| item["status"] == "merged")
            .map(|item| item["id"].clone())
            .collect();
        let mut out = json!({ "ok": ok, "merged": merged });
        if let Some(at) = stopped_at {
            out["stopped_at"] = json!(at);
        }
        out["results"] = Value::from(results);
        out["note"] = json!(
            "Merged changes are uncommitted modifications in your working tree (new files untracked); review and commit them yourself."
        );
        out
    }

    /// Saves the task's worktree record to disk and keeps it as the task's record; the save's error, if any.
    pub(super) fn store_record(&self, id: &str, record: WorktreeRecord) -> Result<(), String> {
        let saved = wt::save_record(&record).map_err(|error| error.message);
        let mut state = sync::lock(&self.inner.state);
        if let Some(task) = state.task_mut(id)
            && task.worktree.is_some()
        {
            task.worktree = Some(record);
        }
        saved
    }

    pub fn cancel(&self, id: &str, reason: &str) -> Result<Value, String> {
        let own = self.end(
            id,
            json!({ "status": "cancelled", "error": reason, "verification": "unverified" }),
        );
        let children: Vec<String> = {
            let state = sync::lock(&self.inner.state);
            state
                .tasks
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(id) && t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for child in children {
            let _ = self.cancel(&child, &format!("Ancestor {id} cancelled: {reason}"));
        }
        own
    }

    /// The children a collect selects: visible to the caller, not the caller itself, in admission order.
    pub(super) fn selected(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<Vec<String>, String> {
        jsrt::payload::fields(payload, &["selectors", "timeout_ms"])?;
        let selectors = match payload.get("selectors").and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .map(|s| nonempty_string(Some(s), "selector"))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let state = sync::lock(&self.inner.state);
        Ok(state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && Some(t.id.as_str()) != parent)
            .filter(|t| state.visible(&t.id, parent))
            .filter(|t| selectors.is_empty() || selectors.contains(&t.id))
            .map(|t| t.id.clone())
            .collect())
    }

    /// `rlm.progress`: each of `ids` still running, with its level and forecasts ([`crate::progress::report`]).
    pub(super) fn progress(&self, ids: &[String]) -> Vec<Value> {
        let running: Vec<Value> = {
            let state = sync::lock(&self.inner.state);
            ids.iter()
                .filter_map(|id| state.task(id))
                .filter(|t| t.outcome.is_none())
                .map(|t| {
                    json!({"id": t.id, "name": t.name, "lane": t.lane_name, "started": t.started_at,
                        "timeout": t.timeout_ms})
                })
                .collect()
        };
        let hook = jsrt::sync::lock(&self.inner.transcripts).clone();
        let now = (self.inner.now)();
        running
            .into_iter()
            .map(|task| {
                let messages = match (&hook, task["lane"].as_str()) {
                    (Some(hook), Some(lane)) => hook(lane).unwrap_or_default(),
                    _ => Vec::new(),
                };
                let started = task["started"]
                    .as_f64()
                    .filter(|at| *at > 0.0)
                    .or_else(|| {
                        messages
                            .iter()
                            .filter_map(|m| m["timestamp"].as_f64())
                            .find(|at| *at > 0.0)
                    })
                    .unwrap_or(now);
                let timeout = task["timeout"].as_u64();
                let mut entry = crate::progress::report(&messages, now, (now - started).max(0.0), timeout);
                entry["id"] = task["id"].clone();
                entry["name"] = task["name"].clone();
                entry
            })
            .collect()
    }

    pub(super) fn unfinished(&self, ids: &[String]) -> Vec<String> {
        let state = sync::lock(&self.inner.state);
        ids.iter()
            .filter(|id| state.task(id).is_some_and(|t| t.outcome.is_none()))
            .cloned()
            .collect()
    }

    pub(super) fn collected(&self, outcomes: Vec<(String, Result<Value, String>)>) -> Result<Value, String> {
        let results = outcomes
            .into_iter()
            .map(|(id, outcome)| outcome.map(|result| json!({ "id": id, "result": result })))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "results": results }))
    }

    /// `rlm.list_subagents` of `lane` with what the TUI's views show besides (never given to a cell): each
    /// subagent's spawn `name` and `model`, and its `startedAt` / `endedAt` (ms since the epoch) when known.
    pub fn subagent_views(&self, lane: &str) -> Result<Value, String> {
        let mut listed = self.handle("rlm.list_subagents", json!({}), lane)?;
        let state = sync::lock(&self.inner.state);
        for view in listed["subagents"].as_array_mut().into_iter().flatten() {
            let Some(task) = view["id"].as_str().and_then(|id| state.task(id)) else {
                continue;
            };
            if let Some(name) = &task.name {
                view["name"] = json!(name);
            }
            if let Some(model) = &task.model {
                view["model"] = json!(model);
            }
            for (key, at) in [("startedAt", task.started_at), ("endedAt", task.ended_at)] {
                if let Some(at) = at.filter(|at| *at > 0.0) {
                    view[key] = json!(at);
                }
            }
        }
        Ok(listed)
    }

    /// Unfinished children started from `lane` ("main" for the root), in admission order: what a turn scheduler
    /// waits for (with [`Delegation::wait`] or the [`DetachedEnd`] channel) after the model ends its turn.
    pub fn running(&self, lane: &str) -> Vec<String> {
        let state = sync::lock(&self.inner.state);
        let parent = state.lane_tasks.get(lane).cloned();
        state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && t.outcome.is_none() && t.parent_id == parent)
            .map(|t| t.id.clone())
            .collect()
    }

    /// What the finished children started from `lane` left undone (`rlm.finish(left=[...])`), in admission order:
    /// each child's id, spawn name and items. Children that left nothing are not listed.
    pub fn left_items(&self, lane: &str) -> Vec<(String, String, Vec<String>)> {
        let state = sync::lock(&self.inner.state);
        let parent = state.lane_tasks.get(lane).cloned();
        state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && t.parent_id == parent)
            .filter_map(|t| {
                let items = crate::verdict::left_items(t.outcome.as_ref()?.as_ref().ok()?);
                (!items.is_empty()).then(|| (t.id.clone(), t.name.clone().unwrap_or_else(|| t.id.clone()), items))
            })
            .collect()
    }

    /// Host-side wait for tasks' results (in the given order), without holding any kernel. `timeout` bounds the
    /// whole wait; a task still running when it passes is left out.
    pub fn wait(&self, ids: &[String], timeout: Option<Duration>) -> Vec<(String, Result<Value, String>)> {
        self.wait_until(ids, timeout, None)
    }

    fn wait_for_tasks_loop(
        &self,
        mut state: std::sync::MutexGuard<'_, State>,
        ids: &[String],
        deadline: Option<std::time::Instant>,
        cancel: Option<&Signal>,
    ) -> Vec<(String, Result<Value, String>)> {
        loop {
            let pending = ids.iter().any(|id| state.task(id).is_some_and(|t| t.outcome.is_none()));
            let left = deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
            if !pending || left.is_some_and(|l| l.is_zero()) || cancel.is_some_and(Signal::is_aborted) {
                break;
            }
            state = match left {
                Some(left) => {
                    self.inner
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner),
            };
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters -= 1;
                if let Some(outcome) = &task.outcome {
                    out.push((id.clone(), outcome.clone()));
                }
            }
        }
        out
    }

    /// [`Delegation::wait`] that also ends when `cancel` is aborted.
    pub fn wait_until(
        &self,
        ids: &[String],
        timeout: Option<Duration>,
        cancel: Option<&Signal>,
    ) -> Vec<(String, Result<Value, String>)> {
        let listener = cancel.and_then(|cancel| {
            let inner = self.inner.clone();
            cancel.on_abort(move |_| {
                std::thread::spawn(move || {
                    let _state = sync::lock(&inner.state);
                    inner.changed.notify_all();
                });
            })
        });
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut state = sync::lock(&self.inner.state);
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters += 1;
            }
        }
        let out = self.wait_for_tasks_loop(state, ids, deadline, cancel);
        if let (Some(cancel), Some(listener)) = (cancel, listener) {
            cancel.forget(listener);
        }
        out
    }

    /// Requests now waiting on a task's result.
    pub fn waiters(&self, id: &str) -> usize {
        sync::lock(&self.inner.state).task(id).map_or(0, |t| t.waiters)
    }

    pub fn tasks(&self) -> Result<Vec<Value>, String> {
        sync::lock(&self.inner.state).journal.list()
    }

    pub(super) fn finish_request(
        &self,
        mut payload: Map<String, Value>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        if parent.is_none() {
            let finish = jsrt::sync::lock(&self.inner.root).finish.clone();
            if let Some(finish) = finish {
                return finish(payload);
            }
        }
        let mut state = sync::lock(&self.inner.state);
        let id = parent
            .filter(|id| state.is_child(id))
            .ok_or("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply")?
            .to_string();
        let worktree = state.worktree_of(Some(&id)).cloned();
        let workspace = state
            .workspace_of(Some(&id), self.inner.workspace.as_deref())
            .map(|w| w.display().to_string());
        let task = state.task_mut(&id).ok_or("Unknown Ultron task")?;
        if task.outcome.is_some() || task.finishing {
            return Err("rlm.finish: this subagent has already ended".into());
        }
        if task.verdict_invalid {
            return Err("rlm.finish: no attempts left; end your turn, your reply is returned unverified".into());
        }
        normalize_worktree_changed_files(&mut payload, worktree.as_ref());
        let checked = validate_verdict(&payload, workspace.as_deref()).and_then(|verdict| {
            let problems = task
                .contract
                .as_ref()
                .map_or_default(|contract| contract.returns_problems(&verdict["outputs"]));
            if problems.is_empty() {
                Ok(verdict)
            } else {
                Err(problems)
            }
        });
        apply_verdict_outcome(task, checked)
    }
}

fn check_merge_preconditions(
    result: &Value,
    info: &Value,
    merge_status: Option<&str>,
    include_failed: bool,
    base: &Value,
) -> Option<(Value, bool)> {
    if !info.is_object() {
        return Some((
            merge_item(
                base,
                json!({ "status": "no_worktree",
                    "reason": "it ran in the shared tree: its changes are already there" }),
            ),
            false,
        ));
    }
    if merge_status == Some("merged") {
        return Some((
            merge_item(
                base,
                json!({ "status": "merged", "files": info["changed_files"], "note": "already merged" }),
            ),
            false,
        ));
    }
    let verdict = &result["verdict"];
    let passed = result["status"] == "succeeded" && (!verdict.is_object() || verdict["status"] == "passed");
    if !passed && !include_failed {
        let status = result["status"].as_str().unwrap_or("unknown");
        let verdict_note = verdict["status"]
            .as_str()
            .map_or(String::new(), |v| format!(", verdict {v}"));
        let mut item = merge_item(
            base,
            json!({ "status": "skipped",
                "reason": format!("the child did not succeed ({status}{verdict_note}); include_failed=True merges it anyway") }),
        );
        if !info["commit"].is_null() {
            item["commit"] = info["commit"].clone();
        }
        return Some((item, false));
    }
    None
}

fn normalize_worktree_changed_files(payload: &mut Map<String, Value>, worktree: Option<&WorktreeRecord>) {
    if let (Some(worktree), Some(Value::Array(files))) = (worktree, payload.get_mut("changed_files")) {
        for file in files.iter_mut() {
            if let Value::String(path) = file {
                *path = wt::in_worktree(path, worktree);
            }
        }
    }
}

fn apply_verdict_outcome(task: &mut Task, checked: Result<Value, Vec<String>>) -> Result<Value, String> {
    match checked {
        Ok(verdict) => {
            let status = verdict["status"].clone();
            task.verdict = Some(verdict);
            Ok(json!({ "recorded": true, "status": status,
                "next": "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran." }))
        }
        Err(problems) => {
            task.verdict_rejections += 1;
            let list = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
            task.verdict_problems = problems;
            let left = MAX_VERDICT_REJECTIONS + 1 - task.verdict_rejections.min(MAX_VERDICT_REJECTIONS + 1);
            if left > 0 || task.verdict.is_some() {
                let tail = if task.verdict.is_some() {
                    "; your earlier verdict stands until then".to_string()
                } else {
                    format!(" ({left} attempt{} left)", if left == 1 { "" } else { "s" })
                };
                return Err(format!(
                    "rlm.finish rejected:\n{list}\nFix these and call rlm.finish again{tail}."
                ));
            }
            task.verdict_invalid = true;
            Err(format!(
                "rlm.finish rejected:\n{list}\nNo attempts left: end your turn; your reply is returned unverified, with these problems."
            ))
        }
    }
}
