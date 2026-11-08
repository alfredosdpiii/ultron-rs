//! Ported expectations of the TypeScript RPC tests: `rpc-jsonl.test.ts` (framing), the `RpcEventTranslator` case
//! of `ultron-native-rpc.test.ts`, `rpc-client-process-exit`,
//! `rpc-client-clear-queue` / `rpc-client-clone` (client).

use cli::rpc::client::{RpcClient, RpcClientOptions};
use cli::rpc::events::{RpcEventTranslator, frame_to_assistant_message_event, queue_update};
use cli::rpc::jsonl::{JsonlLines, serialize_json_line};
use protocol::watch::{QueueKind, WatchEvent};
use serde_json::{Value, json};

#[test]
fn cli_rpc_jsonl_keeps_unicode_separators() {
    let line = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}));
    assert!(line.contains("a\u{2028}b\u{2029}c"));
    assert!(line.ends_with('\n'));
    let lines: Vec<String> = JsonlLines::new(line.as_bytes()).map(Result::unwrap).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&lines[0]).unwrap(),
        json!({"text": "a\u{2028}b\u{2029}c"})
    );
}

#[test]
fn cli_rpc_jsonl_handles_crlf_and_final_line() {
    let lines: Vec<String> = JsonlLines::new(&b"{\"a\":1}\r\n{\"b\":2}\r\n"[..])
        .map(Result::unwrap)
        .collect();
    assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}"]);
    let lines: Vec<String> = JsonlLines::new(&b"{\"a\":1}"[..]).map(Result::unwrap).collect();
    assert_eq!(lines, ["{\"a\":1}"]);
    // Records are written like JSON.stringify: integral floats without a fraction.
    assert_eq!(
        serialize_json_line(&json!({"n": 3.0, "s": "x"})),
        "{\"n\":3,\"s\":\"x\"}\n"
    );
}

fn assistant() -> Value {
    json!({
        "role": "assistant", "content": [{"type": "text", "text": "ok"}], "api": "openai-completions",
        "provider": "mock", "model": "mock",
        "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "stop", "timestamp": 2,
    })
}

/// A lane-watch event from its JSON.
fn watch(event: Value) -> WatchEvent {
    WatchEvent::decode(&event).unwrap()
}

#[test]
fn cli_rpc_translator_reconstructs_turn_and_agent_boundaries() {
    let mut translator = RpcEventTranslator::default();
    let user = json!({"role": "user", "content": "hi", "timestamp": 1});
    let events = [
        watch(json!({"type": "run_start", "runId": "r1"})),
        watch(json!({"type": "message_start", "runId": "r1", "message": user})),
        watch(json!({"type": "message_end", "runId": "r1", "message": user})),
        watch(json!({"type": "message_start", "runId": "r1", "message": assistant()})),
        watch(
            json!({"type": "message_update", "runId": "r1", "usage": assistant()["usage"],
            "frame": {"type": "text_delta", "contentIndex": 0, "delta": "ok", "partial": assistant()}}),
        ),
        watch(json!({"type": "message_end", "runId": "r1", "message": assistant()})),
        watch(json!({"type": "run_end", "runId": "r1", "status": "completed", "endedAt": 1})),
    ];
    let lines: Vec<Value> = events.into_iter().flat_map(|e| translator.translate(e)).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "agent_start",
            "message_start",
            "message_end",
            "turn_start",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );
    assert_eq!(
        lines[5]["assistantMessageEvent"],
        json!({"type": "text_delta", "contentIndex": 0, "delta": "ok"})
    );
    assert_eq!(
        lines[7],
        json!({"type": "turn_end", "message": assistant(), "toolResults": []})
    );
    assert_eq!(lines[8]["messages"], json!([user, assistant()]));
    assert_eq!(lines[8]["willRetry"], false);
}

#[test]
fn cli_rpc_translator_reports_a_failed_run_as_an_error_message() {
    let mut translator = RpcEventTranslator::default();
    translator.translate(watch(json!({"type": "run_start", "runId": "r1"})));
    let lines = translator.translate(watch(
        json!({"type": "run_end", "runId": "r1", "status": "failed", "error": {"message": "No API key"}, "endedAt": 7}),
    ));
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["message_start", "message_end", "agent_end", "agent_settled"]);
    assert_eq!(lines[0]["message"]["errorMessage"], "No API key");
    assert_eq!(lines[0]["message"]["provider"], "ultron");
    assert_eq!(lines[0]["message"]["timestamp"], 7);
    assert_eq!(lines[2]["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn cli_rpc_translator_tool_and_queue_events() {
    let mut translator = RpcEventTranslator::default();
    translator.translate(watch(
        json!({"type": "tool_start", "toolCallId": "t1", "toolName": "rlm", "args": {"code": "1"}}),
    ));
    let update = translator.translate(watch(
        json!({"type": "tool_update", "toolCallId": "t1", "toolName": "rlm", "partialResult": {"content": []}}),
    ));
    assert_eq!(
        update[0],
        json!({"type": "tool_execution_update", "toolCallId": "t1", "toolName": "rlm", "args": {"code": "1"}, "partialResult": {"content": []}})
    );
    translator.translate(watch(
        json!({"type": "tool_end", "toolCallId": "t1", "toolName": "rlm", "result": {}, "isError": false}),
    ));
    // After the end, the arguments are forgotten.
    let late = translator.translate(watch(
        json!({"type": "tool_update", "toolCallId": "t1", "toolName": "rlm", "partialResult": null}),
    ));
    assert!(late[0].get("args").is_none());
    assert_eq!(
        queue_update(&[
            (QueueKind::Steer, "a".into()),
            (QueueKind::FollowUp, "b".into()),
            (QueueKind::NextRun, "c".into())
        ]),
        json!({"type": "queue_update", "steering": ["a"], "followUp": ["b", "c"]})
    );
    let end = translator.translate(watch(
        json!({"type": "compaction_end", "reason": "manual", "status": "failed", "error": {"message": "boom"}}),
    ));
    assert_eq!(
        end[0],
        json!({"type": "compaction_end", "reason": "manual", "aborted": false, "willRetry": false, "errorMessage": "boom"})
    );
}

#[test]
fn cli_rpc_frames_drop_partials() {
    let partial = json!({"content": [{"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {}}]});
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "toolcall_start", "contentIndex": 0, "partial": partial})),
        Some(json!({"type": "toolcall_start", "contentIndex": 0, "id": "c1", "toolName": "rlm"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "x"}}, "partial": partial})
        ),
        Some(
            json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "x"}}})
        )
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "toolcall_delta", "contentIndex": 0, "delta": "", "partial": partial})
        ),
        None
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "text_end", "contentIndex": 1, "content": "hi", "partial": {}})
        ),
        Some(json!({"type": "text_end", "contentIndex": 1, "content": "hi"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "start", "partial": {}})),
        Some(json!({"type": "start"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "done", "reason": "stop"})),
        None
    );
}

fn script_client(script: &str) -> (tempfile::TempDir, RpcClient) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.sh");
    std::fs::write(&path, script).unwrap();
    let client = RpcClient::start(&RpcClientOptions {
        program: "/bin/sh".into(),
        program_args: vec![path.display().to_string()],
        ..Default::default()
    })
    .unwrap();
    (dir, client)
}

#[test]
fn cli_rpc_client_rejects_a_request_when_the_process_exits() {
    let (_dir, mut client) = script_client("read line\nexit 43\n");
    let error = client.get_commands().unwrap_err();
    assert!(
        error.starts_with("Agent process exited (code=43 signal=null)"),
        "{error}"
    );
}

#[test]
fn cli_rpc_client_sends_clear_queue_and_clone() {
    // An agent that answers each command with a canned response carrying its id.
    let (_dir, mut client) = script_client(
        r#"while read line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *clear_queue*) printf '{"id":"%s","type":"response","command":"clear_queue","success":true,"data":{"steering":["Change direction"],"followUp":["Summarize when finished"]}}\n' "$id" ;;
    *clone*) printf '{"id":"%s","type":"response","command":"clone","success":true,"data":{"cancelled":false}}\n' "$id" ;;
    *) printf '{"id":"%s","type":"response","command":"x","success":false,"error":"nope"}\n' "$id" ;;
  esac
done
"#,
    );
    assert_eq!(
        client.clear_queue().unwrap(),
        json!({"steering": ["Change direction"], "followUp": ["Summarize when finished"]})
    );
    assert_eq!(client.clone_session().unwrap(), json!({"cancelled": false}));
    assert_eq!(client.get_state().unwrap_err(), "nope");
    let sent: Vec<&Value> = client.transcript.iter().collect();
    assert_eq!(sent[0]["id"], "req_1");
    assert_eq!(sent[1]["id"], "req_2");
    assert_eq!(client.stop(), Some(0));
}

#[test]
fn cli_rpc_watch_events_become_rpc_events() {
    let translate = |event: Value| RpcEventTranslator::default().translate(watch(event));
    // A message outside a run (a `bash` result) is not part of the next run's messages.
    let mut translator = RpcEventTranslator::default();
    translator.translate(watch(json!({"type": "run_start", "runId": "r1"})));
    translator.translate(watch(
        json!({"type": "message_end", "lane": "main", "message": {"role": "bashExecution"}}),
    ));
    translator.translate(watch(
        json!({"type": "message_end", "lane": "main", "runId": "r1", "message": {"role": "user"}}),
    ));
    let ended = translator.translate(watch(json!({"type": "run_end", "runId": "r1", "status": "completed"})));
    assert_eq!(ended[0]["messages"], json!([{"role": "user"}]));
    // AgentSession's summarization retry events pass through as they are, without the lane.
    assert_eq!(
        translate(
            json!({"type": "summarization_retry_scheduled", "lane": "main", "attempt": 1, "maxAttempts": 3,
            "delayMs": 2000, "errorMessage": "Overloaded"})
        ),
        [
            json!({"type": "summarization_retry_scheduled", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "Overloaded"})
        ]
    );
    // A run's retry is rpc-events.ts `auto_retry_start` / `auto_retry_end`; a success has no `finalError`.
    let scheduled = json!({"type": "retry_scheduled", "lane": "main", "runId": "r1", "step": "s0", "attempt": 1,
        "maxAttempts": 3, "delayMs": 2000, "errorMessage": "Overloaded"});
    let ended = |success: bool, error: Value| json!({"type": "retry_end", "lane": "main", "runId": "r1", "success": success, "attempt": 2, "finalError": error});
    assert_eq!(
        translate(scheduled),
        [
            json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "Overloaded"})
        ]
    );
    assert_eq!(
        translate(ended(true, Value::Null)),
        [json!({"type": "auto_retry_end", "success": true, "attempt": 2})]
    );
    assert_eq!(
        translate(ended(false, json!("Overloaded"))),
        [json!({"type": "auto_retry_end", "success": false, "attempt": 2, "finalError": "Overloaded"})]
    );
    // Only a failed run carries its error; an aborted one ends without.
    let failed = translate(json!({"type": "run_end", "status": "failed", "error": {"message": "boom"}, "endedAt": 5}));
    assert_eq!(failed[0]["message"]["errorMessage"], "boom");
    assert_eq!(failed[0]["message"]["timestamp"], 5);
    let aborted = translate(json!({"type": "run_end", "status": "aborted", "endedAt": 6}));
    assert_eq!(aborted[0]["type"], "agent_end");
    // A tool call's start frame names the call; queue items show their text; the name event is not RPC's.
    let start = json!({"type": "message_update", "lane": "main", "runId": "r1", "message": {"role": "assistant", "usage": {}},
        "frame": {"type": "toolcall_start", "contentIndex": 0, "id": "t1", "toolName": "rlm"}});
    assert_eq!(
        translate(start)[0]["assistantMessageEvent"],
        json!({"type": "toolcall_start", "contentIndex": 0, "id": "t1", "toolName": "rlm"})
    );
    let queued = json!({"type": "queue_update", "lane": "main", "queues": [
        {"type": "message", "kind": "followUp", "entryId": "e1", "message": {"role": "user", "content": [{"type": "text", "text": "later"}]}}
    ]});
    assert_eq!(
        translate(queued),
        [json!({"type": "queue_update", "steering": [], "followUp": ["later"]})]
    );
    assert!(translate(json!({"type": "session_name", "name": "x"})).is_empty());
}
