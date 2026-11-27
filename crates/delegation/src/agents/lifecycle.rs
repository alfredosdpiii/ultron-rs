use super::*;

impl Agents {
    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    pub(super) fn finish(&self, id: &str, result: Value) -> Result<Value, String> {
        {
            let mut state = self.state();
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
            if let Some(task) = state.task_mut(id) {
                task.finishing = true;
            }
        }
        self.commit(id, result)
    }

    fn commit_error(
        &self,
        mut state: MutexGuard<'_, State>,
        id: &str,
        error: String,
        meant: &str,
        lane: Option<String>,
    ) -> Result<Value, String> {
        if let Some(task) = state.task_mut(id) {
            task.signal.abort(error.clone());
        }
        self.settle_task(&mut state, id, meant);
        drop(state);
        if let Some(lane) = lane {
            self.abort_lane(&lane);
        }
        self.inner.changed.notify_all();
        Err(error)
    }

    fn finish_commit_ok(
        &self,
        mut state: MutexGuard<'_, State>,
        id: &str,
        result: Value,
        owner_lane: String,
        closed: bool,
    ) -> Result<Value, String> {
        let status = settlement_status(result["status"].as_str());
        self.settle_task(&mut state, id, status);
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        let record = public_record(&task.record);
        let event = (task.notify.is_some() && !closed).then(|| TaskEnd {
            task: record.clone(),
            kind: task.notify.unwrap_or("task_done"),
            owner_lane,
            root_id: task.reservation.as_ref().map(|r| r.root_id().to_string()),
            awaited: task.waiters > 0,
            fetch: fetch_hint(&task.definition, id),
            cost: task.cost,
        });
        let (cost, model) = (task.cost, task.model.clone());
        drop(state);
        self.inner.changed.notify_all();
        if let Some(hook) = &self.inner.on_task_end {
            hook(&record, cost, model.as_deref());
        }
        if let Some(event) = event {
            let _ = sync::lock(&self.inner.events).send(event);
        }
        Ok(result)
    }

    /// The terminal transition of a task this caller is finishing.
    pub(super) fn commit(&self, id: &str, result: Value) -> Result<Value, String> {
        let terminal = match result["status"].as_str() {
            Some("succeeded") => "completed",
            Some(other) => other,
            None => "failed",
        }
        .to_string();
        let meant = settlement_status(result["status"].as_str());
        let mut state = self.state();
        let committed = state.journal.transition(id, &terminal, Some(result));
        let now = self.now();
        let closed = state.closed;
        let owner_lane = state
            .parent_of(id)
            .and_then(|p| state.task(&p).and_then(|t| t.lane.clone()))
            .unwrap_or_else(|| "main".into());
        let outcome = committed.map(|record| {
            if let Some(task) = state.task_mut(id) {
                task.record = record.clone();
            }
            record["result"].clone()
        });
        let Some(task) = state.task_mut(id) else {
            return Err("Unknown Ultron task".into());
        };
        task.ended_at.get_or_insert(now);
        task.outcome = Some(outcome.clone());
        task.done.abort("ended");
        let lane = task.lane.clone();
        match outcome {
            Err(error) => self.commit_error(state, id, error, meant, lane),
            Ok(result) => self.finish_commit_ok(state, id, result, owner_lane, closed),
        }
    }

    /// `cancel`: the task (when it has not ended) and every unfinished descendant; siblings keep running.
    pub(super) fn cancel(&self, id: &str, reason: &str) -> Result<Value, String> {
        let begun = {
            let mut state = self.state();
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            if task.outcome.is_some() || task.finishing {
                None
            } else {
                task.finishing = true;
                task.signal.abort(reason);
                Some(task.lane.clone())
            }
        };
        let own = match begun {
            Some(lane) => {
                if let Some(lane) = lane {
                    self.abort_lane(&lane);
                }
                self.commit(id, cancelled(reason))
            }
            None => self.wait_outcome(id, None),
        };
        let children: Vec<String> = self
            .state()
            .tasks
            .iter()
            .filter(|t| t.parent_id.as_deref() == Some(id) && t.outcome.is_none())
            .map(|t| t.id.clone())
            .collect();
        let cascade = format!("Ancestor {id} cancelled: {reason}");
        let threads: Vec<_> = children
            .into_iter()
            .map(|child| {
                let (host, cascade) = (self.clone(), cascade.clone());
                std::thread::spawn(move || {
                    let _ = host.cancel(&child, &cascade);
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
        own
    }

    fn poll_wait_outcome(
        &self,
        state: &mut MutexGuard<'_, State>,
        id: &str,
        cell: Option<(&Signal, bool)>,
        cancelled_task: &mut bool,
    ) -> Result<Option<Result<Value, String>>, String> {
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        if let Some(outcome) = &task.outcome {
            return Ok(Some(outcome.clone()));
        }
        if !task.live && !task.finishing {
            return Ok(Some(
                task.record
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "Unknown Ultron task".into()),
            ));
        }
        if let Some((signal, cancel_task)) = cell
            && let Some(reason) = signal.reason()
        {
            if !cancel_task {
                return Ok(Some(Err(format!("Waiting for {id} was cancelled: {reason}"))));
            }
            if !*cancelled_task {
                *cancelled_task = true;
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Waits for a task's terminal result. `cell`: the requesting cell; when it is cancelled the wait ends with an
    /// error (`cancel_task`: and the task is cancelled first, as an invoke's own task is).
    pub(super) fn wait_outcome(&self, id: &str, cell: Option<(&Signal, bool)>) -> Result<Value, String> {
        let listener = cell.and_then(|(signal, _)| {
            let inner = self.inner.clone();
            let id = signal.on_abort(move |_| {
                std::thread::spawn(move || {
                    let _state = sync::lock(&inner.state);
                    inner.changed.notify_all();
                });
            })?;
            Some((signal, id))
        });
        let mut state = self.state();
        if let Some(task) = state.task_mut(id) {
            task.waiters += 1;
        }
        let mut cancelled_task = false;
        let outcome = loop {
            let was_cancelled = cancelled_task;
            if let Some(outcome) = self.poll_wait_outcome(&mut state, id, cell, &mut cancelled_task)? {
                break outcome;
            }
            if cancelled_task && !was_cancelled {
                drop(state);
                let _ = self.cancel(id, "Parent task aborted");
                state = self.state();
                continue;
            }
            state = self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        };
        if let Some(task) = state.task_mut(id) {
            task.waiters -= 1;
        }
        drop(state);
        if let Some((signal, listener)) = listener {
            signal.forget(listener);
        }
        outcome
    }

    /// Whether `id` is `caller`'s own task or a descendant (the root lane sees everything).
    pub(super) fn visible(records: &[Value], id: &str, caller: Option<&str>) -> bool {
        let Some(caller) = caller else {
            return true;
        };
        let mut current = records.iter().find(|r| r["id"] == id);
        while let Some(record) = current {
            if record["id"] == caller {
                return true;
            }
            current = record["parentId"]
                .as_str()
                .and_then(|parent| records.iter().find(|r| r["id"] == parent));
        }
        false
    }

    /// Closes the host: later requests are refused and every unfinished task is cancelled.
    pub fn close(&self) {
        let _admitting = sync::lock(&self.inner.admissions);
        let unfinished: Vec<String> = {
            let mut state = self.state();
            state.closed = true;
            state
                .tasks
                .iter()
                .filter(|t| t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for id in unfinished {
            let _ = self.cancel(&id, "Ultron task host closed");
        }
    }

    /// Run `rlm-child@1` workflow nodes with `worktree=True` / `"auto"` as subagents of `children` (set once).
    pub fn set_worktree_children(&self, children: crate::host::Delegation) {
        let _ = self.inner.worktree_children.set(children);
    }

    /// Admit an `rlm-frame@1` task for `lane` (as `agents.spawn` with this payload, under the lane's task) without
    /// a completion notice: inference waits for its result itself. Returns the task id.
    pub fn spawn_frame(&self, lane: &str, payload: Value) -> Result<String, String> {
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        self.load_tasks()?;
        let parent = self.caller_task(lane);
        let request = self.request(&payload, parent.as_deref())?;
        self.spawn_task(request, None, parent.as_deref(), None)
    }

    /// Cancel task `id` and its subtree with `reason` (inference cancels its frame tasks with the calling cell).
    pub fn cancel_task(&self, id: &str, reason: &str) -> Result<Value, String> {
        self.cancel(id, reason)
    }

    pub(super) fn is_live(&self, id: &str) -> bool {
        self.state().task(id).is_some_and(|t| t.live)
    }

    pub(super) fn record(&self, id: &str) -> Value {
        self.state().task(id).map_or(Value::Null, |t| t.record.clone())
    }

    /// The result of a task this request admitted (or found by its key): awaited when live, else as stored.
    pub(super) fn result_of(&self, id: &str, cell: Option<(&Signal, bool)>) -> Result<Value, String> {
        if self.is_live(id) {
            return self.wait_outcome(id, cell);
        }
        Ok(self.record(id).get("result").cloned().unwrap_or(Value::Null))
    }
}

fn settlement_status(status: Option<&str>) -> &'static str {
    match status {
        Some("succeeded") => "succeeded",
        Some("cancelled") => "cancelled",
        Some("interrupted") => "unknown",
        _ => "failed",
    }
}
