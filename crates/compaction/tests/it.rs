//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "bug_summary.rs"]
mod bug_summary;
#[path = "fixture.rs"]
mod fixture;
#[path = "prompt_budget.rs"]
mod prompt_budget;
#[path = "recalled_memory.rs"]
mod recalled_memory;
#[path = "upstream.rs"]
mod upstream;
