//! A task host reopened after its worker died (Ultron Durable). The journal's tasks become the host's again:
//! finished results stay collectable and listed, and an unfinished subagent that a durable runtime still runs keeps
//! running: its waiter delivers its reply when it ends, and the result is checked and announced as any child's.
//! Unfinished tasks nothing runs read as the journal recovered them (`interrupted`, never replayed).
//!
//! The journal follows `task-store.ts` (reopening records every unfinished task as interrupted, which is what the
//! TypeScript product reads), except for a reattached child: it stays `running` in the journal and its end is
//! journaled as any child's, so a result the parent collected never changes and a later restart reattaches it again
//! (specs/spawn-collect: OneResult and KeepsRunning across two restarts).

use super::*;

/// Waits for a reattached child's reply. It is given the task's signal, aborted when the task is cancelled.
pub type ChildWaiter = Box<dyn FnOnce(Signal) -> Result<String, String> + Send>;

/// A child a durable runtime still runs, as its session kept it.
pub struct Reattached {
    pub waiter: ChildWaiter,
    /// Its contract as [`Delegation::contract_of`] gave it when it was spawned (`None`: it had none, or the session
    /// predates kept contracts): the host checks the child's end against it, as before the restart.
    pub contract: Option<Value>,
    /// The run to repeat for a `{"retry": N}` hop (its signal is replaced by the task's); `None`: a failed
    /// contract is reported without a retry.
    pub run: Option<ChildRun>,
}

impl Delegation {
    /// Load the journal's tasks after a restart. `reattach` is asked about every subagent task the journal held
    /// unfinished before it was reopened (`unfinished`, read from the stored document first) and returns a waiter
    /// (with the child's kept contract) for those a durable runtime still runs. Returns the reattached task ids.
    ///
    /// A kept contract that no longer parses fails the child when it ends ("Contract failed: ..."): the host never
    /// lets a contract it cannot read pass as no contract.
    pub fn restore(
        &self,
        unfinished: &[String],
        mut reattach: impl FnMut(&str) -> Option<Reattached>,
    ) -> Result<Vec<String>, String> {
        let mut waiters = Vec::new();
        {
            let mut state = sync::lock(&self.inner.state);
            let known: Vec<String> = state.tasks.iter().map(|task| task.id.clone()).collect();
            // Asked while the journal reopens: a child a runtime still runs stays `running` in the same write that
            // records the others `interrupted`, so a later restart finds it unfinished and reattaches it again.
            let mut kept: HashMap<String, Reattached> = HashMap::new();
            let records = state.journal.reopen(|record| {
                let id = record["id"].as_str().unwrap_or_default();
                if record["definition"] != CHILD || !unfinished.iter().any(|u| u == id) || known.iter().any(|k| k == id)
                {
                    return false;
                }
                match reattach(id) {
                    Some(reattached) => {
                        kept.insert(id.to_string(), reattached);
                        true
                    }
                    None => false,
                }
            })?;
            // The children's saved worktree records: a reattached child keeps its worktree, and a kept worktree
            // already merged stays merged (`rlm.merge` does not merge it again).
            let saved = self
                .inner
                .workspace
                .as_deref()
                .map(wt::list_records)
                .unwrap_or_default();
            for record in records {
                let id = record["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() || state.task(&id).is_some() {
                    continue;
                }
                let child = record["definition"] == CHILD;
                let (waiter, contract, run) = match kept.remove(&id) {
                    Some(r) => (Some(r.waiter), r.contract, r.run),
                    None => (None, None, None),
                };
                // The Git baseline kept beside a contract with workspace checks: their "before" from now on.
                let git_baseline = contract
                    .as_ref()
                    .and_then(|kept| GitBaseline::from_value(&kept["baseline"]));
                let contract = contract.map(|kept| SpawnContract::from_described(&kept).map(Arc::new));
                let signal = Signal::new();
                let outcome = match &waiter {
                    Some(_) => None,
                    None => {
                        signal.abort("ended");
                        Some(Ok(record["result"].clone()))
                    }
                };
                let lane_name = child.then(|| lanes::lane_names::child_lane(&id));
                let saved = saved
                    .iter()
                    .find(|record| child && record.task_id.as_deref() == Some(id.as_str()));
                let merge_status = saved
                    .filter(|record| record.state == "merged")
                    .map(|_| "merged".to_string());
                // A reattached worktree child keeps its worktree (its saved record names the task): its work is
                // committed on its branch when it ends, and `rlm.merge` finds it.
                // This worker owns it now: its close prunes the worktree once the child has ended.
                let worktree = waiter.as_ref().and(saved.cloned()).map(|mut record| {
                    record.owner = wt::Owner::of_this_process();
                    let _ = wt::save_record(&record);
                    record
                });
                if let (Some(lane), Some(_)) = (&lane_name, &waiter) {
                    state.lane_tasks.insert(lane.clone(), id.clone());
                }
                state.tasks.push(Task {
                    id: id.clone(),
                    parent_id: record["parentId"].as_str().map(String::from),
                    record,
                    outcome,
                    spawn_depth: 0,
                    lane_name,
                    // A run kept for a retry hop still names its child and model.
                    name: run.as_ref().map(|run| run.name.clone()),
                    model: run.as_ref().and_then(|run| run.model.clone()),
                    started_at: None,
                    ended_at: None,
                    timeout_ms: run.as_ref().map(|run| run.timeout_ms).filter(|ms| *ms > 0),
                    verdict: None,
                    verdict_rejections: 0,
                    verdict_problems: Vec::new(),
                    verdict_invalid: false,
                    waiters: 0,
                    signal: signal.clone(),
                    finishing: false,
                    worktree,
                    worktree_baseline: None,
                    outside_before: None,
                    before: None,
                    merge_status,
                    contract: contract.clone().and_then(Result::ok),
                    git_baseline,
                });
                if let Some(waiter) = waiter {
                    let unreadable = contract.and_then(Result::err);
                    waiters.push((id, signal, waiter, run, unreadable));
                }
            }
        }
        let resumed = waiters.iter().map(|(id, ..)| id.clone()).collect();
        for (id, signal, waiter, run, unreadable) in waiters {
            let host = self.clone();
            std::thread::spawn(move || host.run_restored(&id, &signal, waiter, run, unreadable));
        }
        Ok(resumed)
    }

    fn run_restored(
        &self,
        id: &str,
        signal: &Signal,
        waiter: ChildWaiter,
        run: Option<ChildRun>,
        unreadable: Option<String>,
    ) {
        let reply = waiter(signal.clone());
        if signal.is_aborted() {
            return;
        }
        let mut result = self.reply_result(id, reply);
        let contract = sync::lock(&self.inner.state).task(id).and_then(|t| t.contract.clone());
        if let Some(contract) = contract {
            let run = run.map(|run| ChildRun {
                signal: signal.clone(),
                ..run
            });
            result = match self.enforce_contract(id, &contract, run, result, signal) {
                Some(result) => result,
                None => return,
            };
        } else if let Some(error) = unreadable {
            result["status"] = json!("failed");
            result["error"] = json!(format!(
                "Contract failed: the session's kept contract is unreadable: {error}"
            ));
        }
        // As any child's end (its worktree work committed first, then the journal).
        let _ = self.end(id, result);
    }
}
