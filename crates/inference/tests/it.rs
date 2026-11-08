//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod common;
#[path = "../../ai/tests/claude_subscription_support/isolate.rs"]
mod isolate;

#[path = "claude_code_frames.rs"]
mod claude_code_frames;
#[path = "fixture.rs"]
mod fixture;
#[path = "frame_models.rs"]
mod frame_models;
#[path = "map.rs"]
mod map;
#[path = "providers.rs"]
mod providers;
#[path = "units.rs"]
mod units;
