//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod chat_support;
mod support;

#[path = "chat_cells.rs"]
mod chat_cells;
#[path = "chat_children.rs"]
mod chat_children;
#[path = "chat_compaction.rs"]
mod chat_compaction;
#[path = "chat_generation.rs"]
mod chat_generation;
#[path = "chat_inbox.rs"]
mod chat_inbox;
#[path = "chat_submissions.rs"]
mod chat_submissions;
#[path = "chat_swarm.rs"]
mod chat_swarm;
#[path = "child_compaction.rs"]
mod child_compaction;
#[path = "scheduler_failure.rs"]
mod scheduler_failure;
#[path = "scheduler_ownership.rs"]
mod scheduler_ownership;
#[path = "scheduler_phases.rs"]
mod scheduler_phases;
#[path = "scheduler_recovery.rs"]
mod scheduler_recovery;
#[path = "scheduler_scheduling.rs"]
mod scheduler_scheduling;
#[path = "typescript_fixture.rs"]
mod typescript_fixture;
