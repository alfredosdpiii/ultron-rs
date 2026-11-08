//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "atomic_files.rs"]
mod atomic_files;
#[path = "command_values.rs"]
mod command_values;
#[path = "durable_model.rs"]
mod durable_model;
#[path = "env.rs"]
mod env;
#[path = "fixture.rs"]
mod fixture;
#[path = "js_values.rs"]
mod js_values;
#[path = "lock.rs"]
mod lock;
#[path = "ported.rs"]
mod ported;
#[path = "service_keys.rs"]
mod service_keys;
#[path = "settings_keys.rs"]
mod settings_keys;
#[path = "worker_settings.rs"]
mod worker_settings;
