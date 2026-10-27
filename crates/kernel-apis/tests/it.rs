//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "artifacts.rs"]
mod artifacts;
#[path = "cell_hints.rs"]
mod cell_hints;
#[path = "context.rs"]
mod context;
#[path = "context_state_cache.rs"]
mod context_state_cache;
#[path = "diagram.rs"]
mod diagram;
#[path = "diagram_contrast.rs"]
mod diagram_contrast;
#[path = "durable_work.rs"]
mod durable_work;
#[path = "family.rs"]
mod family;
#[path = "fixture.rs"]
mod fixture;
#[path = "goal.rs"]
mod goal;
#[path = "images.rs"]
mod images;
#[path = "instances.rs"]
mod instances;
#[path = "loop_detector.rs"]
mod loop_detector;
#[path = "memory_auto.rs"]
mod memory_auto;
#[path = "memory_journal.rs"]
mod memory_journal;
#[path = "memory_records.rs"]
mod memory_records;
#[path = "progress.rs"]
mod progress;
#[path = "prompt_schedule.rs"]
mod prompt_schedule;
#[path = "schedules.rs"]
mod schedules;
#[path = "skills_invoke.rs"]
mod skills_invoke;
#[path = "tool_gate.rs"]
mod tool_gate;

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
