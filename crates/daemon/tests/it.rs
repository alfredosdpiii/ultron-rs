//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "agent_markers.rs"]
mod agent_markers;
#[path = "dream.rs"]
mod dream;
#[path = "extension_ui.rs"]
mod extension_ui;
#[path = "fixture.rs"]
mod fixture;
#[path = "instance.rs"]
mod instance;
#[path = "lane_publish.rs"]
mod lane_publish;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "persona_setting.rs"]
mod persona_setting;
#[path = "prompt_schedules.rs"]
mod prompt_schedules;
#[path = "session_control.rs"]
mod session_control;
#[path = "watchdog.rs"]
mod watchdog;
#[path = "worker_settings.rs"]
mod worker_settings;
#[path = "worker_settings_claude_code_children.rs"]
mod worker_settings_claude_code_children;
