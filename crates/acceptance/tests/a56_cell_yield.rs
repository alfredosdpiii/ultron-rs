//! A56 (Rust): a slow cell never holds the chat. In the real CLI over RPC with a scripted provider, a root cell still
//! running after `ULTRON_CELL_YIELD_AFTER` returns its tool call (the cell goes on in the background), the turn ends,
//! the user chats on while it runs, and its end arrives as a `cell_done` `<runtime_event>` that re-invokes the idle
//! root with the cell's output. The cell waits for a file the test writes, never on a timer.

mod common;

use common::{HANG, Install, Provider, Reply, Request};

const PROMPT: &str = "YIELD: crunch the numbers";
const SECOND: &str = "SECOND: are you still there?";

fn first_user(request: &Request) -> String {
    request
        .users()
        .into_iter()
        .find(|u| !u.starts_with("[clock]"))
        .unwrap_or_default()
}

fn last_user(request: &Request) -> String {
    request.users().last().cloned().unwrap_or_default()
}

fn last_is_tool(request: &Request) -> bool {
    request.body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .is_some_and(|m| m["role"] == "tool")
}

#[test]
fn acceptance_a56_a_slow_root_cell_yields_its_turn_the_user_chats_on_and_its_end_reinvokes_the_model() {
    let provider = Provider::start(|request: &Request| {
        if first_user(request) != PROMPT {
            return Reply::Text("unexpected".into());
        }
        let user = last_user(request);
        if !last_is_tool(request) && user.contains("<runtime_event kind=\"cell_done\"") {
            return Reply::Text("CELL FINISHED".into());
        }
        if !last_is_tool(request) && user == SECOND {
            return Reply::Text("STILL HERE".into());
        }
        if request.messages("assistant").is_empty() {
            return Reply::Rlm(
                "import os, time\nwhile not os.path.exists('go'):\n    time.sleep(0.05)\nprint('CRUNCHED 42')".into(),
            );
        }
        Reply::Text("WORKING ON IT IN THE BACKGROUND".into())
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_CELL_YIELD_AFTER", "1")]);

    // The cell outlives its 1 s budget: the tool call returns and the turn ends while it still runs.
    client.prompt_and_wait(PROMPT, HANG).unwrap();
    assert!(!install.project().join("go").exists());
    let cell = provider.requests()[1].tool_results().last().cloned().unwrap();
    assert!(
        cell.starts_with("Cell ") && cell.contains(" was still running after 1 s"),
        "{cell}"
    );
    assert!(cell.contains("<runtime_event kind=\"cell_done\""), "{cell}");
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("WORKING ON IT IN THE BACKGROUND")
    );

    // The user keeps chatting while the cell runs.
    client.prompt_and_wait(SECOND, HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("STILL HERE"));

    // Its end wakes the idle root with the cell's output.
    std::fs::write(install.project().join("go"), "").unwrap();
    common::wait_until("the cell_done run answers", || {
        client.get_last_assistant_text().ok().flatten().as_deref() == Some("CELL FINISHED")
    });
    let event = provider
        .requests()
        .iter()
        .map(last_user)
        .find(|u| u.contains("<runtime_event kind=\"cell_done\""))
        .unwrap();
    assert!(
        event.contains("status=\"completed\"") && event.contains("CRUNCHED 42"),
        "{event}"
    );
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
