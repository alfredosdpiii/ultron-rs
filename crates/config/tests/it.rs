//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "autoreview_ban.rs"]
mod autoreview_ban;
#[path = "command_values.rs"]
mod command_values;
#[path = "durable_model.rs"]
mod durable_model;
#[path = "fixture.rs"]
mod fixture;
#[path = "ported.rs"]
mod ported;
#[path = "settings_keys.rs"]
mod settings_keys;
#[path = "worker_settings.rs"]
mod worker_settings;
