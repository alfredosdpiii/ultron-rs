//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "auto_setup.rs"]
mod auto_setup;
#[path = "daemon.rs"]
mod daemon;
#[path = "differential.rs"]
mod differential;
#[path = "findings.rs"]
mod findings;
#[path = "hook_differential.rs"]
mod hook_differential;
#[path = "init_differential.rs"]
mod init_differential;
#[path = "installed_hooks.rs"]
mod installed_hooks;
#[path = "project_python.rs"]
mod project_python;
#[path = "registry.rs"]
mod registry;
#[path = "shell_differential.rs"]
mod shell_differential;
#[path = "shell_writes.rs"]
mod shell_writes;
