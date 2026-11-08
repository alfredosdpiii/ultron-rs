//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod backends_support;

#[path = "escape.rs"]
mod escape;
#[path = "fixtures.rs"]
mod fixtures;
#[path = "launch_env.rs"]
mod launch_env;
#[path = "refusal.rs"]
mod refusal;
#[path = "settings.rs"]
mod settings;
