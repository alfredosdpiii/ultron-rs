//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "client_fixture.rs"]
mod client_fixture;
#[path = "config.rs"]
mod config;
#[path = "dies_connection.rs"]
mod dies_connection;
#[path = "gateway_command.rs"]
mod gateway_command;
#[path = "gateway_fixture.rs"]
mod gateway_fixture;
#[path = "handshake_close.rs"]
mod handshake_close;
#[path = "http_fixture.rs"]
mod http_fixture;
#[path = "http_resumption.rs"]
mod http_resumption;
#[path = "js_parity.rs"]
mod js_parity;
#[path = "oauth_fixture.rs"]
mod oauth_fixture;
#[path = "oauth_ported.rs"]
mod oauth_ported;
#[path = "oauth_store_update.rs"]
mod oauth_store_update;
#[path = "ordering_fixture.rs"]
mod ordering_fixture;
#[path = "runtime_fixture.rs"]
mod runtime_fixture;
#[path = "server_requests.rs"]
mod server_requests;
#[path = "sign_in_hint.rs"]
mod sign_in_hint;
#[path = "validation_fixture.rs"]
mod validation_fixture;
