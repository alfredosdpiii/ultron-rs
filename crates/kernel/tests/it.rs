//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "base_env.rs"]
mod base_env;
#[path = "fixture.rs"]
mod fixture;
#[path = "ops.rs"]
mod ops;
#[path = "process.rs"]
mod process;
#[path = "snapshot.rs"]
mod snapshot;
#[path = "snapshot_fixture.rs"]
mod snapshot_fixture;
#[path = "snapshot_key_store.rs"]
mod snapshot_key_store;
#[path = "tree_memory.rs"]
mod tree_memory;
