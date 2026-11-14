//! Language server diagnostics on REPL writes, against a scripted server (`fake_server.py`): what a write of a file
//! with errors, a clean one and an unwatched one says, through the host's before-write check; how a missing or
//! broken server is found through mise; and what a server that is slow, loading or dead costs the write.

use host::Guard;
use host::file_hooks::FileHooks;
use lsp::{LspGuard, Report};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FAKE: &str = include_str!("fake_server.py");
/// Logs where it runs, whether it was told to install or to answer yes, and its tools, then runs the command after
/// `--` with the tools it "installed" on PATH.
const MISE: &str = "#!/bin/sh\necho \"cwd=$(pwd) auto_install=$MISE_EXEC_AUTO_INSTALL yes=$MISE_YES $*\" >> \"$FAKE_MISE_LOG\"\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nPATH=\"$FAKE_MISE_TOOLS:$PATH\" exec \"$@\"\n";

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    tools: PathBuf,
}

/// Runs the scripted server at `script`. Every path is absolute: the session PATH of these tests holds nothing of
/// the machine's, so no test can reach a real language server or a real mise (which would install one).
fn launcher(script: &Path) -> String {
    let python = kernel::env::python(|name| std::env::var(name).ok());
    let python = sandbox::launcher::resolve_program(&python, &std::env::var("PATH").unwrap_or_default());
    format!(
        "#!/bin/sh\nexec '{}' -I '{}' \"$@\"\n",
        python.display(),
        script.display()
    )
}

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Project {
    /// A Git repository with a Python project in it, and an empty directory first on its PATH.
    fn new() -> Project {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let (bin, tools) = (base.join("bin"), base.join("tools"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(base.join("fake_server.py"), FAKE).unwrap();
        Project {
            _tmp: tmp,
            root,
            bin,
            tools,
        }
    }

    fn server(&self) -> String {
        launcher(&self.bin.with_file_name("fake_server.py"))
    }

    fn with_server(self) -> Project {
        for command in ["pyright-langserver", "rust-analyzer"] {
            executable(&self.bin.join(command), &self.server());
        }
        self
    }

    fn with_mise(self) -> Project {
        executable(&self.bin.join("mise"), MISE);
        executable(&self.tools.join("pyright-langserver"), &self.server());
        self
    }

    fn mise_log(&self) -> String {
        std::fs::read_to_string(self.tools.join("mise.log")).unwrap_or_default()
    }

    /// The session environment: nothing of this process's, so no real server or mise is ever reached.
    fn guard(&self, mode: &str) -> LspGuard {
        LspGuard::new(vec![
            ("PATH".into(), self.bin.display().to_string()),
            ("HOME".into(), self.root.display().to_string()),
            ("FAKE_LSP".into(), mode.into()),
            (
                "FAKE_MISE_LOG".into(),
                self.tools.join("mise.log").display().to_string(),
            ),
            ("FAKE_MISE_TOOLS".into(), self.tools.display().to_string()),
        ])
    }

    fn hooks(&self, mode: &str) -> FileHooks {
        let mut hooks = FileHooks::new(&self.root);
        hooks.add(Box::new(self.guard(mode)));
        hooks
    }
}

fn notes(hooks: &FileHooks, file: &str, content: &str) -> Vec<String> {
    let verdict = hooks.before_write("main", file, content);
    assert_eq!(verdict["blocked"], false, "{verdict}");
    verdict["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note.as_str().unwrap().to_string())
        .collect()
}

const ONE_ERROR: &str = "[LSP] pyright: 1 error in src/app.py with this write:\n  2:5 E1 ERROR found on line 2";

#[test]
fn lsp_write_reports_the_errors_of_the_proposed_text_and_nothing_for_a_clean_one() {
    for mode in ["push", "unversioned", "pull"] {
        let project = Project::new().with_server();
        let hooks = project.hooks(mode);
        // The file is not on disk: the errors are those of the text about to be written. Warnings are not shown.
        assert_eq!(
            notes(&hooks, "src/app.py", "x = 1\ny = ERROR\nz = WARN\n"),
            [ONE_ERROR],
            "{mode}"
        );
        assert_eq!(
            notes(&hooks, "src/app.py", "x = 1\ny = ERROR\n# same\n"),
            [ONE_ERROR],
            "{mode}"
        );
        assert_eq!(notes(&hooks, "src/app.py", "x = 1\n"), [] as [&str; 0], "{mode}");
        // A clean file that stays clean: a server that then says nothing is not waited on for the whole budget.
        let started = Instant::now();
        assert_eq!(notes(&hooks, "src/app.py", "x = 2\n"), [] as [&str; 0], "{mode}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{mode}: {:?}",
            started.elapsed()
        );
        assert_eq!(
            notes(&hooks, "src/app.py", "ERROR\n"),
            ["[LSP] pyright: 1 error in src/app.py with this write:\n  1:1 E1 ERROR found on line 1"],
            "{mode}"
        );
        assert_eq!(project.mise_log(), "", "a server on PATH needs no mise");
    }
}

#[test]
fn lsp_write_lists_at_most_ten_errors() {
    let project = Project::new().with_server();
    let note = notes(&project.hooks("push"), "many.py", &"ERROR\n".repeat(12)).remove(0);
    let lines: Vec<&str> = note.lines().collect();
    assert_eq!(lines[0], "[LSP] pyright: 12 errors in many.py with this write:");
    assert_eq!(lines.len(), 12, "{note}");
    assert_eq!(lines[11], "  … and 2 more");
}

#[test]
fn lsp_files_without_a_server_or_a_project_are_not_checked() {
    let project = Project::new().with_server();
    let guard = project.guard("push");
    // No server for the extension.
    assert_eq!(
        guard.diagnose(&project.root.join("notes.txt"), "ERROR", lsp::BUDGET),
        Report::Unwatched
    );
    // A Rust file with no Cargo.toml above it: rust-analyzer would report nothing true about it.
    assert_eq!(
        guard.diagnose(&project.root.join("loose.rs"), "ERROR", lsp::BUDGET),
        Report::Unwatched
    );
    // A Python file outside any repository or project.
    let scratch = project.bin.join("scratch.py");
    assert_eq!(guard.diagnose(&scratch, "ERROR", lsp::BUDGET), Report::Unwatched);
    assert_eq!(notes(&project.hooks("push"), "notes.txt", "ERROR"), [] as [&str; 0]);
}

#[test]
fn lsp_project_root_is_the_outermost_marker_inside_the_repository() {
    let project = Project::new();
    let member = project.root.join("crates/a");
    std::fs::create_dir_all(member.join("src")).unwrap();
    std::fs::write(member.join("Cargo.toml"), "").unwrap();
    let rust = &lsp::servers::SERVERS[0];
    assert_eq!(rust.name, "rust-analyzer");
    let file = member.join("src/lib.rs");
    assert_eq!(lsp::servers::root_of(rust, &file), Some(member.clone()));
    // With a workspace manifest at the top, one server serves every member.
    std::fs::write(project.root.join("Cargo.toml"), "").unwrap();
    assert_eq!(lsp::servers::root_of(rust, &file), Some(project.root.clone()));
    // A marker above the repository is not this project's.
    std::fs::write(project.root.parent().unwrap().join("Cargo.toml"), "").unwrap();
    assert_eq!(lsp::servers::root_of(rust, &file), Some(project.root.clone()));
}

#[test]
fn lsp_server_missing_from_path_is_run_through_mise_which_installs_it() {
    let project = Project::new().with_mise();
    let hooks = project.hooks("push");
    assert_eq!(notes(&hooks, "src/app.py", "x = 1\ny = ERROR\n"), [ONE_ERROR]);
    assert_eq!(notes(&hooks, "src/app.py", "x = 1\n"), [] as [&str; 0]);
    // Once per server, not per write.
    let log = project.mise_log();
    assert_eq!(log.lines().count(), 1, "{log}");
    // mise runs outside the project (inside it, `mise exec` trusts the project's mise.toml and runs what it
    // holds) and is never told to answer yes; the server it starts enters the project.
    assert!(
        log.starts_with("cwd=/ auto_install=1 yes= exec npm:pyright -- /bin/sh -c "),
        "{log}"
    );
    assert!(
        log.ends_with(&format!(" sh {} pyright-langserver --stdio\n", project.root.display())),
        "{log}"
    );
}

#[test]
fn lsp_typescript_project_naming_the_effect_plugin_gets_the_server_that_can_load_it() {
    let project = Project::new();
    for command in ["typescript-language-server", "effect-tsgo"] {
        executable(&project.bin.join(command), &project.server());
    }
    let server_of = |project: &Project| {
        let notes = notes(&project.hooks("push"), "src/a.ts", "ERROR\n");
        notes[0].split(':').next().unwrap().to_string()
    };
    let tsconfig = project.root.join("tsconfig.json");
    std::fs::write(&tsconfig, "{}").unwrap();
    assert_eq!(server_of(&project), "[LSP] typescript-language-server");
    // TypeScript 7 loads no plugins: the Effect build of it reports the plugin's errors.
    let plugin = r#"{ "compilerOptions": { "plugins": [{ "name": "@effect/language-service" }] } } // comment"#;
    std::fs::write(&tsconfig, plugin).unwrap();
    assert_eq!(server_of(&project), "[LSP] effect-tsgo");
    // The project's own TypeScript 6 does load it, in tsserver.
    let typescript = project.root.join("node_modules/typescript");
    std::fs::create_dir_all(&typescript).unwrap();
    std::fs::write(typescript.join("package.json"), r#"{"version":"6.0.3"}"#).unwrap();
    assert_eq!(server_of(&project), "[LSP] typescript-language-server");
    std::fs::write(typescript.join("package.json"), r#"{"version":"7.0.2"}"#).unwrap();
    assert_eq!(server_of(&project), "[LSP] effect-tsgo");
}

#[test]
fn lsp_rust_errors_that_need_macro_expansion_to_be_true_are_not_shown() {
    let error = |code: &str, message: &str| lsp::client::Diagnostic {
        line: 0,
        column: 0,
        code: Some(code.into()),
        message: message.into(),
    };
    let codes = |spec: &lsp::servers::Spec, errors: Vec<lsp::client::Diagnostic>| -> Vec<String> {
        let kept = lsp::servers::reliable(spec, errors);
        kept.into_iter().map(|error| error.code.unwrap()).collect()
    };
    let rust = &lsp::servers::SERVERS[0];
    let seen = || {
        vec![
            error("E0308", "expected u8, found &'static str"),
            error("E0282", "type annotations needed"),
            error("syntax-error", "Syntax Error in Expansion: expected pattern"),
            error("syntax-error", "Syntax Error: expected expression"),
            error("E0425", "no such value in this scope"),
        ]
    };
    assert_eq!(codes(rust, seen()), ["E0308", "syntax-error", "E0425"]);
    // A name the file does not define may come from the macro that could not be expanded.
    let mut unexpanded = seen();
    unexpanded.push(error(
        "macro-error",
        "`OUT_DIR` not set, build scripts may have failed to run",
    ));
    assert_eq!(codes(rust, unexpanded), ["E0308", "syntax-error"]);
    // Other servers' errors are theirs to judge.
    assert_eq!(codes(&lsp::servers::SERVERS[1], seen()).len(), 5);
}

#[test]
fn lsp_server_on_path_that_does_not_start_falls_back_to_mise() {
    // A shim without its tool, as mise and rustup leave on PATH.
    let project = Project::new().with_mise();
    executable(
        &project.bin.join("pyright-langserver"),
        "#!/bin/sh\necho 'No version is set for shim' >&2\nexit 1\n",
    );
    assert_eq!(
        notes(&project.hooks("push"), "src/app.py", "x = 1\ny = ERROR\n"),
        [ONE_ERROR]
    );
    assert_eq!(project.mise_log().lines().count(), 1);
}

#[test]
fn lsp_server_that_cannot_be_started_says_so_once_and_never_holds_a_write() {
    let project = Project::new();
    executable(
        &project.bin.join("pyright-langserver"),
        "#!/bin/sh\necho 'No version is set for shim' >&2\nexit 1\n",
    );
    let hooks = project.hooks("push");
    let first = notes(&hooks, "src/app.py", "ERROR\n");
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].starts_with("[LSP] no language server errors for files like src/app.py: pyright did not start (`")
            && first[0]
                .ends_with("pyright-langserver --stdio`: it exited while starting: No version is set for shim)."),
        "{first:?}"
    );
    let started = Instant::now();
    assert_eq!(notes(&hooks, "src/app.py", "ERROR\n"), [] as [&str; 0]);
    assert_eq!(notes(&hooks, "other.py", "ERROR\n"), [] as [&str; 0]);
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());

    // Neither the server nor mise.
    let bare = Project::new();
    assert_eq!(
        notes(&bare.hooks("push"), "src/app.py", "ERROR\n"),
        [
            "[LSP] no language server errors for files like src/app.py: pyright did not start (`pyright-langserver` is not on PATH, and neither is mise to install it)."
        ]
    );
}

#[test]
fn lsp_server_still_loading_its_project_is_announced_once_and_not_waited_for_again() {
    // rust-analyzer is the server that says when its project is loaded.
    let project = Project::new().with_server();
    std::fs::write(project.root.join("Cargo.toml"), "").unwrap();
    let hooks = project.hooks("loading");
    let started = Instant::now();
    assert_eq!(
        notes(&hooks, "src/lib.rs", "ERROR\n"),
        ["[LSP] rust-analyzer is starting for .: errors it finds are shown with later writes."]
    );
    assert!(started.elapsed() >= lsp::BUDGET, "the first write gives it the budget");
    let started = Instant::now();
    assert_eq!(notes(&hooks, "src/lib.rs", "ERROR\n"), [] as [&str; 0]);
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

#[test]
fn lsp_server_that_does_not_answer_or_dies_leaves_the_write_unchecked() {
    let project = Project::new().with_server();
    let file = project.root.join("src/app.py");
    let slow = project.guard("slow");
    assert_eq!(
        slow.diagnose(&file, "ERROR\n", Duration::from_millis(600)),
        Report::Unanswered { server: "pyright" }
    );
    // A server that exits is started again once, then given up on.
    let dying = project.guard("die");
    let budget = Duration::from_secs(4);
    assert_eq!(
        dying.diagnose(&file, "ERROR\n", budget),
        Report::Unanswered { server: "pyright" }
    );
    assert_eq!(
        dying.diagnose(&file, "ERROR\n", budget),
        Report::Failed {
            server: "pyright",
            reason: "it exited twice".into()
        }
    );
    assert_eq!(
        dying.diagnose(&file, "ERROR\n", budget),
        Report::Failed {
            server: "pyright",
            reason: "it exited twice".into()
        }
    );
}

#[test]
fn lsp_is_on_unless_the_environment_or_the_setting_turns_it_off() {
    let none = json!({});
    assert!(lsp::enabled(None, &none));
    assert!(lsp::enabled(Some(""), &none));
    assert!(lsp::enabled(Some("on"), &none));
    for off in ["off", "0", "false", "no", "none", " OFF "] {
        assert!(!lsp::enabled(Some(off), &none), "{off:?}");
    }
    let disabled: Value = json!({ "lsp": { "enabled": false } });
    assert!(!lsp::enabled(None, &disabled));
    // The environment wins.
    assert!(lsp::enabled(Some("on"), &disabled));
    assert!(lsp::enabled(None, &json!({ "lsp": { "enabled": true } })));
    // Servers would run outside the sandbox: off, whatever the environment says.
    for mode in ["auto", "required"] {
        assert!(
            !lsp::enabled(Some("on"), &json!({ "sandbox": { "mode": mode } })),
            "{mode}"
        );
    }
    assert!(lsp::enabled(None, &json!({ "sandbox": { "mode": "off" } })));
}
