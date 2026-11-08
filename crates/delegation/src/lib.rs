//! Host side of `rlm.spawn` / `rlm.collect` / `rlm.finish` / `rlm.merge` and background tasks, ported from Ultron's
//! `task-store.ts`, `worktrees.ts`, `workspace-snapshot.ts`, `verdict.ts` and the spawn/collect/merge handling of
//! `native-host.ts`, with host-side waits that never hold a cell (the REPL lanes they park are `crates/lanes`).

pub mod agents;
pub mod checkpoints;
pub mod child;
pub mod contract;
pub mod definitions;
pub mod estimate;
pub mod fork;
pub mod git_baseline;
pub mod host;
pub mod procedure;
pub mod progress;
pub mod skill_runs;
pub mod spawn_contract;
pub mod task_store;
pub mod usage;
pub mod verdict;
pub mod workflow_worktrees;
pub mod workspace;
pub mod worktrees;

/// A subagent's or typed task's `timeout_ms` when it names none, and the most it may name (native-host.ts).
const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// `timeout_ms` of a spawn or task request: absent is the default; else an integer from 1 to the maximum.
fn timeout_ms(value: Option<&serde_json::Value>) -> Result<u64, String> {
    match value {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(value) => value
            .as_f64()
            .filter(|t| t.fract() == 0.0 && (1.0..=MAX_TIMEOUT_MS as f64).contains(t))
            .map(|t| t as u64)
            .ok_or_else(|| format!("timeout_ms must be an integer between 1 and {MAX_TIMEOUT_MS}")),
    }
}
