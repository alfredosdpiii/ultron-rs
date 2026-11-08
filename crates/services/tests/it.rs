//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

#[path = "../../kernel-apis/tests/common/mod.rs"]
mod common;

#[path = "gates_fixture.rs"]
mod gates_fixture;
#[path = "hook_and_values.rs"]
mod hook_and_values;
#[path = "kernel_fixture.rs"]
mod kernel_fixture;
#[path = "local_fixture.rs"]
mod local_fixture;
