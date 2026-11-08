//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "agent_child.rs"]
mod agent_child;
#[path = "agents_fixture.rs"]
mod agents_fixture;
#[path = "budget_estimate.rs"]
mod budget_estimate;
#[path = "contracts.rs"]
mod contracts;
#[path = "fork.rs"]
mod fork;
#[path = "git_baseline.rs"]
mod git_baseline;
#[path = "journal_failures.rs"]
mod journal_failures;
#[path = "kept_contracts.rs"]
mod kept_contracts;
#[path = "lane_routing.rs"]
mod lane_routing;
#[path = "lanes.rs"]
mod lanes;
#[path = "lanes_api.rs"]
mod lanes_api;
#[path = "orphan_prune.rs"]
mod orphan_prune;
#[path = "predict_fixture.rs"]
mod predict_fixture;
#[path = "restore_worktrees.rs"]
mod restore_worktrees;
#[path = "schema_fixture.rs"]
mod schema_fixture;
#[path = "skill_procedure.rs"]
mod skill_procedure;
#[path = "skill_runs.rs"]
mod skill_runs;
#[path = "spawn.rs"]
mod spawn;
#[path = "spawn_context.rs"]
mod spawn_context;
#[path = "spawn_contracts.rs"]
mod spawn_contracts;
#[path = "spawn_model.rs"]
mod spawn_model;
#[path = "spawn_worktrees.rs"]
mod spawn_worktrees;
#[path = "task_store.rs"]
mod task_store;
#[path = "usage_ceilings.rs"]
mod usage_ceilings;
#[path = "usage_fixture.rs"]
mod usage_fixture;
#[path = "usage_reinvoke.rs"]
mod usage_reinvoke;
#[path = "verdict.rs"]
mod verdict;
#[path = "workflow_worktrees.rs"]
mod workflow_worktrees;
#[path = "worktrees.rs"]
mod worktrees;
