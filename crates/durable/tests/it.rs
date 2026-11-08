//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod replay_support;
mod session_support;

#[path = "conformance.rs"]
mod conformance;
#[path = "session_checkpoints_migrations.rs"]
mod session_checkpoints_migrations;
#[path = "session_documents.rs"]
mod session_documents;
#[path = "session_forks.rs"]
mod session_forks;
#[path = "session_tables.rs"]
mod session_tables;
