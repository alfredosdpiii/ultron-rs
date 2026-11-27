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
        // And language servers off: a session with them on starts (and through mise installs) real servers.
        for name in ["ULTRON_HINDSIGHT_URL", "ULTRON_MEMORANTADO_URL", "ULTRON_LSP"] {
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

/// Every workspace crate must inherit `[workspace.lints]` so `unsafe_code = "forbid"` and workspace clippy lints
/// cannot be silently dropped when a new crate is added.
#[test]
fn every_workspace_crate_inherits_workspace_lints() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut missing = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        let dir = entry.unwrap().path();
        let Ok(manifest) = fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let mut in_lints = false;
        let mut inherited = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_lints = trimmed == "[lints]";
            } else if in_lints && trimmed == "workspace = true" {
                inherited = true;
                break;
            }
        }
        if !inherited {
            let krate = dir.file_name().unwrap().to_string_lossy();
            missing.push(format!("crates/{krate}/Cargo.toml is missing [lints] workspace = true"));
        }
    }
    missing.sort();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// `loki-guardrails` must be pinned once in root `[workspace.dependencies]` with an exact `=` version, and every
/// workspace crate depending on `loki` must inherit it via `loki.workspace = true`.
#[test]
fn every_loki_guardrails_pin_is_exact_and_consistent() {
    let root_manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let root_text = fs::read_to_string(&root_manifest).unwrap();
    let pinned_in_root = root_text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains("loki-guardrails") && trimmed.contains("version = \"=")
    });
    assert!(
        pinned_in_root,
        "Cargo.toml must pin loki-guardrails with an exact `=` version in [workspace.dependencies]"
    );

    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut users = 0usize;
    let mut violations = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        let dir = entry.unwrap().path();
        let Ok(manifest) = fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let krate = dir.file_name().unwrap().to_string_lossy().into_owned();
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed == "loki.workspace = true"
                || (trimmed.starts_with("loki =") && trimmed.contains("workspace = true"))
            {
                users += 1;
            } else if trimmed.contains("loki-guardrails") {
                violations.push(format!(
                    "crates/{krate}/Cargo.toml must use `loki.workspace = true` instead of duplicating loki-guardrails"
                ));
            }
        }
    }
    assert!(users > 0, "expected at least one crate using loki.workspace = true");
    violations.sort();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

/// Every `crates/*/src` file must stay free of non-exempt inline comments (doc comments `///`/`//!`,
/// `// ponytail:`, `// SAFETY:`, and license headers remain allowed). Run `scripts/strip-comments.py --apply`.
#[test]
fn no_non_exempt_inline_comments_in_src() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = repo.join("scripts/strip-comments.py");
    let crates = repo.join("crates");
    let mut src_dirs: Vec<std::path::PathBuf> = fs::read_dir(&crates)
        .unwrap()
        .filter_map(|e| {
            let src = e.ok()?.path().join("src");
            src.is_dir().then_some(src)
        })
        .collect();
    src_dirs.sort();
    let output = std::process::Command::new("python3")
        .arg(&script)
        .arg("--check")
        .args(&src_dirs)
        .output()
        .expect("python3 must run scripts/strip-comments.py");
    assert!(
        output.status.success(),
        "non-exempt inline comments found in crates/*/src (run `scripts/strip-comments.py --apply crates/*/src`):\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Production code in `crates/*/src` (everything before `#[cfg(test)]`) must not call `.unwrap()`.
#[test]
fn no_unwrap_in_prod_src() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut files);
    }
    let mut hits = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let prod = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        let rel = file.strip_prefix(&crates).unwrap_or(&file).display();
        for (idx, line) in prod.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            if line.contains(".unwrap()") {
                hits.push(format!("crates/{rel}:{}: {}", idx + 1, trimmed));
            }
        }
    }
    hits.sort();
    assert!(
        hits.is_empty(),
        "production .unwrap() calls found:\n{}",
        hits.join("\n")
    );
}

/// Production code in `crates/*/src` must use `jsrt::sync::lock` instead of `.lock().expect(...)` /
/// `.read().expect(...)` / `.write().expect(...)`, so a panic in one thread never cascades into mutex-poison panics.
#[test]
fn no_lock_expect_in_prod_src() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut files);
    }
    let mut hits = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let prod = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        let rel = file.strip_prefix(&crates).unwrap_or(&file).display();
        let lines: Vec<&str> = prod.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            let joined = if idx + 1 < lines.len() {
                format!("{trimmed}{}", lines[idx + 1].trim_start())
            } else {
                trimmed.to_string()
            };
            let matches_lock_expect = |text: &str| {
                text.contains(".lock().expect(")
                    || text.contains(".read().expect(")
                    || text.contains(".write().expect(")
            };
            if matches_lock_expect(line) || (matches_lock_expect(&joined) && !matches_lock_expect(lines[idx + 1])) {
                hits.push(format!("crates/{rel}:{}: {}", idx + 1, trimmed));
            }
        }
    }
    hits.sort();
    assert!(
        hits.is_empty(),
        "production lock/rwlock .expect() calls found (use jsrt::sync::lock):\n{}",
        hits.join("\n")
    );
}

/// `crates/session-core/src` reads environment variables through injected `RunEnv` rather than ambient `std::env::var`,
/// keeping session tests isolated. Only the process entrypoints and crash-injection hook are exempt.
#[test]
fn no_direct_env_var_in_session_core() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../session-core/src");
    let mut files = Vec::new();
    rust_files(&dir, &mut files);
    const EXEMPT: [&str; 3] = ["lane.rs", "lib.rs", "turns.rs"];
    let mut hits = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&dir).unwrap_or(&file).display().to_string();
        if EXEMPT.contains(&rel.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        let prod = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        for (idx, line) in prod.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            if line.contains("std::env::var(") || line.contains("std::env::var_os(") {
                hits.push(format!("crates/session-core/src/{rel}:{}: {}", idx + 1, trimmed));
            }
        }
    }
    hits.sort();
    assert!(
        hits.is_empty(),
        "ambient std::env::var calls in session-core (use RunEnv):\n{}",
        hits.join("\n")
    );
}

/// Atomic file writes in `crates/*/src` must go through `jsrt::fs::atomic_write` / `jsrt::fs::atomic_write_mode`
/// instead of hand-rolling `.tmp` + `std::fs::rename`.
#[test]
fn no_hand_rolled_tmp_rename_in_prod_src() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut files);
    }
    const EXEMPT: [&str; 5] = [
        "cli/src/assets.rs",
        "config/src/paths.rs",
        "diagram/src/lib.rs",
        "jsrt/src/fs.rs",
        "review/src/checkout.rs",
    ];
    let mut hits = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&crates).unwrap_or(&file).display().to_string();
        if EXEMPT.contains(&rel.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        let prod = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        if prod.contains("std::fs::rename(") && (prod.contains(".tmp") || prod.contains("tmp-")) {
            hits.push(format!(
                "crates/{rel} hand-rolls tmp+rename (use jsrt::fs::atomic_write)"
            ));
        }
    }
    hits.sort();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// Settings keys already owned by `config::keys` must be read through its typed accessors rather than raw
/// `.get("<key>")` lookups in `crates/*/src` outside `crates/config/src`.
#[test]
fn no_raw_settings_get_for_owned_keys() {
    const OWNED: [&str; 7] = [
        "engineering",
        "alwaysSkills",
        "enabledModels",
        "defaultThinkingLevel",
        "modelThinkingLevels",
        "shellPath",
        "shellCommandPrefix",
    ];
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut files);
    }
    let mut hits = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&crates).unwrap_or(&file).display().to_string();
        if rel.starts_with("config/src/") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        let prod = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        for (idx, line) in prod.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            for key in OWNED {
                let needle = format!(".get(\"{key}\")");
                if line.contains(&needle) {
                    hits.push(format!("crates/{rel}:{}: raw {needle} (use config::keys)", idx + 1));
                }
            }
        }
    }
    hits.sort();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// Ratchet on source file size in `crates/*/src`: no new file may exceed 1,200 lines, and grandfathered files may not
/// grow past their recorded ceiling (split into submodules when adding features).
#[test]
fn no_new_god_files() {
    const MAX_LINES: usize = 1_200;
    const GRANDFATHERED: [(&str, usize); 28] = [
        ("ai/src/anthropic.rs", 1247),
        ("ai/src/antigravity/mod.rs", 1358),
        ("ai/src/aws_credentials.rs", 1410),
        ("ai/src/bedrock.rs", 1607),
        ("ai/src/claude_code_cli.rs", 1634),
        ("ai/src/openai_completions.rs", 1481),
        ("ai/src/openai_responses.rs", 1281),
        ("claude-root/src/mcp_server.rs", 1598),
        ("cli/src/setup_wizard.rs", 1759),
        ("client/src/runtime.rs", 1306),
        ("delegation/src/usage.rs", 1397),
        ("delegation/src/worktrees.rs", 1739),
        ("durable/src/transaction.rs", 1312),
        ("harness/src/scheduler/mod.rs", 2014),
        ("interactive/src/agent_hub.rs", 1240),
        ("interactive/src/components.rs", 1342),
        ("interactive/src/tree_selector.rs", 1344),
        ("kernel-apis/src/memory.rs", 1705),
        ("rlm-views/src/graph.rs", 2022),
        ("rlm-views/src/pane.rs", 1665),
        ("rlm-views/src/visualizer.rs", 1226),
        ("session-core/src/lane.rs", 1487),
        ("tui/src/editor.rs", 1494),
        ("tui/src/fullscreen.rs", 1381),
        ("tui/src/latex.rs", 1850),
        ("tui/src/marked.rs", 1986),
        ("tui/src/tables.rs", 1839),
        ("tui/src/vim.rs", 1400),
    ];
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut files);
    }
    let mut violations = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&crates).unwrap_or(&file).display().to_string();
        let count = fs::read_to_string(&file).unwrap().lines().count();
        let limit = GRANDFATHERED
            .iter()
            .find(|(path, _)| *path == rel)
            .map_or(MAX_LINES, |(_, cap)| *cap);
        if count > limit {
            violations.push(format!(
                "crates/{rel} has {count} lines (limit {limit}; split into submodules)"
            ));
        }
    }
    violations.sort();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
