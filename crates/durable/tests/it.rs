//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "conformance.rs"]
mod conformance;
#[path = "document_state.rs"]
mod document_state;
#[path = "session_checkpoints_migrations.rs"]
mod session_checkpoints_migrations;
#[path = "session_documents.rs"]
mod session_documents;
#[path = "session_forks.rs"]
mod session_forks;
#[path = "session_tables.rs"]
mod session_tables;
#[path = "session_watches.rs"]
mod session_watches;

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
