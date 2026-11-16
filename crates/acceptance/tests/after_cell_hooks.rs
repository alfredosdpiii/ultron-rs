//! After-cell file hooks through the `ultron` binary (session-worker.ts `cellStarted`/`cellEnded`/`takePending` with
//! loki.ts `afterCellChanges`): in a repository with a committed Loki policy, a cell that writes a secret by other
//! means than `write()`/`edit()` (here `Path.write_text`) returns as usual, and Loki's finding arrives with the
//! lane's next cell result. Ported from ultron-loki.test.ts ("bash writes are caught after"). Under a turn cap the
//! finding sits between the cell's text and the budget line (session-worker.ts `[withFindings(result), hint, line]`).

use crate::common;

use common::{Install, Provider, Reply, rpc_tool_results};
use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// The install's project as a repository with Loki's default policy and `file` committed.
fn committed(install: &Install, file: &str, text: &str) {
    let project = install.project();
    let home = install.root.path();
    git(&project, home, &["init", "-q", "-b", "main"]);
    git(&project, home, &["config", "user.email", "t@example.com"]);
    git(&project, home, &["config", "user.name", "T"]);
    // `loki init --minimal`: the engine and the default policy in .loki/.
    let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/loki-engine");
    std::fs::create_dir_all(project.join(".loki")).unwrap();
    std::fs::copy(engine.join("loki.py"), project.join(".loki/loki.py")).unwrap();
    std::fs::copy(engine.join("templates/loki.json"), project.join(".loki/loki.json")).unwrap();
    std::fs::write(project.join(file), text).unwrap();
    git(&project, home, &["add", "-A"]);
    git(&project, home, &["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
}

/// The cell result that carries Loki's note after a cell added two functions to a committed `store.lua`, in a
/// session whose hygiene model is a stand-in flagging line 2 as a swallowed error and line 3 as a dense line.
fn hygiene_note(level: Option<&str>) -> String {
    let provider = Provider::start(move |request| {
        let results = request.tool_results();
        if results.is_empty() {
            let text = "LIMIT = 10\\nfunction load() end\\nfunction save() end\\n";
            return Reply::Rlm(format!(
                "from pathlib import Path\nPath('store.lua').write_text('{text}')\nprint('written')"
            ));
        }
        if results.iter().any(|r| r.contains("[Loki]")) || results.len() > 500 {
            return Reply::Text("done".into());
        }
        Reply::Rlm(format!("print('next cell', {})", results.len()))
    });
    let install = Install::new(&provider);
    committed(&install, "store.lua", "LIMIT = 10\n");
    // Stands in for a Python with NumPy: it answers Loki's hygiene request for one file.
    let model = install.root.path().join("hygiene-python");
    let reply = r#"[[[2, "swallowed_error", "Ignored error"], [3, "dense_line", "Dense line"]]]"#;
    std::fs::write(&model, format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{reply}'\n")).unwrap();
    std::fs::set_permissions(&model, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let model = model.display().to_string();
    let mut env = vec![
        (delegation::usage::MAX_TOTAL_TURNS_ENV, "1000"),
        ("ULTRON_LOKI_PYTHON", model.as_str()),
    ];
    env.extend(level.map(|level| ("ULTRON_LOKI_HYGIENE", level)));
    let mut client = install.rpc(&["--no-session"], &env);
    client.prompt_and_wait("Add the store", common::HANG).unwrap();
    let results = rpc_tool_results(&client.get_messages().unwrap());
    let told: Vec<&String> = results.iter().filter(|r| r.contains("[Loki]")).collect();
    assert_eq!(told.len(), 1, "the note is delivered once: {results:?}");
    told[0].clone()
}

/// The session's Loki guard passes on hygiene notes for the lines a cell added: the default kinds as an FYI, every
/// kind under `ULTRON_LOKI_HYGIENE=all`.
#[test]
fn acceptance_after_cell_hygiene_notes_arrive_as_advisories() {
    let swallowed = "store.lua:2: loki/hygiene-swallowed-error: Ignored error: `function load() end`";
    let dense = "store.lua:3: loki/hygiene-dense-line: Dense line: `function save() end`";
    let note = hygiene_note(None);
    assert!(
        note.contains("FYI (advisory, not blocking; no change required):"),
        "{note}"
    );
    assert!(note.contains(swallowed), "{note}");
    assert!(!note.contains("fix these"), "{note}");
    assert!(!note.contains("hygiene-dense-line"), "{note}");
    let all = hygiene_note(Some("all"));
    assert!(all.contains(swallowed) && all.contains(dense), "{all}");
}

#[test]
fn acceptance_after_cell_loki_finding_arrives_with_the_next_cell() {
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0".repeat(2));
    let write = format!(
        "from pathlib import Path\nPath('settings.py').write_text('DEBUG = False\\nAPI_KEY = \"{key}\"\\n')\nprint('written')"
    );
    // The check runs in the background after the first cell; later cells run until one carries its finding (a
    // hang guard of 500 cells, never a timing expectation).
    let provider = Provider::start(move |request| {
        let results = request.tool_results();
        if results.is_empty() {
            return Reply::Rlm(write.clone());
        }
        if results.iter().any(|r| r.contains("[Loki]")) || results.len() > 500 {
            return Reply::Text("done".into());
        }
        // Each cell differs: the same cell seven times in a row is refused (the turn guards).
        Reply::Rlm(format!("print('next cell', {})", results.len()))
    });
    let install = Install::new(&provider);
    committed(&install, "settings.py", "DEBUG = False\n");
    let mut client = install.rpc(&["--no-session"], &[(delegation::usage::MAX_TOTAL_TURNS_ENV, "1000")]);
    client.prompt_and_wait("Change the settings", common::HANG).unwrap();
    let results = rpc_tool_results(&client.get_messages().unwrap());
    assert!(results[0].contains("written"));
    assert!(
        !results[0].contains("[Loki]"),
        "the writing cell itself returns as usual"
    );
    let told: Vec<&String> = results.iter().filter(|r| r.contains("[Loki]")).collect();
    assert_eq!(told.len(), 1, "the finding is delivered once: {results:?}");
    let second = told[0];
    assert!(second.contains("next cell"), "{second}");
    assert!(
        second.contains("[Loki] 1 new finding from this cell's changes; fix these:"),
        "{second}"
    );
    assert!(second.contains("loki/secret"), "{second}");
    assert!(!second.contains(&key));
    let at = |needle: &str| second.find(needle).unwrap_or_else(|| panic!("{needle} in {second}"));
    assert!(at("next cell") < at("[Loki]"), "{second}");
    assert!(at("[Loki]") < at("[budget left for this request:"), "{second}");
    assert!(second.trim_end().ends_with("model turns]"), "{second}");
}
