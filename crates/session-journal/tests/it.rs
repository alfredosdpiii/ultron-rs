//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "all_messages_fixture.rs"]
mod all_messages_fixture;
#[path = "catalog.rs"]
mod catalog;
#[path = "codec_and_ids.rs"]
mod codec_and_ids;
#[path = "context_projectors.rs"]
mod context_projectors;
#[path = "legacy_names.rs"]
mod legacy_names;
#[path = "listing_order.rs"]
mod listing_order;
#[path = "names_and_run.rs"]
mod names_and_run;
#[path = "storage_fixture.rs"]
mod storage_fixture;
#[path = "torn_tail.rs"]
mod torn_tail;
