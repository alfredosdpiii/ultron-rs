use std::fs;
use std::path::Path;

/// A crate with `autotests = false` builds its integration tests as one binary, `tests/it.rs`, whose modules are the
/// other `tests/*.rs` files (`#[path = "x.rs"]`), plus any file that needs a binary of its own (a `[[test]]` in
/// Cargo.toml). A file in neither list would never be compiled or run. Crates on cargo's default `autotests` build
/// every `tests/*.rs` themselves, so they are skipped. Subdirectories (`support/`, `*_support/`) are modules pulled in
/// by `mod` and are not checked.
#[test]
fn every_test_file_is_built() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut missing = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        let dir = entry.unwrap().path();
        let Ok(manifest) = fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        if !manifest.contains("autotests = false") {
            continue;
        }
        let Ok(files) = fs::read_dir(dir.join("tests")) else {
            continue;
        };
        let merged = fs::read_to_string(dir.join("tests/it.rs")).unwrap_or_default();
        let krate = dir.file_name().unwrap().to_string_lossy().into_owned();
        for file in files {
            let file = file.unwrap();
            let name = file.file_name().into_string().unwrap();
            if !file.file_type().unwrap().is_file() || !name.ends_with(".rs") || name == "it.rs" {
                continue;
            }
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            if !listed {
                missing.push(format!(
                    "crates/{krate}/tests/{name} is neither a module of tests/it.rs nor a [[test]] of \
                     crates/{krate}/Cargo.toml"
                ));
            }
        }
    }
    missing.sort();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A test that builds a process or session environment of its own (an `ultron` started with a cleared environment,
/// a `session_core::RunEnv`, or a `cli::Env` it runs a command or a session with) turns both memory services off by default: an unset `ULTRON_HINDSIGHT_URL` is
/// Hindsight's default URL and an unset `ULTRON_MEMORANTADO_URL` lets a session detect the user's own memorantado,
/// so a test would read (and could write) the user's real memory. `gates.sh`'s own `ULTRON_MEMORANTADO_URL=off`
/// never reaches such a process. A test that exercises detection sets its own URL after the defaults. A file whose
/// environment is copied whole from a checked harness's command, or that only reads models or usage and never
/// starts a session, is listed in `EXEMPT`.
#[test]
fn every_test_environment_turns_memory_services_off() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("tests"), &mut files);
    }
    const EXEMPT: [&str; 6] = [
        // The environment of `daemon_common::Home::instance_command` / `common::Sandbox::ultron`, copied whole.
        "cli/tests/daemon_durable_session_options.rs",
        "cli/tests/resume_unavailable_model.rs",
        // No session starts: no memory backend is chosen.
        "cli/tests/service_cli.rs",
        "cli/tests/subcommand_units.rs",
        "session-core/tests/copilot_models.rs",
        "session-core/tests/subscription_models.rs",
    ];
    let mut missing = Vec::new();
    for file in files {
        let relative = file.strip_prefix(&crates).unwrap_or(&file).display().to_string();
        let text = fs::read_to_string(&file).unwrap();
        if EXEMPT.contains(&relative.as_str()) {
            continue;
        }
        let spawns =
            text.contains("CARGO_BIN_EXE_ultron") && (text.contains("env_clear()") || text.contains("env_clear: true"));
        // A `cli::Env` (the same struct) counts where the file runs a command or composes a session with it.
        let runs = text.contains("cli::Env {")
            && ["Runtime::new", "OpenOptions", "cli::run("]
                .iter()
                .any(|call| text.contains(call));
        if !spawns && !runs && !text.contains("RunEnv {") {
            continue;
        }
        for name in ["ULTRON_HINDSIGHT_URL", "ULTRON_MEMORANTADO_URL"] {
            let constant = name.trim_start_matches("ULTRON_").to_string() + "_ENV";
            if !text.contains(name) && !text.contains(&constant) {
                missing.push(format!(
                    "crates/{relative} builds a test environment without {name}=off"
                ));
            }
        }
    }
    missing.sort();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
