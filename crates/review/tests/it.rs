//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "autoreview.rs"]
mod autoreview;
#[path = "ban.rs"]
mod ban;
#[path = "daemon.rs"]
mod daemon;
#[path = "engine.rs"]
mod engine;
#[path = "engine_env.rs"]
mod engine_env;
#[path = "github_accounts.rs"]
mod github_accounts;
#[path = "github_fixtures.rs"]
mod github_fixtures;
#[path = "github_review.rs"]
mod github_review;
#[path = "ledger.rs"]
mod ledger;
#[path = "prepare_env.rs"]
mod prepare_env;
#[path = "promotion.rs"]
mod promotion;
#[path = "retrying_websocket.rs"]
mod retrying_websocket;
#[path = "review_record.rs"]
mod review_record;
#[path = "rules.rs"]
mod rules;
#[path = "service.rs"]
mod service;
#[path = "state_writes.rs"]
mod state_writes;

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
