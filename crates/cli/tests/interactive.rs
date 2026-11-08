//! The interactive TUI in a real terminal (tmux). Parity tests replay `fixtures/interactive.json`, recorded from the
//! TypeScript client TUI (`node packages/coding-agent/src/cli.ts`, which opens experimental/client-tui.ts) by
//! `fixtures/interactive.mjs`: the Rust binary starts in tmux against the same scripted Anthropic SSE server with
//! private HOME / ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR temp dirs, gets the same keystrokes, and every stable
//! screen must equal the TypeScript one after the same normalization (temp paths, ids, spinner frames).
//! Rust-only tests cover the user requirements that differ from TypeScript (session names, the resume picker).

use crate::recorded_version;
use crate::tui_harness::{Mock, TempDir, Tui, replies, text_reply};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/interactive.json")).unwrap())
}

/// Replay one recorded scenario and compare every screen.
fn replay(name: &str) {
    let scenario = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("scenario {name} is not in the fixture"));
    let mock = Mock::start(replies(scenario), None);
    // The TypeScript client TUI names nothing and has none of the Rust-only additions.
    let mut env = vec![
        (session_core::core::AUTO_NAME_ENV, "off"),
        (interactive::qol::PARITY_ENV, "1"),
    ];
    if scenario["splash"] != true {
        env.push((cli::interactive::SPLASH_ENV, "off"));
    }
    let tui = Tui::start(
        mock.port,
        scenario["cols"].as_u64().unwrap(),
        scenario["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &env,
    );
    let captures = scenario["captures"].as_array().unwrap();
    for (i, (step, want)) in scenario["steps"].as_array().unwrap().iter().zip(captures).enumerate() {
        if step["release"] == true {
            mock.release();
        }
        if let Some(text) = step["type"].as_str() {
            tui.send_text(text);
        }
        if let Some(keys) = step["keys"].as_array() {
            tui.send_keys(&keys.iter().map(|k| k.as_str().unwrap()).collect::<Vec<_>>());
        }
        let want = recorded_version::current(want.as_str().unwrap());
        let want = want.as_str();
        let got = tui.wait_screen(tui.wait(step["wait"].as_str().unwrap()), want);
        if got != want {
            let diff: Vec<String> = got
                .lines()
                .zip(want.lines())
                .enumerate()
                .filter(|(_, (g, w))| g != w)
                .map(|(row, (g, w))| format!("row {row}:\n  got  {g:?}\n  want {w:?}"))
                .collect();
            panic!(
                "{name} step {i} differs from TypeScript:\n{}\n--- got\n{got}\n--- want\n{want}",
                diff.join("\n")
            );
        }
    }
}

macro_rules! parity {
    ($($test:ident => $name:literal),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                replay($name);
            }
        )*

        #[test]
        fn cli_interactive_fixture_has_a_test_per_scenario() {
            let tested = [$($name),*];
            for scenario in fixture()["scenarios"].as_array().unwrap() {
                let name = scenario["name"].as_str().unwrap();
                assert!(tested.contains(&name), "scenario {name} has no parity test");
            }
        }
    };
}

parity! {
    cli_interactive_startup_screen_matches_ts => "startup",
    cli_interactive_rlm_cell_turn_matches_ts => "prompt-rlm-cell",
    cli_interactive_streaming_and_steering_match_ts => "streaming-and-steer",
    cli_interactive_escape_aborts_turn_like_ts => "escape-abort",
    cli_interactive_ctrl_c_clears_editor_like_ts => "ctrl-c-clear",
    cli_interactive_slash_autocomplete_matches_ts => "slash-list",
    cli_interactive_unknown_command_matches_ts => "unknown-command",
    cli_interactive_session_info_matches_ts => "session-info",
    cli_interactive_hotkeys_match_ts => "hotkeys",
    cli_interactive_new_session_matches_ts => "new-session",
    cli_interactive_copy_without_messages_matches_ts => "copy-empty",
    cli_interactive_model_selector_matches_ts => "model-selector",
    cli_interactive_quit_matches_ts => "quit",
    cli_interactive_quit_after_turn_matches_ts => "quit-after-turn",
    cli_interactive_ctrl_d_exits_like_ts => "ctrl-d-exit",
    cli_interactive_splash_matches_ts => "splash",
}

/// Time to the first frame showing the header: under the TypeScript product's best time
/// (`fixtures/interactive-latency.json`, `interactive.mjs --latency`). A wall-clock benchmark against a time
/// recorded on an idle machine, so it runs alone, never inside the shared suite (it failed there under load).
#[test]
#[ignore = "benchmark: run alone with `cargo test -p cli --test it -- --ignored first_frame_beats`"]
fn cli_interactive_first_frame_beats_typescript_startup() {
    let latency: Value = serde_json::from_str(include_str!("../fixtures/interactive-latency.json")).unwrap();
    let budget = latency["tsFirstFrameMs"].as_u64().unwrap();
    let mock = Mock::start(Vec::new(), None);
    let mut best = u128::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !tui.capture().contains("ultron v") {
            assert!(Instant::now() < deadline, "no first frame");
            std::thread::sleep(Duration::from_millis(5));
        }
        best = best.min(started.elapsed().as_millis());
    }
    assert!(
        best < u128::from(budget),
        "first frame after {best} ms; TypeScript shows it after {budget} ms"
    );
}

fn footer_cwd_line(screen: &str) -> String {
    screen
        .lines()
        .find(|l| l.starts_with("~/cwd"))
        .unwrap_or("")
        .to_string()
}

/// The first prompt names the session at once (the first line, cleaned); the model's title replaces it when it
/// arrives; the name is stored in the session file.
#[test]
fn cli_interactive_first_prompt_names_session_then_model_title() {
    let mock = Mock::start_with_held_title(vec![text_reply("Sure.")], "Parser fix plan");
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("please   fix the parser\nthen run tests");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Sure.");
    // The fallback is up while the title call waits.
    assert_eq!(footer_cwd_line(&screen), "~/cwd • please fix the parser");
    mock.wait_title_request();
    mock.release_title();
    let screen = tui.wait("Parser fix plan");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • Parser fix plan");
    // Pi's terminal title carries the name too.
    assert_eq!(
        tui.tmux(&["display-message", "-p", "-t", "t", "#{pane_title}"]).trim(),
        "ultron - Parser fix plan - cwd"
    );
    assert_eq!(mock.title_requests(), 1);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    let names = stored_names(tui.root());
    assert_eq!(names, ["please fix the parser", "Parser fix plan"]);
}

/// `/name` before the title arrives wins: the late title is dropped, and `/name` with no argument shows the name.
#[test]
fn cli_interactive_slash_name_wins_over_late_title() {
    let mock = Mock::start_with_held_title(vec![text_reply("Ok.")], "Model title");
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("first prompt");
    tui.send_keys(&["Enter"]);
    tui.wait("Ok.");
    tui.send_text("/name My session");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Session name set: My session");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • My session");
    // The title arrives after the user's name and is dropped.
    mock.wait_title_request();
    mock.release_title();
    naming_recorded(tui.root());
    tui.send_text("/name");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Session name: My session");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • My session");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    assert_eq!(stored_names(tui.root()), ["first prompt", "My session"]);
}

/// A title that arrives after `/new` belongs to the old session: the new one stays unnamed.
#[test]
fn cli_interactive_late_title_does_not_name_a_new_session() {
    let mock = Mock::start_with_held_title(vec![text_reply("Ok.")], "Old title");
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("old work");
    tui.send_keys(&["Enter"]);
    tui.wait("Ok.");
    mock.wait_title_request();
    tui.send_text("/new");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("New session started");
    assert_eq!(footer_cwd_line(&screen), "~/cwd");
    mock.release_title();
    // A title the client read before `/name` reaches the session would be applied first: the session takes its
    // inputs in order.
    mock.wait_title_consumed();
    tui.send_text("/name");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Usage: /name <name>");
    assert_eq!(footer_cwd_line(&screen), "~/cwd");
}

/// `ultron -n <name>` names the session from the start; the first prompt does not rename it.
#[test]
fn cli_interactive_name_flag_names_session() {
    let mock = Mock::start(vec![text_reply("Hi.")], Some("Ignored".into()));
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5", "-n", "Release prep"],
        &[],
    );
    let screen = tui.wait("ultron v");
    assert_eq!(
        footer_cwd_line(&tui.wait("Session:")),
        "~/cwd • Release prep",
        "{screen}"
    );
    tui.send_text("hello");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Hi.");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • Release prep");
    assert_eq!(mock.title_requests(), 0);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    assert_eq!(stored_names(tui.root()), ["Release prep"]);
}

/// Wait until the naming response was recorded (`record_naming` commits its usage row, then applies or drops the
/// title in the same step), hang guard 120 s: the late title has been handled, whichever way.
fn naming_recorded(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !session_files(&root.join("agent/experimental/sessions"))
        .iter()
        .any(|f| std::fs::read_to_string(f).is_ok_and(|t| t.contains("\"source\":\"session-naming\"")))
    {
        assert!(Instant::now() < deadline, "the naming response was never recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Names stored in the session files under the agent dir (each `ultron.session.name` value the format-4 journals set),
/// oldest first.
fn stored_names(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for file in session_files(&root.join("agent/experimental/sessions")) {
        for line in std::fs::read_to_string(file).unwrap().lines().skip(1) {
            let records = match serde_json::from_str::<Value>(line).unwrap() {
                Value::Array(records) => records,
                record => vec![record],
            };
            for record in records {
                if record["namespace"] == "ultron.session.name" && record["op"] == "set" {
                    names.push(record["value"].as_str().unwrap().to_string());
                }
            }
        }
    }
    names
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(session_files(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    found
}

/// Two sessions in this directory (one named) and one elsewhere: `/resume` lists this directory's only, by name or
/// first message, and switching shows the resumed transcript; `ultron -r` lists them with names and opens the pick.
#[test]
fn cli_interactive_resume_lists_this_directory_with_names() {
    let mock = Mock::start(vec![text_reply("One."), text_reply("Two.")], None);
    let tmp = TempDir::new();
    let root = tmp.0.clone();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let quiet = [(session_core::core::AUTO_NAME_ENV, "off")];
    {
        let tui = Tui::start_at(
            &root,
            mock.port,
            100,
            30,
            &[&args[..], &["-n", "Named one"]].concat(),
            &quiet,
        );
        tui.wait("ultron v");
        tui.send_text("alpha prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("One.");
        tui.send_text("/new");
        tui.send_keys(&["Enter"]);
        tui.wait("New session started");
        tui.send_text("beta prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("Two.");
        // A session of another directory, in this directory's session folder and in its own.
        let here = session_files(&root.join("agent/experimental/sessions"))[0]
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::write(
            here.join("2026-01-02T00-00-00-000Z_0199bbbb-0000-7000-8000-000000000000.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"0199bbbb-0000-7000-8000-000000000000\",\"timestamp\":\"2026-01-02T00:00:00.000Z\",\"cwd\":\"/elsewhere\"}\n{\"type\":\"session_info\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"2026-01-02T00:00:01.000Z\",\"name\":\"Foreign\"}\n",
        )
        .unwrap();
        let other = root.join("agent/experimental/sessions/--elsewhere--");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("2026-01-01T00-00-00-000Z_0199aaaa-0000-7000-8000-000000000000.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"0199aaaa-0000-7000-8000-000000000000\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/elsewhere\"}\n",
        )
        .unwrap();
        tui.send_text("/resume");
        tui.send_keys(&["Enter"]);
        // Each check waits for the frame that completes it: a list, transcript or footer can be drawn after the
        // frame that first shows the heading or status (a fixed settle time loses that race under load).
        let screen = tui.wait_for("the session list", |s| {
            s.contains("Resume Session") && s.contains("› beta prompt") && s.contains("  Named one")
        });
        assert!(!screen.contains("elsewhere") && !screen.contains("Foreign"), "{screen}");
        tui.send_keys(&["Down", "Enter"]);
        let screen = tui.wait_for("the resumed transcript", |s| {
            s.contains("Resumed session")
                && s.contains("alpha prompt")
                && s.contains("One.")
                && !s.contains("beta prompt")
                && footer_cwd_line(s) == "~/cwd • Named one"
        });
        assert!(!screen.contains("beta prompt"), "{screen}");
        tui.send_text("/quit");
        tui.send_keys(&["Enter"]);
        tui.wait("<exited>");
    }
    let tui = Tui::start_at(&root, mock.port, 100, 30, &[&args[..], &["-r"]].concat(), &quiet);
    let screen = tui.wait_for("both sessions listed", |s| {
        s.contains("Select a session number:") && s.lines().filter(|l| l.starts_with("  ")).count() >= 2
    });
    let listed: Vec<&str> = screen.lines().filter(|l| l.starts_with("  ")).collect();
    assert_eq!(listed.len(), 2, "{screen}");
    // Listed newest file first (`listNativeSessions`, session_journal_lists_sessions_newest_modified_first): which
    // file was written last depends on whether the resumed session's kernel started (its module start writes the
    // skills catalog) before `/quit`, so the rows are found by content, not position.
    let named = listed
        .iter()
        .position(|l| l.ends_with("<ID>  Named one"))
        .unwrap_or_else(|| panic!("no named row: {screen}"));
    let other = listed[1 - named];
    assert_eq!(listed[named], format!("  {}. <ID>  Named one", named + 1), "{screen}");
    assert_eq!(other, format!("  {}. <ID>", 2 - named), "{screen}");
    tui.send_text(&(named + 1).to_string());
    tui.send_keys(&["Enter"]);
    let screen = tui.wait_for("the picked session", |s| {
        s.contains("One.") && s.contains("alpha prompt") && footer_cwd_line(s) == "~/cwd • Named one"
    });
    assert!(screen.contains("alpha prompt"), "{screen}");
}

/// `/resume` search covers every message (`allMessagesText`), through the worker's session listing: a word only a
/// later reply holds finds that session; its row still shows the first prompt.
#[test]
fn cli_interactive_resume_search_finds_a_word_from_a_later_message() {
    let mock = Mock::start(
        vec![text_reply("One."), text_reply("Quokka habitat."), text_reply("Three.")],
        None,
    );
    let tmp = TempDir::new();
    let root = tmp.0.clone();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let tui = Tui::start_at(
        &root,
        mock.port,
        100,
        30,
        &args,
        &[(session_core::core::AUTO_NAME_ENV, "off")],
    );
    tui.wait("ultron v");
    for (prompt, reply) in [("alpha prompt", "One."), ("second ask", "Quokka habitat.")] {
        tui.send_text(prompt);
        tui.send_keys(&["Enter"]);
        tui.wait(reply);
    }
    tui.send_text("/new");
    tui.send_keys(&["Enter"]);
    tui.wait("New session started");
    tui.send_text("beta prompt");
    tui.send_keys(&["Enter"]);
    tui.wait("Three.");
    tui.send_text("/resume");
    tui.send_keys(&["Enter"]);
    tui.wait_for("the session list", |s| {
        s.contains("Resume Session") && s.contains("beta prompt") && s.contains("alpha prompt")
    });
    // A quoted phrase matches as a substring, so nothing in the temp paths can match by accident.
    tui.send_text("\"quokka\"");
    // The selector's rows: the lines between its search line and the rule under it (the transcript above still
    // shows the current session).
    let rows = |s: &str| -> Vec<String> {
        s.lines()
            .skip_while(|l| !l.starts_with("> \"quokka\""))
            .skip(1)
            .take_while(|l| !l.starts_with('─'))
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    };
    let screen = tui.wait_for("the search result", |s| {
        rows(s)
            .first()
            .is_some_and(|r| r.starts_with("› alpha prompt") || r.contains("No sessions"))
    });
    let rows = rows(&screen);
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(rows[0].starts_with("› alpha prompt"), "{screen}");
    tui.send_keys(&["Enter"]);
    tui.wait_for("the resumed transcript", |s| {
        s.contains("Resumed session") && s.contains("Quokka habitat.") && !s.contains("beta prompt")
    });
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// Up in the editor recalls prompts from earlier runs in this directory (prompt-history.jsonl).
#[test]
fn cli_interactive_prompt_history_survives_restart() {
    let mock = Mock::start(vec![text_reply("A."), text_reply("B.")], None);
    let tmp = TempDir::new();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let quiet = [(session_core::core::AUTO_NAME_ENV, "off")];
    {
        let tui = Tui::start_at(&tmp.0, mock.port, 100, 30, &args, &quiet);
        tui.wait("ultron v");
        tui.send_text("remember this prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("A.");
        tui.send_keys(&["C-d"]);
        tui.wait("<exited>");
    }
    let history = std::fs::read_to_string(tmp.0.join("agent/prompt-history.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(history.lines().next().unwrap()).unwrap();
    assert_eq!(entry["text"], "remember this prompt");
    assert_eq!(entry["cwd"], tmp.0.join("cwd").display().to_string());
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 30, &args, &quiet);
    tui.wait("Session:");
    tui.send_keys(&["Up"]);
    let screen = tui.wait("remember this prompt");
    assert!(screen.lines().any(|l| l == " remember this prompt"), "{screen}");
}

/// `/copy` puts the last assistant text on the clipboard through the terminal (OSC 52; tmux keeps it as a buffer).
#[test]
fn cli_interactive_copy_sends_last_assistant_text_to_the_terminal_clipboard() {
    let mock = Mock::start(vec![text_reply("Copy **me** please.")], None);
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        // The TypeScript `/copy` (with the overlays on, `/copy` opens the picker: interactive_overlays.rs).
        &[
            (session_core::core::AUTO_NAME_ENV, "off"),
            (interactive::qol::PARITY_ENV, "1"),
        ],
    );
    tui.wait("Session:");
    tui.tmux(&["set-option", "-s", "set-clipboard", "on"]);
    tui.send_text("hi");
    tui.send_keys(&["Enter"]);
    tui.wait("Copy me please.");
    tui.send_text("/copy");
    tui.send_keys(&["Enter"]);
    tui.wait("Copied last agent message to clipboard");
    assert_eq!(tui.tmux(&["show-buffer"]), "Copy **me** please.");
}
