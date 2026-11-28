//! Unit tests for the Antigravity provider.

#[cfg(test)]
use super::client::{Agy, Step, WorkspaceDir, normalize_usage, permitted, quota_banner_error};
use super::prompt::{delta_prompt, prompt, prompt_blocks};
use super::{sessions, stream_simple};
use crate::env::ProviderEnv;
use crate::options::SimpleStreamOptions;
use crate::types::{AssistantContent, AssistantMessage, Message, Model, StopReason, TranscriptContext};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

fn message(value: Value) -> Message {
    serde_json::from_value(value).unwrap()
}

#[test]
fn prompt_keeps_the_final_message_when_the_system_text_is_huge() {
    let big = "x".repeat(500_000);
    let mut messages = vec![message(json!({"role":"system","content":big,"timestamp":0}))];
    for i in 0..200 {
        messages.push(message(
            json!({"role":"user","content":format!("old {i} {}", "y".repeat(2_000)),"timestamp":0}),
        ));
    }
    messages.push(message(json!({"role":"user","content":"THE REQUEST","timestamp":0})));
    let text = prompt(&messages, 150_000).unwrap();
    assert!(text.len() < 151_000, "{}", text.len());
    assert!(text.contains("THE REQUEST"));
    assert!(text.contains("[system text cut to fit]"));
    assert!(text.contains("oldest messages were left out"));
}

#[test]
fn prompt_is_whole_when_it_fits() {
    let messages = vec![
        message(json!({"role":"system","content":"be brief","timestamp":0})),
        message(json!({"role":"user","content":"hi","timestamp":0})),
    ];
    let text = prompt(&messages, 150_000).unwrap();
    assert!(text.contains("be brief") && text.contains("hi"));
    assert!(!text.contains("cut to fit") && !text.contains("left out"));
}

#[test]
fn prompt_refuses_a_final_message_over_the_budget() {
    let messages = vec![message(
        json!({"role":"user","content":"z".repeat(200_000),"timestamp":0}),
    )];
    let error = prompt(&messages, 150_000)
        .map(|text| text.len())
        .unwrap_err()
        .to_string();
    assert!(error.contains("final message") && error.contains("150000"), "{error}");
}

fn fake_agy_options() -> SimpleStreamOptions {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fake-agy");
    let env = [
        ("PATH".to_string(), format!("{fixture}:/usr/bin:/bin")),
        ("ULTRON_AGY_SESSIONS".to_string(), "0".to_string()),
    ]
    .into_iter()
    .collect();
    let mut options = SimpleStreamOptions::default();
    options.base.env = Some(env);
    options
}

fn session_test_options() -> (tempfile::TempDir, SimpleStreamOptions) {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".gemini/config")).unwrap();
    let config = r#"{"mcpServers":{"ultron":{"command":"python3"}}}"#;
    std::fs::write(home.path().join(".gemini/config/mcp_config.json"), config).unwrap();
    let mut options = fake_agy_options();
    let env = options.base.env.as_mut().unwrap();
    env.insert("ULTRON_AGY_SESSIONS".into(), "1".into());
    env.insert("HOME".into(), home.path().display().to_string());
    (home, options)
}

async fn step_tool_call(
    model: &Model,
    messages: &mut Vec<Message>,
    options: &SimpleStreamOptions,
    output: &str,
) -> AssistantMessage {
    let reply = stream_simple(
        model,
        &TranscriptContext {
            messages: messages.clone(),
        },
        options,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(reply.stop_reason, StopReason::ToolUse, "{:?}", reply.error_message);
    let AssistantContent::ToolCall(call) = &reply.content[0] else {
        panic!()
    };
    messages.push(Message::Assistant(reply.clone()));
    messages.push(message(
        json!({"role":"toolResult","toolCallId":call.id,"toolName":"rlm","content":[{"type":"text","text":output}],"isError":false,"timestamp":0}),
    ));
    reply
}

fn tool_result(text: &str) -> Message {
    message(
        json!({"role":"toolResult","toolCallId":"c","toolName":"rlm","content":[{"type":"text","text":text}],"isError":false,"timestamp":0}),
    )
}

#[tokio::test]
async fn stream_simple_records_agy_usage() {
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let context = TranscriptContext {
        messages: vec![tool_result("one"), tool_result("two")],
    };
    let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let usage = &message.usage;
    assert_eq!((usage.input, usage.output, usage.reasoning), (12000, 47, Some(7)));
    assert_eq!(
        (usage.cache_read, usage.cache_write, usage.total_tokens),
        (9000, 100, 12047)
    );
}

#[tokio::test]
async fn turn_errors_carry_agy_stderr() {
    let model = crate::models::antigravity_model("offline-logged-out").unwrap();
    let context = TranscriptContext {
        messages: vec![tool_result("one")],
    };
    let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
        .await
        .unwrap();
    let error = message.error_message.unwrap_or_default();
    assert!(error.contains("agy login"), "{error}");
}

fn system(tools: Value) -> Message {
    message(json!({"role":"system","content":"SYSTEM RULES","toolsAdded":tools,"timestamp":0}))
}

fn rlm_tool() -> Value {
    json!([{"name":"rlm","description":"run a cell","parameters":{"type":"object"}}])
}

fn reply_text(message: &AssistantMessage) -> Value {
    match &message.content[..] {
        [AssistantContent::Text(text)] => serde_json::from_str(&text.text).unwrap(),
        other => panic!("{other:?} {:?}", message.error_message),
    }
}

#[test]
fn permissions_cover_the_inventory_in_both_agy_forms() {
    let names: HashSet<String> = ["rlm".to_string(), "read".to_string()].into();
    let allowed = [
        json!({"name":"mcp_ultron_read"}),
        json!({"title":"Run mcp_ultron_rlm"}),
        json!({"name":"call_mcp_tool","args":{"ServerName":"ultron","ToolName":"read","Arguments":{}}}),
    ];
    let refused = [
        json!({"name":"mcp_ultron_write"}),
        json!({"name":"mcp_other_rlm"}),
        json!({"name":"run_command","title":"Run rlm"}),
        json!({"name":"call_mcp_tool","args":{"ServerName":"other","ToolName":"rlm"}}),
        json!({"name":"call_mcp_tool","args":{"ServerName":"ultron","ToolName":"write"}}),
    ];
    assert!(allowed.iter().all(|tool| permitted(tool, &names)));
    assert!(refused.iter().all(|tool| !permitted(tool, &names)));
}

#[tokio::test]
async fn oversized_tool_schemas_are_an_error() {
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let big = json!([{"name":"rlm","description":"d".repeat(60_000),"parameters":{"type":"object"}}]);
    let context = TranscriptContext {
        messages: vec![
            system(big),
            message(json!({"role":"user","content":"hi","timestamp":0})),
        ],
    };
    let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
        .await
        .unwrap();
    let error = message.error_message.unwrap_or_default();
    assert!(error.contains("tool schemas") && error.contains("48000"), "{error}");
}

#[tokio::test]
async fn the_system_prompt_uses_append_when_advertised() {
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let context = TranscriptContext {
        messages: vec![
            system(json!([])),
            message(json!({"role":"user","content":"hi","timestamp":0})),
        ],
    };
    let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
        .await
        .unwrap();
    let report = reply_text(&message);
    assert_eq!(report["append"], json!("SYSTEM RULES"));
    assert_eq!(report["system"], false);
}

#[test]
fn sessions_are_the_default_and_write_only_ultrons_config_entry() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".gemini/config");
    std::fs::create_dir_all(&config).unwrap();
    let mine = r#"{"mcpServers":{"other":{"serverUrl":"http://x"}},"keep":1}"#;
    std::fs::write(config.join("mcp_config.json"), mine).unwrap();
    let env: ProviderEnv = [
        ("HOME".to_string(), home.path().display().to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
    ]
    .into();
    assert!(sessions::enabled(Some(&env)));
    let written: Value = serde_json::from_slice(&std::fs::read(config.join("mcp_config.json")).unwrap()).unwrap();
    assert_eq!(written["keep"], 1);
    assert_eq!(written["mcpServers"]["other"]["serverUrl"], "http://x");
    assert_eq!(written["mcpServers"]["ultron"]["timeoutSeconds"], -1);
    assert_eq!(
        written["mcpServers"]["ultron"]["tools"],
        json!({"rlm": {"eager": true, "background": "off"}})
    );
    let script = config.join("ultron-mcp-relay.py");
    assert_eq!(written["mcpServers"]["ultron"]["args"], json!([script]));
    assert!(
        std::fs::read_to_string(&script)
            .unwrap()
            .contains("ULTRON_AGY_BRIDGE_URL")
    );
    let mut off = env.clone();
    off.insert("ULTRON_AGY_SESSIONS".into(), "0".into());
    assert!(!sessions::enabled(Some(&off)));
    std::fs::write(config.join("mcp_config.json"), "[1]").unwrap();
    assert!(
        !sessions::enabled(Some(&env)),
        "a config Ultron cannot extend leaves session mode off"
    );
    assert_eq!(std::fs::read_to_string(config.join("mcp_config.json")).unwrap(), "[1]");
}

/// One agy serves a tool loop inside one turn, then a later prompt that carries only the new messages.
#[tokio::test]
async fn sessions_replay_after_agy_times_out_a_held_call() {
    let (home, mut options) = session_test_options();
    let flag = home.path().join("cancelled-once");
    options
        .base
        .env
        .as_mut()
        .unwrap()
        .insert("FAKE_AGY_CANCEL".into(), flag.display().to_string());
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let mut messages = vec![
        system(rlm_tool()),
        message(json!({"role":"user","content":"cancel case","timestamp":0})),
    ];
    for output in ["32", "olleh"] {
        step_tool_call(&model, &mut messages, &options, output).await;
        if output == "32" {
            assert!(flag.exists(), "agy timed out the first held call");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let last = stream_simple(&model, &TranscriptContext { messages }, &options, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(last.stop_reason, StopReason::Stop, "{:?}", last.error_message);
    assert_eq!(reply_text(&last)["echo"], json!([{"type":"text","text":"olleh"}]));
}

#[tokio::test]
async fn sessions_continue_one_agy_across_model_calls() {
    let (_home, options) = session_test_options();
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let mut messages = vec![
        system(rlm_tool()),
        message(json!({"role":"user","content":"go","timestamp":0})),
    ];
    for output in ["32", "olleh"] {
        let reply = step_tool_call(&model, &mut messages, &options, output).await;
        assert_eq!(reply.usage.input, 11000, "a tool-call step reports agy's context size");
    }
    let first = stream_simple(
        &model,
        &TranscriptContext {
            messages: messages.clone(),
        },
        &options,
        &mut |_| {},
    )
    .await
    .unwrap();
    let report = reply_text(&first);
    assert_eq!(report["echo"], json!([{"type":"text","text":"olleh"}]));
    assert_eq!(
        (
            report["append"].as_str(),
            report["system"].as_bool(),
            report["prompts"].as_u64()
        ),
        (Some("SYSTEM RULES"), Some(false), Some(1))
    );
    assert_eq!(first.usage.input, 12000);
    messages.push(Message::Assistant(first));
    messages.push(message(json!({"role":"user","content":"again","timestamp":0})));
    let second = stream_simple(
        &model,
        &TranscriptContext {
            messages: messages.clone(),
        },
        &options,
        &mut |_| {},
    )
    .await
    .unwrap();
    let again = reply_text(&second);
    assert_eq!(
        (again["pid"].clone(), again["prompts"].as_u64(), again["delta"].as_u64()),
        (report["pid"].clone(), Some(2), Some(1))
    );
    messages.insert(
        2,
        message(json!({"role":"user","content":"edited history","timestamp":0})),
    );
    let fresh = stream_simple(&model, &TranscriptContext { messages }, &options, &mut |_| {})
        .await
        .unwrap();
    assert_ne!(reply_text(&fresh)["pid"], report["pid"]);
}

#[tokio::test]
async fn sessions_keep_live_agy_when_user_or_runtime_event_arrives_mid_tool_call() {
    let (_home, options) = session_test_options();
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let mut messages = vec![
        system(rlm_tool()),
        message(json!({"role":"user","content":"go","timestamp":0})),
    ];
    for (i, output) in ["32", "olleh"].into_iter().enumerate() {
        step_tool_call(&model, &mut messages, &options, output).await;
        if i == 1 {
            messages.push(message(
                json!({"role":"user","content":"<runtime_event kind=\"cell_done\" id=\"cell-1\"/>","timestamp":0}),
            ));
        }
    }
    let done = stream_simple(
        &model,
        &TranscriptContext {
            messages: messages.clone(),
        },
        &options,
        &mut |_| {},
    )
    .await
    .unwrap();
    let report = reply_text(&done);
    assert_eq!(report["prompts"].as_u64(), Some(1), "agy must stay in one live turn");
    assert_eq!(
        report["echo"],
        json!([
            {"type": "text", "text": "olleh"},
            {"type": "text", "text": "[Message received while tool ran]\n<runtime_event kind=\"cell_done\" id=\"cell-1\"/>"}
        ])
    );
}

#[tokio::test]
async fn session_load_resumes_with_delta_prompt_and_ignores_replay_chunks() {
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let options = fake_agy_options();
    let messages = vec![
        system(rlm_tool()),
        message(json!({"role":"user","content":"follow up","timestamp":0})),
    ];
    let mut agy = Agy::resume(
        &model,
        &messages,
        options.base.env.as_ref(),
        "offline".into(),
        WorkspaceDir::Temp(tempfile::tempdir().unwrap()),
    )
    .await
    .unwrap();
    let prompt = delta_prompt(&messages[1..]).unwrap();
    let (content, _) = agy.advance(Step::Prompt(prompt), None).await.unwrap();
    let [AssistantContent::Text(text)] = &content[..] else {
        panic!("{content:?}");
    };
    assert!(!text.text.contains("HISTORICAL-REPLAY"), "{}", text.text);
    let report: Value = serde_json::from_str(&text.text).unwrap();
    assert_eq!(report["loaded"], true);
    assert_eq!(report["delta"], 1);
}

#[test]
fn prompt_blocks_extracts_images_as_native_acp_blocks() {
    let messages = vec![message(json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "describe this"},
            {"type": "image", "data": "QUJD", "mimeType": "image/png"}
        ],
        "timestamp": 0
    }))];
    let blocks = prompt_blocks(&messages, 150_000).unwrap();
    assert_eq!(blocks.len(), 2);
    let text = blocks[0]["text"].as_str().unwrap();
    assert!(text.contains("[attached image #1]"));
    assert!(text.contains("An MCP timeout or dropped tool wait does not mean"));
    assert!(!text.contains("QUJD"));
    assert_eq!(
        blocks[1],
        json!({"type": "image", "data": "QUJD", "mimeType": "image/png"})
    );
}

#[test]
fn prompt_blocks_strips_mime_parameters_on_image_attachments() {
    let messages = vec![message(json!({
        "role": "user",
        "content": [
            {"type": "image", "data": "QUJD", "mimeType": " image/png ; charset=binary"}
        ],
        "timestamp": 0
    }))];
    let blocks = prompt_blocks(&messages, 150_000).unwrap();
    assert_eq!(
        blocks[1],
        json!({"type": "image", "data": "QUJD", "mimeType": "image/png"})
    );
}

#[test]
fn normalize_usage_fills_missing_totals_and_falls_back_to_context_used() {
    let missing_total = normalize_usage(&json!({"usage": {"inputTokens": 500, "outputTokens": 25}}), 999);
    assert_eq!(
        (missing_total.input, missing_total.output, missing_total.total_tokens),
        (500, 25, 525)
    );

    let fallback = normalize_usage(&json!({"usage": {"outputTokens": 10}}), 4200);
    assert_eq!(
        (fallback.input, fallback.output, fallback.total_tokens),
        (4200, 10, 4210)
    );

    let cumulative = normalize_usage(
        &json!({"usage": {
            "inputTokens": 21_093,
            "cachedReadTokens": 494_396,
            "outputTokens": 29_442,
            "totalTokens": 544_931
        }}),
        161_267,
    );
    assert_eq!(
        (
            cumulative.input,
            cumulative.cache_read,
            cumulative.output,
            cumulative.total_tokens
        ),
        (6_599, 154_668, 29_442, 161_267)
    );
    assert_eq!(cumulative.input + cumulative.cache_read, 161_267);
}

#[tokio::test]
async fn session_resume_preferred() {
    let model = crate::models::antigravity_model("offline-test").unwrap();
    let mut options = fake_agy_options();
    options
        .base
        .env
        .as_mut()
        .unwrap()
        .insert("FAKE_AGY_RESUME".into(), "1".into());
    let messages = vec![
        system(rlm_tool()),
        message(json!({"role":"user","content":"follow up","timestamp":0})),
    ];
    let mut agy = Agy::resume(
        &model,
        &messages,
        options.base.env.as_ref(),
        "offline".into(),
        WorkspaceDir::Temp(tempfile::tempdir().unwrap()),
    )
    .await
    .unwrap();
    let prompt = delta_prompt(&messages[1..]).unwrap();
    let (content, _) = agy.advance(Step::Prompt(prompt), None).await.unwrap();
    let [AssistantContent::Text(text)] = &content[..] else {
        panic!("{content:?}");
    };
    let report: Value = serde_json::from_str(&text.text).unwrap();
    assert_eq!(report["resumed"], true);
    assert_eq!(report["loaded"], false);
}

#[tokio::test]
async fn quota_banner_returns_error() {
    let model = crate::models::antigravity_model("offline-quota").unwrap();
    let context = TranscriptContext {
        messages: vec![tool_result("one")],
    };
    let message = stream_simple(&model, &context, &fake_agy_options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.unwrap_or_default();
    assert!(error.contains("Individual quota reached"), "{error}");

    let nonzero = crate::types::Usage {
        input: 100,
        total_tokens: 100,
        ..crate::types::Usage::default()
    };
    assert!(quota_banner_error("⚠ Individual quota reached.", &nonzero).is_none());
}

#[test]
fn prompt_blocks_extracts_file_attachments_as_acp_embedded_resources() {
    let messages = vec![message(json!({
        "role": "user",
        "content": "<file name=\"/tmp/foo.rs\">\nfn main() {}\n</file>\nexplain this",
        "timestamp": 0
    }))];
    let blocks = prompt_blocks(&messages, 150_000).unwrap();
    assert_eq!(blocks.len(), 2);
    let text = blocks[0]["text"].as_str().unwrap();
    assert!(text.contains("[attached file #1: /tmp/foo.rs]"));
    assert!(!text.contains("fn main() {}"));
    assert_eq!(
        blocks[1],
        json!({
            "type": "resource",
            "resource": {
                "uri": "file:///tmp/foo.rs",
                "mimeType": "text/plain",
                "text": "fn main() {}"
            }
        })
    );
}
