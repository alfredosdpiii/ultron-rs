//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod common;
mod fake_tasks;

#[path = "cell_hints.rs"]
mod cell_hints;
#[path = "context.rs"]
mod context;
#[path = "context_state_cache.rs"]
mod context_state_cache;
#[path = "durable_work.rs"]
mod durable_work;
#[path = "family.rs"]
mod family;
#[path = "fixture.rs"]
mod fixture;
#[path = "goal.rs"]
mod goal;
#[path = "images.rs"]
mod images;
#[path = "instances.rs"]
mod instances;
#[path = "loop_detector.rs"]
mod loop_detector;
#[path = "memory_auto.rs"]
mod memory_auto;
#[path = "memory_journal.rs"]
mod memory_journal;
#[path = "memory_records.rs"]
mod memory_records;
#[path = "progress.rs"]
mod progress;
#[path = "prompt_schedule.rs"]
mod prompt_schedule;
#[path = "schedules.rs"]
mod schedules;
#[path = "sessions.rs"]
mod sessions;
#[path = "skills_invoke.rs"]
mod skills_invoke;
#[path = "tool_gate.rs"]
mod tool_gate;
