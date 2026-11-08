//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod server_peer;
#[path = "../../server/tests/support/mod.rs"]
mod support;

#[path = "fixture.rs"]
mod fixture;
#[path = "lost.rs"]
mod lost;
#[path = "radius.rs"]
mod radius;
#[path = "radius_connection.rs"]
mod radius_connection;
#[path = "replica.rs"]
mod replica;
#[path = "unix.rs"]
mod unix;
