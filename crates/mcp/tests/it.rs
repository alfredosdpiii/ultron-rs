//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

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
#[path = "http_fixture.rs"]
mod http_fixture;
#[path = "http_resumption.rs"]
mod http_resumption;
#[path = "oauth_fixture.rs"]
mod oauth_fixture;
#[path = "oauth_ported.rs"]
mod oauth_ported;
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

#[test]
fn every_test_file_is_built() {
    let (merged, manifest) = (include_str!("it.rs"), include_str!("../Cargo.toml"));
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && name != "it.rs" {
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            assert!(
                listed,
                "tests/{name} is neither a module of tests/it.rs nor a [[test]] of Cargo.toml"
            );
        }
    }
}
