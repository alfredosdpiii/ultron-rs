//! A session's `write()` of a source file comes back with the errors its language server finds in the new text
//! (session_core::compose adds `lsp::LspGuard` to the host's file hooks), the write itself going through; with
//! `ULTRON_LSP=off` no server starts. The server is the scripted one of `crates/lsp/tests`.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;

const FAKE: &str = include_str!("../../lsp/tests/fake_server.py");
const CELL: &str = "await write('app.py', 'x = 1\\ny = ERROR\\n')\nprint('written')";

/// A directory to put first on the session's PATH: the scripted server as `pyright-langserver`, which leaves a
/// `started` file beside it, and a `mise` that fails, so the machine's own is never reached.
fn bin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake_server.py");
    std::fs::write(&script, FAKE).unwrap();
    let python = kernel::env::python(|name| std::env::var(name).ok());
    let python = sandbox::launcher::resolve_program(&python, &std::env::var("PATH").unwrap_or_default());
    let server = format!(
        "#!/bin/sh\ntouch '{}'\nexec '{}' -I '{}'\n",
        dir.path().join("started").display(),
        python.display(),
        script.display()
    );
    for (name, text) in [("pyright-langserver", server.as_str()), ("mise", "#!/bin/sh\nexit 1\n")] {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// The output of the cell of a session whose `ULTRON_LSP` is `lsp`, and whether the server was started.
fn written_with(lsp: &str) -> (String, bool) {
    let bin = bin();
    let a = actor(
        in_order(vec![tool("toolu_1", CELL), text("done")]),
        Setup {
            files: vec![("pyproject.toml", Vec::new())],
            vars: vec![
                ("PATH", format!("{}:/usr/local/bin:/usr/bin:/bin", bin.path().display())),
                (lsp::LSP_ENV, lsp.into()),
            ],
            ..Setup::default()
        },
    );
    let ended = a.prompt("write the file");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    assert_eq!(
        std::fs::read_to_string(a.cwd.join("app.py")).unwrap(),
        "x = 1\ny = ERROR\n"
    );
    let results = a.requests().last().map(p_session_support::tool_results).unwrap();
    (
        results[0].as_str().unwrap().to_string(),
        bin.path().join("started").exists(),
    )
}

#[test]
fn session_core_write_shows_the_language_servers_errors_and_still_writes() {
    let (output, started) = written_with("on");
    assert!(
        output.contains("[LSP] pyright: 1 error in app.py with this write:\n  2:5 E1 ERROR found on line 2\n"),
        "{output}"
    );
    assert!(output.contains("written"), "{output}");
    assert!(started);
}

#[test]
fn session_core_write_starts_no_language_server_when_lsp_is_off() {
    let (output, started) = written_with("off");
    assert!(!output.contains("[LSP]") && output.contains("written"), "{output}");
    assert!(!started);
}
