//! The turn guards through the session actor (faux Anthropic server on loopback, private temp profile): an empty
//! reply is continued twice and then ends the turn, what a subagent left undone (`rlm.finish(left=[...])`) continues
//! the root's turn once, and the `turnEndChecks` setting's commands are run by the host when a turn that made an
//! `rlm` call ends, continuing it with the ones that failed, five times at most since the user's last input.

use crate::p_session_support;

use p_session_support::{Actor, Script, Setup, actor, in_order, text, tool, tool_results, user_texts};
use serde_json::{Value, json};
use session_core::core::Op;
use session_core::core::turn_guard::{EMPTY_TEXT, parse_checks};

/// A reply with no content at all.
fn empty() -> String {
    [
        json!({"type": "message_start", "message": {"id": "msg_e", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 0}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

fn settle(a: &Actor) {
    a.call(Op::Describe).unwrap();
}

/// The guards' nudges, as clients are shown them.
fn nudges(a: &Actor, guard: &str) -> Vec<Value> {
    a.events("message_end")
        .into_iter()
        .map(|e| e["message"].clone())
        .filter(|m| m["details"]["guard"] == guard)
        .collect()
}

/// Waits until the session is idle after its `runs`-th run ended: no run, no check in progress.
fn idle_after(a: &Actor, runs: usize) {
    a.events.wait("idle after the turn-end checks", |events| {
        let end = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e["type"] == "run_end")
            .nth(runs - 1)?
            .0;
        events[end..]
            .iter()
            .any(|e| e["type"] == "activity" && e["active"] == false)
            .then_some(())
    });
    settle(a);
}

#[test]
fn session_core_turn_guard_continues_an_empty_reply_twice_then_ends_the_turn() {
    let a = actor(Box::new(|_, _| empty()), Setup::default());
    let ended = a.prompt("fix the bug");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    a.events.wait_count("run_end", 3);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 3, "the prompt's run and two continues");
    let sent = nudges(&a, "empty reply");
    let details: Vec<Value> = sent.iter().map(|m| m["details"].clone()).collect();
    assert_eq!(
        details,
        [
            json!({"guard": "empty reply", "nudge": 1, "max": 2}),
            json!({"guard": "empty reply", "nudge": 2, "max": 2})
        ]
    );
    assert_eq!(sent[0]["content"], EMPTY_TEXT);
    // The model reads it as the next user message.
    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(user_texts(&requests[1]).last().unwrap(), &json!(EMPTY_TEXT));
    // The user's next prompt starts the count over; a reply with text is never continued.
    let b = actor(in_order(vec![empty(), text("Done: nothing to fix.")]), Setup::default());
    b.prompt("go");
    b.events.wait_count("run_end", 2);
    settle(&b);
    assert_eq!(b.events("run_start").len(), 2);
    assert_eq!(nudges(&b, "empty reply").len(), 1);
}

#[test]
fn session_core_turn_guard_is_off_under_its_switch_and_the_typescript_guide() {
    for var in [("ULTRON_TURN_GUARDS", "off"), ("ULTRON_GUIDE", "ts")] {
        let a = actor(
            Box::new(|_, _| empty()),
            Setup {
                vars: vec![(var.0, var.1.to_string())],
                ..Setup::default()
            },
        );
        a.prompt("go");
        settle(&a);
        assert_eq!(a.events("run_start").len(), 1, "{var:?}");
        assert!(nudges(&a, "empty reply").is_empty(), "{var:?}");
    }
}

fn checks_setup(command: &str) -> Setup {
    Setup {
        settings: Some(json!({ "turnEndChecks": [command] })),
        ..Setup::default()
    }
}

#[test]
fn session_core_turn_end_checks_continue_the_turn_until_they_pass() {
    let script = in_order(vec![
        tool("t1", "print(1)"),
        text("Done."),
        tool("t2", "open('done.txt', 'w').write('x')\nprint('written')"),
        text("Fixed."),
    ]);
    let a = actor(script, checks_setup("test -f done.txt"));
    let ended = a.prompt("do the work");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    idle_after(&a, 2);
    assert_eq!(a.events("run_start").len(), 2, "the passing checks continue nothing");
    let sent = nudges(&a, "turn-end checks failed");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0]["details"],
        json!({"guard": "turn-end checks failed", "nudge": 1, "max": 5})
    );
    let said = sent[0]["content"].as_str().unwrap();
    assert!(said.contains("\n- run \"test -f done.txt\": exit 1\n"), "{said}");
    let requests = a.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(user_texts(&requests[2]).last().unwrap(), &json!(said));
}

#[test]
fn session_core_turn_end_checks_stop_at_five_continues_and_skip_a_turn_without_work() {
    let script: Script = Box::new(|_, index| {
        if index % 2 == 0 {
            tool(&format!("t{index}"), "print(1)")
        } else {
            text("Done.")
        }
    });
    let a = actor(script, checks_setup("false"));
    a.prompt("go");
    a.events.wait_count("run_end", 6);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 6, "the prompt's run and five continues");
    let numbers: Vec<Value> = nudges(&a, "turn-end checks failed")
        .iter()
        .map(|m| m["details"]["nudge"].clone())
        .collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5].map(Value::from));
    // A turn that only talked changed nothing: no check runs for it.
    let talks = actor(in_order(vec![text("Here is the plan.")]), checks_setup("false"));
    talks.prompt("plan it");
    idle_after(&talks, 1);
    assert_eq!(talks.events("run_start").len(), 1);
    assert!(nudges(&talks, "turn-end checks failed").is_empty());
}

#[test]
fn session_core_turn_end_checks_setting_is_a_list_of_commands() {
    let parse = |value: Value| parse_checks(json!({ "turnEndChecks": value }).as_object().unwrap());
    assert_eq!(parse(json!(null)).unwrap().len(), 0);
    let checks = parse(json!(["cargo fmt --check", {"run": "scripts/test.sh", "timeout_ms": 300000}])).unwrap();
    assert_eq!(
        checks.iter().map(|c| c.label()).collect::<Vec<_>>(),
        ["run \"cargo fmt --check\"", "run \"scripts/test.sh\""]
    );
    assert_eq!(
        parse(json!("make")).unwrap_err(),
        "turnEndChecks must be a list of commands"
    );
    assert_eq!(
        parse(json!([{"files_exist": ["a"]}])).unwrap_err(),
        "turnEndChecks[0] must be a command (a string or {\"run\": ...})"
    );
    assert!(
        parse(json!([{"run": "x", "timeout_ms": 0}]))
            .unwrap_err()
            .starts_with("turnEndChecks[0].timeout_ms")
    );
}

#[test]
fn session_core_turn_guard_gives_the_root_what_its_subagent_left_undone_once() {
    let root = "import asyncio\nh = await rlm.spawn('CHILD: fix the parser', name='fixer')\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([h]))[0]['result']\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nprint(r['verdict']['left'])";
    let child = "print(await rlm.finish('failed', 'fixed half of it', left=['the retry path is untested', 'docs/api.md is stale']))";
    let script: Script = Box::new(move |request, _| {
        let first = user_texts(request)
            .iter()
            .filter_map(Value::as_str)
            .find(|t| !t.starts_with("[clock]"))
            .unwrap_or_default()
            .to_string();
        let has_results = !tool_results(request).is_empty();
        match (first.starts_with("ROOT"), has_results) {
            (true, false) => tool("toolu_root", root),
            (true, true) => text("root done"),
            (false, false) => tool("toolu_child", child),
            (false, true) => text("child done"),
        }
    });
    let a = actor(script, Setup::default());
    let ended = a.prompt("ROOT: delegate");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    // The nudge's run, to its end.
    a.events.wait("the left items' run", |events| {
        let at = events
            .iter()
            .position(|e| e["type"] == "message_end" && e["message"]["details"]["guard"] == "work left by subagents")?;
        events[at..].iter().any(|e| e["type"] == "run_end").then_some(())
    });
    settle(&a);
    let sent = nudges(&a, "work left by subagents");
    assert_eq!(sent.len(), 1, "once per subagent: {sent:?}");
    let said = sent[0]["content"].as_str().unwrap();
    assert!(said.starts_with("Subagents ended with work left undone"), "{said}");
    assert!(
        said.contains("\n- the retry path is untested\n- docs/api.md is stale\n"),
        "{said}"
    );
    assert!(said.contains("fixer ("), "{said}");
    // The root's cell saw them in the verdict, and the event's tag counts them.
    let results: Vec<Value> = a.requests().iter().flat_map(tool_results).collect();
    assert!(
        results
            .iter()
            .any(|r| r.to_string().contains("the retry path is untested")),
        "{results:?}"
    );
}

#[test]
fn session_core_turn_end_checks_keep_the_session_busy_and_a_new_prompt_cancels_them() {
    let script = in_order(vec![tool("t1", "print(1)"), text("Done."), text("Stopped.")]);
    // Never passes on its own: it waits for a file nobody writes.
    let a = actor(script, checks_setup("while [ ! -e go ]; do sleep 0.05; done; false"));
    let ended = a.prompt("do the work");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    // The run is over and the checks run: a client waiting for idle keeps waiting.
    let state = a.call(Op::State).unwrap().unwrap();
    assert_eq!(
        (&state["isStreaming"], &state["isChecking"]),
        (&json!(false), &json!(true)),
        "{state}"
    );
    let pending = a.call(Op::AsyncPending).unwrap().unwrap();
    assert_eq!(pending["pending"], true, "{pending}");
    // The user moved on: the checks are stopped and their result continues nothing.
    a.prompt("never mind");
    idle_after(&a, 2);
    let state = a.call(Op::State).unwrap().unwrap();
    assert_eq!(state.get("isChecking"), None, "{state}");
    assert_eq!(a.events("run_start").len(), 2);
    assert!(nudges(&a, "turn-end checks failed").is_empty());
}
