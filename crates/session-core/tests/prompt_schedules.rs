//! Scheduled prompts through the session actor (a Rust-only feature; literal expectations): a cell creates a
//! schedule with `schedule.every` (schedule_api.py over kernel_apis::prompt_schedule), the server's fire
//! (`Op::ScheduleFire`, as crates/daemon sends it) starts a run whose prompt is the `ultron.schedule` custom message,
//! duplicate and early fires do nothing, missed slots collapse into one run, a fire during a run waits for that run
//! to end, and a cancel stops it. Time is a test clock file (`__ULTRON_SCHEDULE_CLOCK_FILE`), never a real wait.

use crate::p_session_support;

use kernel_apis::prompt_schedule::{CLOCK_FILE_ENV, NAMESPACE};
use p_session_support::{HANG, Setup, actor, in_order, text, tool, tool_results, user_texts};
use protocol::custom_types::SCHEDULE;
use serde_json::{Value, json};
use session_core::core::Op;
use std::path::{Path, PathBuf};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn clock(dir: &Path) -> PathBuf {
    let path = dir.join("clock");
    std::fs::write(&path, T0.to_string()).unwrap();
    path
}

fn set_clock(path: &Path, now: i64) {
    std::fs::write(path, now.to_string()).unwrap();
}

fn setup(clock: &Path) -> Setup {
    Setup {
        vars: vec![(CLOCK_FILE_ENV, clock.display().to_string())],
        ..Setup::default()
    }
}

fn list(a: &p_session_support::Actor) -> Vec<Value> {
    a.call(Op::Inspect {
        request: "schedule.list".into(),
        payload: json!({}),
    })
    .unwrap()
    .unwrap()
    .as_array()
    .unwrap()
    .clone()
}

fn fire(a: &p_session_support::Actor, id: &str, due: i64) -> Value {
    a.call(Op::ScheduleFire { id: id.into(), due }).unwrap().unwrap()
}

fn schedule_messages(a: &p_session_support::Actor) -> Vec<Value> {
    a.events("message_end")
        .into_iter()
        .map(|e| e["message"].clone())
        .filter(|m| m["customType"] == SCHEDULE)
        .collect()
}

const CREATE: &str = "sid = await schedule.every('1h', 'check the build', name='build')\nprint(sid)";

#[test]
fn session_core_schedule_from_a_cell_fires_as_a_custom_message_run() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let script = in_order(vec![
        tool("toolu_1", CREATE),
        text("Scheduled."),
        text("Build is green."),
        text("Still green."),
    ]);
    let a = actor(script, setup(&clock));
    let ended = a.prompt("watch the build");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let listed = list(&a);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let id = listed[0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            listed[0]["name"].clone(),
            listed[0]["nextDue"].clone(),
            listed[0]["runs"].clone()
        ),
        (json!("build"), json!(T0 + HOUR), json!(0))
    );
    // The cell printed the id it got back.
    let results = tool_results(&a.requests()[1]);
    assert!(results[0].to_string().contains(&id), "{results:?}");
    // Early: nothing runs.
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": false, "run": null, "schedules": [{"id": id, "name": "build", "nextDue": T0 + HOUR}]})
    );
    set_clock(&clock, T0 + HOUR + 5_000);
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": true, "run": 1, "schedules": [{"id": id, "name": "build", "nextDue": T0 + 2 * HOUR}]})
    );
    a.events.wait_count("run_end", 2);
    // The model reads the prompt as the run's user message.
    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(user_texts(&requests[2]).last().unwrap(), &json!("check the build"));
    // Clients see the custom message, never a user bubble with its text.
    let shown = schedule_messages(&a);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["details"], json!({"name": "build", "id": id, "run": 1}));
    assert_eq!(shown[0]["content"], "check the build");
    let echoed: Vec<Value> = a
        .events("message_end")
        .into_iter()
        .filter(|e| e["message"]["role"] == "user" && e.to_string().contains("check the build"))
        .collect();
    assert!(echoed.is_empty(), "{echoed:?}");
    // A duplicate fire of the same slot does nothing.
    assert_eq!(fire(&a, &id, T0 + HOUR)["fired"], false);
    // Four slots pass unserved: one run, the next slot after now.
    set_clock(&clock, T0 + 6 * HOUR + 1);
    let missed = fire(&a, &id, T0 + 2 * HOUR);
    assert_eq!(
        (missed["fired"].clone(), missed["run"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(missed["schedules"][0]["nextDue"], T0 + 7 * HOUR);
    a.events.wait_count("run_end", 3);
    assert_eq!(a.requests().len(), 4);
    assert_eq!(
        schedule_messages(&a)
            .iter()
            .map(|m| m["details"]["run"].clone())
            .collect::<Vec<_>>(),
        vec![json!(1), json!(2)]
    );
    // The transcript a resumed session opens with keeps them as custom messages.
    let messages = a.call(Op::Messages).unwrap().unwrap();
    let kept: Vec<&Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["customType"] == SCHEDULE)
        .collect();
    assert_eq!(kept.len(), 2, "{messages}");
    assert_eq!(kept[0]["role"], "custom");
    // Every change is a journal write of the schedule's value; a cancel too.
    let cancelled = a.call(Op::ScheduleCancel { id: "build".into() }).unwrap().unwrap();
    assert_eq!(cancelled["schedule"]["state"], "cancelled");
    assert_eq!(cancelled["schedules"], json!([]));
    set_clock(&clock, T0 + 7 * HOUR);
    assert_eq!(fire(&a, &id, T0 + 7 * HOUR)["fired"], false);
    let states: Vec<(Value, Value)> = a
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == NAMESPACE && w["op"] == "set")
        .map(|w| (w["value"]["runs"].clone(), w["value"]["state"].clone()))
        .collect();
    assert_eq!(
        states,
        vec![
            (json!(0), json!("active")),
            (json!(1), json!("active")),
            (json!(2), json!("active")),
            (json!(2), json!("cancelled")),
        ]
    );
    assert_eq!(a.events("run_start").len(), 3);
}

#[test]
fn session_core_schedule_fired_during_a_run_waits_for_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let replies = [
        tool("toolu_1", "await schedule.every('1h', 'check nightly', name='nightly')"),
        text("Scheduled."),
        text("Long answer."),
        text("Nightly done."),
    ];
    let script: p_session_support::Script = Box::new(move |_, index| {
        // The third request (the long run) is answered once the test fired the schedule.
        if index == 2 {
            let _ = released.recv_timeout(HANG);
        }
        replies.get(index).cloned().unwrap_or_else(|| text("(no more replies)"))
    });
    let a = actor(script, setup(&clock));
    a.prompt("schedule the nightly check");
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    set_clock(&clock, T0 + HOUR);
    a.call(Op::Prompt {
        message: "long task".into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap();
    a.requests
        .wait("the long run's request", |requests| (requests.len() >= 3).then_some(()));
    // The fire is recorded at once, but its run waits for the one in progress.
    assert_eq!(fire(&a, &id, T0 + HOUR)["run"], 1);
    assert_eq!(a.events("run_start").len(), 2);
    release.send(()).unwrap();
    a.events.wait_count("run_end", 3);
    let requests = a.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !user_texts(&requests[2]).contains(&json!("check nightly")),
        "{:?}",
        user_texts(&requests[2])
    );
    assert_eq!(user_texts(&requests[3]).last().unwrap(), &json!("check nightly"));
    assert!(a.events("run_end").iter().all(|e| e["status"] == "completed"));
}

#[test]
fn session_core_schedule_python_api_validates_and_lists() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let code = "\
for bad in [('30s', 'x'), ('1h', '')]:
    try:
        await schedule.every(*bad)
    except Exception as e:
        print(type(e).__name__, e)
sid = await schedule.at(90, 'one shot', name='once')
rows = await schedule.list()
print([(r['name'], r['kind'], r['next']) for r in rows])
print((await schedule.cancel('once'))['state'])
print(repr(schedule))";
    let script = in_order(vec![tool("toolu_1", code), text("Done.")]);
    let a = actor(script, setup(&clock));
    a.prompt("try schedules");
    let results = tool_results(&a.requests()[1])
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    for expected in [
        "RuntimeError schedule interval must be at least 60s (got 30s)",
        "ValueError schedule prompt must be a non-empty string",
        "[('once', 'at', '2027-01-15T08:01:30.000Z')]",
        "cancelled",
        "<schedule: watch_pr(repo, pr, mode=, prompt=), loop(interval, prompt), every(interval, prompt, name=, until=, max_runs=), at(when, prompt), list(), cancel(id)>",
    ] {
        assert!(results.contains(expected), "{expected} not in {results}");
    }
}

#[test]
fn session_core_schedule_fire_that_cannot_start_counts_nothing_and_answers_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    // The key is a stored credential the test removes: then a prompt cannot start (a worker without credentials).
    let mut setup = setup(&clock);
    setup.vars.push(("ANTHROPIC_API_KEY", String::new()));
    let script = in_order(vec![
        tool("toolu_1", "await schedule.at(3600, 'say hi', name='hello')"),
        text("Scheduled."),
        text("Hi."),
    ]);
    let a = actor(script, setup);
    let auth = a.root.join("agent/auth.json");
    let store_key = || std::fs::write(&auth, r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#).unwrap();
    store_key();
    assert_eq!(a.prompt("greet me later")["error"], Value::Null);
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    std::fs::remove_file(&auth).unwrap();
    set_clock(&clock, T0 + HOUR + 5);
    for _ in 0..2 {
        let error = a
            .call(Op::ScheduleFire {
                id: id.clone(),
                due: T0 + HOUR,
            })
            .unwrap_err();
        assert!(error.contains("No API key found for anthropic"), "{error}");
    }
    // Still due on its slot (an `at` schedule is not done), no run counted, nothing started or shown.
    let listed = list(&a);
    assert_eq!(
        (
            listed[0]["runs"].clone(),
            listed[0]["state"].clone(),
            listed[0]["nextDue"].clone()
        ),
        (json!(0), json!("active"), json!(T0 + HOUR))
    );
    assert_eq!(a.events("run_start").len(), 1);
    assert!(schedule_messages(&a).is_empty());
    let sets = || {
        a.writes()
            .into_iter()
            .filter(|w| w["namespace"] == NAMESPACE && w["op"] == "set")
            .map(|w| (w["value"]["runs"].clone(), w["value"]["state"].clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(sets(), vec![(json!(0), json!("active"))]);
    // The retry once the key is back runs it, and only then is it counted (and done).
    store_key();
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": true, "run": 1, "schedules": []})
    );
    a.events.wait_count("run_end", 2);
    assert_eq!(user_texts(&a.requests()[2]).last().unwrap(), &json!("say hi"));
    assert_eq!(sets(), vec![(json!(0), json!("active")), (json!(1), json!("done"))]);
}

#[test]
fn session_core_schedule_loop_and_watch_python_bridge_and_topic_changes() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let code = "\
loop_id = await schedule.loop('5m', 'check local task', name='loop', max_runs=2)
assert loop_id == await schedule.loop('5m', 'check local task')
watch_id = await schedule.watch_pr('acme/widget', 42, mode='watch', prompt='report changes', name='watch')
assert watch_id == await schedule.watch_pr('acme/widget', 42, prompt='report changes')
print([(s['name'], s['operation']['type'], s['maxRuns']) for s in await schedule.list()])";
    let a = actor(
        in_order(vec![
            tool("t1", code),
            text("Registered."),
            text("Other topic."),
            text("Loop result."),
        ]),
        setup(&clock),
    );
    a.prompt("loop this every 5 minutes and keep watching the PR");
    let results = tool_results(&a.requests()[1])
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    assert!(results.contains("('loop', 'loop', 2)"), "{results}");
    assert!(results.contains("('watch', 'pr', 100)"), "{results}");
    a.prompt("an unrelated question");
    assert!(list(&a).iter().all(|s| s["state"] == "active"));
    let loop_id = list(&a).iter().find(|s| s["name"] == "loop").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    set_clock(&clock, T0 + 300_000);
    assert_eq!(fire(&a, &loop_id, T0 + 300_000)["fired"], true);
    a.events.wait_count("run_end", 3);
    a.call(Op::ScheduleCancel { id: "watch".into() }).unwrap();
    a.call(Op::ScheduleCancel { id: loop_id }).unwrap();
    assert!(list(&a).iter().all(|s| s["state"] == "cancelled"));
}

#[test]
fn session_core_schedule_cancel_suppresses_already_queued_final_loop_turn() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let replies = [
        tool(
            "t1",
            "await schedule.loop('1m', 'queued task', name='loop', max_runs=1)",
        ),
        text("Registered."),
        text("Long answer."),
    ];
    let script: p_session_support::Script = Box::new(move |_, index| {
        if index == 2 {
            released.recv_timeout(HANG).unwrap();
        }
        replies.get(index).cloned().unwrap_or_else(|| text("unexpected run"))
    });
    let a = actor(script, setup(&clock));
    a.prompt("loop once");
    set_clock(&clock, T0 + 60_000);
    a.call(Op::Prompt {
        message: "another task".into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap();
    a.requests
        .wait("long request", |requests| (requests.len() >= 3).then_some(()));
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    assert_eq!(fire(&a, &id, T0 + 60_000)["fired"], true);
    a.call(Op::ScheduleCancel { id }).unwrap();
    release.send(()).unwrap();
    a.events.wait_count("run_end", 2);
    // A synchronous actor round-trip drains the queued Scheduled input after the completed run.
    list(&a);
    assert_eq!(a.events("run_start").len(), 2);
    assert_eq!(a.requests().len(), 3);
}

#[test]
fn session_core_pr_watch_polls_code_only_then_wakes_model_for_changed_ci() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let gh = dir.path().join("gh");
    std::fs::write(&gh,format!("#!{}\n", kernel::env::python(|name| std::env::var(name).ok())) + r#"import json, pathlib, sys
root = pathlib.Path(__file__).parent
if sys.argv[1:3] == ['pr','view']:
    print((root / 'view').read_text())
elif sys.argv[1:5] == ['api','--hostname','github.com','graphql']:
    print(json.dumps({'data':{'repository':{'pullRequest':{'reviewThreads':{'nodes':[],'pageInfo':{'hasNextPage':False}}}}}}))
else:
    sys.exit(1)
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = |ci: &str| {
        std::fs::write(
            dir.path().join("view"),
            json!({
                "state":"OPEN","mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":"",
                "headRefOid":"abc123","reviews":[],"statusCheckRollup":[{"context":"ci","state":ci}]
            })
            .to_string(),
        )
        .unwrap()
    };
    status("PENDING");
    let mut config = setup(&clock);
    config
        .vars
        .push(("PATH", format!("{}:/usr/bin:/bin", dir.path().display())));
    let script = in_order(vec![
        tool(
            "t1",
            "await schedule.watch_pr('acme/widget', 42, prompt='report changes', interval='1m', name='watch')",
        ),
        text("Watching."),
        text("Answer to another topic."),
        text("CI changed to success."),
    ]);
    let a = actor(script, config);
    a.prompt("keep watching this PR");
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    assert_eq!(fire(&a, &id, T0)["fired"], false);
    set_clock(&clock, T0 + 60_000);
    assert_eq!(fire(&a, &id, T0 + 60_000)["fired"], false);
    assert_eq!(a.requests().len(), 2); // one tool turn + confirmation only, no poll model call
    assert!(schedule_messages(&a).is_empty());
    a.prompt("another topic");
    status("SUCCESS");
    set_clock(&clock, T0 + 120_000);
    assert_eq!(fire(&a, &id, T0 + 120_000)["fired"], true);
    a.events.wait_count("run_end", 3);
    assert_eq!(a.requests().len(), 4);
    assert_eq!(schedule_messages(&a).len(), 1);
    let messages = user_texts(a.requests().last().unwrap());
    let content = messages.last().unwrap().as_str().unwrap();
    assert!(content.contains("SUCCESS") && content.contains("PENDING"), "{content}");
    a.call(Op::ScheduleCancel { id }).unwrap();
}

/// A text answer whose request used `input` tokens.
fn text_at(answer: &str, input: u64) -> String {
    [
        json!({"type": "message_start", "message": {"id": "msg_t", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": answer}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

/// A fire whose prompt takes the context over the compaction threshold: the run compacts first, and the request
/// after it still ends in the schedule's prompt.
#[test]
fn session_core_schedule_fire_that_compacts_first_still_sends_its_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let prompt = format!("check the build {}", "x".repeat(2_000));
    let create = format!("sid = await schedule.every('1h', {prompt:?}, name='build')\nprint(sid)");
    // A 1,000,000-token window with 900,000 reserved: the threshold is 100,000. The answer ends just under it.
    let script = in_order(vec![
        tool("toolu_1", &create),
        text_at("Scheduled.", 99_900),
        text("## Goal\nsummary"),
        text("Build is green."),
    ]);
    let mut setup = setup(&clock);
    setup.settings = Some(json!({"compaction": {"reserveTokens": 900_000, "keepRecentTokens": 1},
        "refine": {"auto": false}}));
    let a = actor(script, setup);
    assert_eq!(a.prompt("watch the build")["error"], Value::Null);
    assert!(a.events("compaction_end").is_empty(), "compacted before the fire");
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    set_clock(&clock, T0 + HOUR + 5_000);
    assert_eq!(fire(&a, &id, T0 + HOUR)["fired"], true);
    a.events.wait_count("run_end", 2);
    assert_eq!(a.events("compaction_end").len(), 1, "{:?}", a.events("compaction_end"));
    let requests = a.requests();
    assert_eq!(requests.len(), 4);
    let last = requests[3]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user", "{last}");
    assert_eq!(user_texts(&requests[3]).last().unwrap(), &json!(prompt));
}
