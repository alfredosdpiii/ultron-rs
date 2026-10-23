//! `/clear` in the interactive TUI (user requirement, as Claude Code's `/clear`; literal expectations), in a real
//! terminal (tmux on a private socket), against a scripted 127.0.0.1 Anthropic server with private HOME / agent /
//! server dirs:
//!
//! - plain `ultron` and `ultron durable`: `/clear` after a reply empties the chat, and the next prompt's request
//!   carries none of the earlier conversation; under `ultron durable` the new session stays on `durable.model`;
//! - `ultron-s` in its home session: `/clear` makes the new session home, a bare `ultron-s -p` afterwards continues
//!   it, and the old session's file stays;
//! - `ultron-s --session <id>` (not the home session): `/clear` leaves `home-session` alone.

mod tui_harness;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: &str = "anthropic/claude-sonnet-4-5";
/// The durable instance's sessions start on `durable.model`: pinned to the mock's model, so nothing reaches a real
/// `claude`.
const SETTINGS: &str = r#"{"sessionNaming":{"enabled":false},"durable":{"model":"anthropic/claude-sonnet-4-5"}}"#;

fn models(port: u16) -> String {
    format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#)
}

/// The harness's layout in `root` (agent dir pointing at `port`, the pinned settings), for runs before the TUI.
fn layout(root: &Path, port: u16) {
    for dir in ["agent", "cwd", "server"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("agent/models.json"), models(port)).unwrap();
    std::fs::write(root.join("agent/settings.json"), SETTINGS).unwrap();
}

/// `program <args>` in `root/cwd` with the harness's private layout; stdin null.
fn run(program: &Path, root: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(root.join("cwd"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("ULTRON_CODING_AGENT_DIR", root.join("agent"))
        .env("ULTRON_SERVER_DIR", root.join("server"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ultron() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_ultron"))
}

/// `ultron-s`: a link to the binary in the root's bin directory.
fn ultron_s(root: &Path) -> PathBuf {
    let link = root.join("bin/ultron-s");
    if !link.exists() {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(ultron(), &link).unwrap();
    }
    link
}

fn home_id(durable: &Path) -> String {
    std::fs::read_to_string(durable.join("home-session"))
        .unwrap()
        .trim()
        .to_string()
}

/// The session files (`*.jsonl`) under `root` whose name holds `id`.
fn session_files(root: &Path, id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && path.file_name().is_some_and(|n| n.to_string_lossy().contains(id))
            {
                found.push(path);
            }
        }
    }
    found
}

/// Type `/clear`, wait for its completion row, submit it, and wait until the new session is open. The completion
/// list can take the first Enter (it completes the name); the second submits.
fn clear(tui: &Tui) -> String {
    tui.send_text("/clear");
    let before = tui.wait("Clear the conversation: start a new session");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait_change(&before);
    if !screen.contains("New session started") {
        tui.send_keys(&["Enter"]);
    }
    tui.wait("New session started")
}

fn quit(tui: &Tui) {
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// One prompt and its reply after `/clear` in `tui`: the chat was empty, and the request holds the new prompt only.
fn clear_then_prompt(tui: &Tui, mock: &Mock) {
    tui.send_text("first question");
    tui.send_keys(&["Enter"]);
    tui.wait("First answer.");
    let screen = clear(tui);
    assert!(
        !screen.contains("first question") && !screen.contains("First answer."),
        "{screen}"
    );
    tui.send_text("second question");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Second answer.");
    assert!(!screen.contains("First answer."), "{screen}");
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert!(requests[0].contains("first question"), "{}", requests[0]);
    let last = &requests[1];
    assert!(last.contains("second question"), "{last}");
    assert!(
        !last.contains("first question") && !last.contains("First answer."),
        "{last}"
    );
}

#[test]
fn cli_clear_tui_empties_the_chat_and_the_next_request_has_no_earlier_messages() {
    let mock = Mock::start(vec![text_reply("First answer."), text_reply("Second answer.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 30, &["--model", MODEL], &[]);
    tui.wait("claude-sonnet-4-5");
    clear_then_prompt(&tui, &mock);
    quit(&tui);
}

#[test]
fn cli_clear_tui_durable_starts_a_new_server_session_on_the_durable_model() {
    let mock = Mock::start(vec![text_reply("First answer."), text_reply("Second answer.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let durable = tmp.0.join(".ultron/server");
    let durable_env = durable.display().to_string();
    let env = [("ULTRON_SERVER_DIR", durable_env.as_str())];
    // No `--model`: the session (and the one `/clear` starts) runs on `durable.model`.
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 30, &["durable"], &env);
    tui.wait("claude-sonnet-4-5");
    clear_then_prompt(&tui, &mock);
    let screen = tui.wait("claude-sonnet-4-5");
    assert!(!screen.contains("claude-opus-5-5"), "{screen}");
    quit(&tui);
    let out = run(ultron(), &tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(tui);
}

#[test]
fn cli_clear_tui_ultron_s_home_session_moves_home_to_the_new_session() {
    let mock = Mock::start(vec![text_reply("First answer."), text_reply("Second answer.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let durable = tmp.0.join(".ultron/server");
    let durable_env = durable.display().to_string();
    let env = [("ULTRON_SERVER_DIR", durable_env.as_str())];
    let link = ultron_s(&tmp.0);
    let out = run(&link, &tmp.0, &["-p", "first question"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let old = home_id(&durable);
    let tui = Tui::start_program(&tmp.0, &link, &models(mock.port), 120, 30, &[], &env);
    tui.wait("First answer.");
    let screen = clear(&tui);
    assert!(!screen.contains("First answer."), "{screen}");
    // The pointer moved before the TUI said so.
    let new = home_id(&durable);
    assert_ne!(new, old);
    quit(&tui);
    drop(tui);
    // A bare `ultron-s` continues the cleared session, not the old one.
    let out = run(&link, &tmp.0, &["-p", "second question"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Second answer.");
    assert_eq!(home_id(&durable), new);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert!(
        requests[1].contains("second question") && !requests[1].contains("first question"),
        "{}",
        requests[1]
    );
    let new_files = session_files(&tmp.0, &new);
    assert_eq!(new_files.len(), 1, "{new_files:?}");
    assert!(
        std::fs::read_to_string(&new_files[0])
            .unwrap()
            .contains("second question")
    );
    // The old session stays on disk, with its own conversation only.
    let old_files = session_files(&tmp.0, &old);
    assert_eq!(old_files.len(), 1, "{old_files:?}");
    let old_text = std::fs::read_to_string(&old_files[0]).unwrap();
    assert!(
        old_text.contains("first question") && !old_text.contains("second question"),
        "{old_text}"
    );
    let out = run(ultron(), &tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn cli_clear_tui_ultron_s_other_session_leaves_home_alone() {
    let mock = Mock::start(vec![text_reply("First answer."), text_reply("Second answer.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let durable = tmp.0.join(".ultron/server");
    let durable_env = durable.display().to_string();
    let env = [("ULTRON_SERVER_DIR", durable_env.as_str())];
    let link = ultron_s(&tmp.0);
    let out = run(&link, &tmp.0, &["-p", "first question"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let other = home_id(&durable);
    let out = run(&link, &tmp.0, &["--new", "-p", "second question"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let home = home_id(&durable);
    assert_ne!(home, other);
    let tui = Tui::start_program(&tmp.0, &link, &models(mock.port), 120, 30, &["--session", &other], &env);
    tui.wait("First answer.");
    clear(&tui);
    assert_eq!(home_id(&durable), home);
    quit(&tui);
    drop(tui);
    assert_eq!(home_id(&durable), home);
    let out = run(ultron(), &tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
