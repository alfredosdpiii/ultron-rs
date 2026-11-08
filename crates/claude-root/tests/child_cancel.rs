//! child.ts `kill`: a cancelled Claude Code subagent gets SIGTERM on its process group and the run ends with the
//! cancel's reason as soon as the process exits (SIGKILL only if it is still running five seconds later).

use claude_root::child::{ChildOptions, ChildRun, ChildServer, run_claude_child};
use claude_root::self_cmd::SelfCommand;
use std::time::{Duration, Instant};

#[test]
fn claude_root_cancelled_child_returns_once_claude_exits_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    // Reads its prompt, writes one stream line, then waits as `claude -p` would for a model reply (SIGTERM ends it).
    std::fs::write(
        &claude,
        "#!/bin/sh\ncat >/dev/null\necho '{\"type\":\"system\"}'\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let tmp = dir.path().display().to_string();
    let var = move |name: &str| (name == "TMPDIR").then(|| tmp.clone());
    let self_cmd = SelfCommand {
        command: "/bin/false".into(),
        args: Vec::new(),
    };
    let cwd = dir.path().display().to_string();
    let options = ChildOptions {
        claude: &claude.display().to_string(),
        server: ChildServer {
            self_cmd: &self_cmd,
            parent_socket: "/nonexistent.sock",
            parent_name: "root",
            frame_model: None,
            no_context_files: true,
            no_skills: true,
        },
        cwd: &cwd,
        model: "sonnet",
        system_prompt: "system".into(),
        skip_permissions: Vec::new(),
        var: &var,
    };
    let run = ChildRun {
        lane_name: "child".into(),
        prompt: "hello".into(),
        level: 1,
        allowance: 0,
        model: None,
        cwd: None,
        env: Vec::new(),
    };
    // Its first stream line arrives while it runs: the run is cancelled from there.
    let signal = kernel::Signal::new();
    let aborted_at = std::cell::Cell::new(None);
    let progress = |_: &claude_root::child::StreamStats| {
        if aborted_at.get().is_none() {
            aborted_at.set(Some(Instant::now()));
            signal.abort("stopped by the test");
        }
    };
    let result = run_claude_child(&options, &run, "token", &signal, &progress);
    let returned = Instant::now();
    assert_eq!(result.unwrap_err(), "stopped by the test");
    let waited = returned - aborted_at.get().expect("aborted");
    assert!(waited < Duration::from_secs(4), "returned {waited:?} after the cancel");
}
