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
