//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "claude_parity.rs"]
mod claude_parity;
#[path = "durable_apis.rs"]
mod durable_apis;
#[path = "external_stats.rs"]
mod external_stats;
#[path = "external_turns.rs"]
mod external_turns;
#[path = "mcp_runtime.rs"]
mod mcp_runtime;
#[path = "persona.rs"]
mod persona;
#[path = "root_sink.rs"]
mod root_sink;
#[path = "stuck_cells.rs"]
mod stuck_cells;
#[path = "subscription_root.rs"]
mod subscription_root;
#[path = "ultron_child_model.rs"]
mod ultron_child_model;
#[path = "ultron_children.rs"]
mod ultron_children;
#[path = "watch.rs"]
mod watch;
