//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "chat_cells.rs"]
mod chat_cells;
#[path = "chat_children.rs"]
mod chat_children;
#[path = "chat_compaction.rs"]
mod chat_compaction;
#[path = "chat_generation.rs"]
mod chat_generation;
#[path = "chat_inbox.rs"]
mod chat_inbox;
#[path = "chat_migrate.rs"]
mod chat_migrate;
#[path = "chat_submissions.rs"]
mod chat_submissions;
#[path = "chat_swarm.rs"]
mod chat_swarm;
#[path = "child_compaction.rs"]
mod child_compaction;
#[path = "reducer_fixture.rs"]
mod reducer_fixture;
#[path = "scheduler_failure.rs"]
mod scheduler_failure;
#[path = "scheduler_ownership.rs"]
mod scheduler_ownership;
#[path = "scheduler_phases.rs"]
mod scheduler_phases;
#[path = "scheduler_recovery.rs"]
mod scheduler_recovery;
#[path = "scheduler_scheduling.rs"]
mod scheduler_scheduling;
#[path = "supervisor_fixture.rs"]
mod supervisor_fixture;
#[path = "typescript_fixture.rs"]
mod typescript_fixture;

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
