//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

#[path = "../../sandbox/tests/backends_support/mod.rs"]
mod backends_support;
#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;

#[path = "after_cell.rs"]
mod after_cell;
#[path = "base_env.rs"]
mod base_env;
#[path = "bash_notes.rs"]
mod bash_notes;
#[path = "cell.rs"]
mod cell;
#[path = "fixture.rs"]
mod fixture;
#[path = "guard_stats.rs"]
mod guard_stats;
#[path = "journal.rs"]
mod journal;
#[path = "js_parity.rs"]
mod js_parity;
#[path = "loki.rs"]
mod loki;
#[path = "loki_project_python.rs"]
mod loki_project_python;
#[path = "loki_timeout.rs"]
mod loki_timeout;
#[path = "protected_paths.rs"]
mod protected_paths;
#[path = "sandbox_launch_env.rs"]
mod sandbox_launch_env;
#[path = "sandbox_no_base_env.rs"]
mod sandbox_no_base_env;
#[path = "sandbox_refusal.rs"]
mod sandbox_refusal;
#[path = "sandboxed.rs"]
mod sandboxed;
#[path = "sandboxed_backends.rs"]
mod sandboxed_backends;
#[path = "secrets.rs"]
mod secrets;
#[path = "shared_jobs.rs"]
mod shared_jobs;
#[path = "shell.rs"]
mod shell;
#[path = "view_image.rs"]
mod view_image;
