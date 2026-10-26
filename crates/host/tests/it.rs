//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "after_cell.rs"]
mod after_cell;
#[path = "base_env.rs"]
mod base_env;
#[path = "bash_notes.rs"]
mod bash_notes;
#[path = "cell.rs"]
mod cell;
#[path = "fixture.rs"]
mod fixture;
#[path = "guard_stats.rs"]
mod guard_stats;
#[path = "journal.rs"]
mod journal;
#[path = "loki.rs"]
mod loki;
#[path = "loki_project_python.rs"]
mod loki_project_python;
#[path = "loki_timeout.rs"]
mod loki_timeout;
#[path = "sandbox_launch_env.rs"]
mod sandbox_launch_env;
#[path = "sandbox_no_base_env.rs"]
mod sandbox_no_base_env;
#[path = "sandbox_refusal.rs"]
mod sandbox_refusal;
#[path = "sandboxed.rs"]
mod sandboxed;
#[path = "sandboxed_backends.rs"]
mod sandboxed_backends;
#[path = "secrets.rs"]
mod secrets;
#[path = "shared_jobs.rs"]
mod shared_jobs;
#[path = "shell.rs"]
mod shell;
#[path = "view_image.rs"]
mod view_image;

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
