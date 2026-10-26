//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "agent_child.rs"]
mod agent_child;
#[path = "agents_fixture.rs"]
mod agents_fixture;
#[path = "contracts.rs"]
mod contracts;
#[path = "fork.rs"]
mod fork;
#[path = "git_baseline.rs"]
mod git_baseline;
#[path = "idle_pool.rs"]
mod idle_pool;
#[path = "idle_sweep.rs"]
mod idle_sweep;
#[path = "kept_contracts.rs"]
mod kept_contracts;
#[path = "kernel_pool.rs"]
mod kernel_pool;
#[path = "kernel_pool_real.rs"]
mod kernel_pool_real;
#[path = "lane_routing.rs"]
mod lane_routing;
#[path = "lanes.rs"]
mod lanes;
#[path = "lanes_api.rs"]
mod lanes_api;
#[path = "orphan_prune.rs"]
mod orphan_prune;
#[path = "pool_memory.rs"]
mod pool_memory;
#[path = "predict_fixture.rs"]
mod predict_fixture;
#[path = "reconcile_fixture.rs"]
mod reconcile_fixture;
#[path = "schema_fixture.rs"]
mod schema_fixture;
#[path = "spawn.rs"]
mod spawn;
#[path = "spawn_contracts.rs"]
mod spawn_contracts;
#[path = "spawn_model.rs"]
mod spawn_model;
#[path = "spawn_worktrees.rs"]
mod spawn_worktrees;
#[path = "task_store.rs"]
mod task_store;
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

#[test]
fn every_test_file_is_built() {
    let (merged, manifest) = (include_str!("it.rs"), include_str!("../Cargo.toml"));
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && name != "it.rs" {
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            assert!(
                listed,
                "tests/{name} is neither a module of tests/it.rs nor a [[test]] of Cargo.toml"
            );
        }
    }
}
