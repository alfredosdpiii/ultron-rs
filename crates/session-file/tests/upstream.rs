//! Ported expectations from packages/coding-agent/test/session-manager/*.test.ts and
//! test/session-context-edit.test.ts (format behavior).

use std::fs;

use serde_json::{Value, json};
use session_file::{
    Env, Leaf, NewSessionOptions, SessionManager, SystemEnv, build_context_entries, build_session_context,
    find_most_recent_session, list_all_sessions, list_sessions, load_entries_from_file, migrate_session_entries,
};

/// Real clock and randomness; the agent dir is a temp dir, never the user's.
struct TestEnv(String);

impl Env for TestEnv {
    fn now_ms(&mut self) -> i64 {
        SystemEnv.now_ms()
    }
    fn random_uuid(&mut self) -> String {
        SystemEnv.random_uuid()
    }
    fn session_id(&mut self) -> String {
        SystemEnv.session_id()
    }
    fn agent_dir(&self) -> String {
        self.0.clone()
    }
    fn home(&self) -> String {
        session_file::SystemEnv.home()
    }
    fn cwd(&self) -> String {
        session_file::SystemEnv.cwd()
    }
}

fn env() -> Box<dyn Env> {
    Box::new(TestEnv("/nonexistent-agent-dir".into()))
}

fn mem() -> SessionManager {
    SessionManager::in_memory(env(), "/project", None, None).unwrap()
}

fn tmp() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().into_owned();
    (dir, path)
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text, "timestamp": 1 })
}

fn assistant(text: &str) -> Value {
    json!({
        "role": "assistant", "content": [{ "type": "text", "text": text }], "api": "anthropic-messages",
        "provider": "anthropic", "model": "claude-test",
        "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
        "stopReason": "stop", "timestamp": 1,
    })
}

fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    json!({ "type": "message", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "message": if role == "user" { user(text) } else { assistant(text) } })
}

fn compaction(id: &str, parent: &str, summary: &str, first_kept: &str) -> Value {
    json!({ "type": "compaction", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "summary": summary, "firstKeptEntryId": first_kept, "tokensBefore": 1000 })
}

fn branch_summary(id: &str, parent: &str, summary: &str, from: &str) -> Value {
    json!({ "type": "branch_summary", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "summary": summary, "fromId": from })
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn roles(messages: &[Value]) -> Vec<&str> {
    messages.iter().map(|m| m["role"].as_str().unwrap()).collect()
}

fn id(v: &str) -> Value {
    Value::from(v)
}

// ---- build-context.test.ts ----

#[test]
fn session_file_context_of_empty_entries_is_empty_off_and_modelless() {
    let ctx = build_session_context(&[], Leaf::Last);
    assert_eq!(ctx.messages, Vec::<Value>::new());
    assert_eq!(ctx.thinking_level, Some(json!("off")));
    assert_eq!(ctx.model, None);
}

#[test]
fn session_file_context_tracks_thinking_and_assistant_model_over_model_change() {
    let entries = [
        msg("1", None, "user", "hello"),
        json!({ "type": "thinking_level_change", "id": "2", "parentId": "1", "timestamp": "t", "thinkingLevel": "high" }),
        json!({ "type": "model_change", "id": "3", "parentId": "2", "timestamp": "t", "provider": "openai", "modelId": "gpt-4" }),
        msg("4", Some("3"), "assistant", "hi"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.thinking_level, Some(json!("high")));
    assert_eq!(roles(&ctx.messages), ["user", "assistant"]);
    assert_eq!(
        ctx.model,
        Some(json!({ "provider": "anthropic", "modelId": "claude-test" }))
    );
    let ctx = build_session_context(&entries[..3], Leaf::Last);
    assert_eq!(ctx.model, Some(json!({ "provider": "openai", "modelId": "gpt-4" })));
}

#[test]
fn session_file_compaction_puts_summary_before_kept_messages() {
    let entries = [
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response1"),
        msg("3", Some("2"), "user", "second"),
        msg("4", Some("3"), "assistant", "response2"),
        compaction("5", "4", "Summary of first two turns", "3"),
        msg("6", Some("5"), "user", "third"),
        msg("7", Some("6"), "assistant", "response3"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.messages.len(), 5);
    assert_eq!(ctx.messages[0]["summary"], "Summary of first two turns");
    assert_eq!(ctx.messages[0]["tokensBefore"], 1000);
    assert_eq!(ctx.messages[0]["timestamp"], 1_735_689_600_000_i64);
    assert_eq!(
        ctx.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["second", "response2", "third", "response3"]
    );
}

#[test]
fn session_file_multiple_compactions_use_the_latest() {
    let entries = [
        msg("1", None, "user", "a"),
        msg("2", Some("1"), "assistant", "b"),
        compaction("3", "2", "First summary", "1"),
        msg("4", Some("3"), "user", "c"),
        msg("5", Some("4"), "assistant", "d"),
        compaction("6", "5", "Second summary", "4"),
        msg("7", Some("6"), "user", "e"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.messages.len(), 4);
    assert_eq!(ctx.messages[0]["summary"], "Second summary");
}

#[test]
fn session_file_context_entries_keep_custom_entries_but_messages_skip_them() {
    let custom = |id: &str, parent: &str| json!({ "type": "custom", "id": id, "parentId": parent, "timestamp": "t", "customType": "c", "data": {} });
    let entries = [
        msg("1", None, "user", "first"),
        custom("2", "1"),
        msg("3", Some("2"), "assistant", "response1"),
        custom("4", "3"),
        msg("5", Some("4"), "user", "second"),
        compaction("6", "5", "Summary", "4"),
        custom("7", "6"),
        msg("8", Some("7"), "assistant", "response2"),
    ];
    let ids: Vec<&Value> = build_context_entries(&entries, Leaf::Last)
        .iter()
        .map(|e| &e["id"])
        .collect();
    assert_eq!(ids, [&id("6"), &id("4"), &id("5"), &id("7"), &id("8")]);
    assert_eq!(
        roles(&build_session_context(&entries, Leaf::Last).messages),
        ["compactionSummary", "user", "assistant"]
    );
}

#[test]
fn session_file_context_follows_the_selected_branch_with_branch_summaries() {
    let entries = [
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "r1"),
        msg("3", Some("2"), "user", "q2"),
        msg("4", Some("3"), "assistant", "r2"),
        compaction("5", "4", "Compacted history", "3"),
        msg("6", Some("5"), "user", "q3"),
        msg("7", Some("6"), "assistant", "r3"),
        msg("8", Some("3"), "user", "wrong path"),
        msg("9", Some("8"), "assistant", "wrong response"),
        branch_summary("10", "3", "Tried wrong approach", "9"),
        msg("11", Some("10"), "user", "better approach"),
    ];
    let main = build_session_context(&entries, Leaf::Id(&id("7")));
    assert_eq!(main.messages[0]["summary"], "Compacted history");
    assert_eq!(
        main.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["q2", "r2", "q3", "r3"]
    );
    let branch = build_session_context(&entries, Leaf::Id(&id("11")));
    assert_eq!(
        roles(&branch.messages),
        ["user", "assistant", "user", "branchSummary", "user"]
    );
    assert_eq!(
        branch.messages[3],
        json!({ "role": "branchSummary", "summary": "Tried wrong approach", "fromId": "9", "timestamp": 1_735_689_600_000_i64 })
    );
    assert_eq!(text(&branch.messages[4]), "better approach");
}

#[test]
fn session_file_unknown_leaf_uses_last_entry_and_orphans_stand_alone() {
    let entries = [msg("1", None, "user", "hello"), msg("2", Some("1"), "assistant", "hi")];
    assert_eq!(
        build_session_context(&entries, Leaf::Id(&id("nonexistent")))
            .messages
            .len(),
        2
    );
    assert_eq!(build_session_context(&entries, Leaf::Root).messages.len(), 0);
    let orphan = [
        msg("1", None, "user", "hello"),
        msg("2", Some("missing"), "assistant", "orphan"),
    ];
    assert_eq!(build_session_context(&orphan, Leaf::Id(&id("2"))).messages.len(), 1);
}

// ---- migration.test.ts ----

#[test]
fn session_file_v1_entries_get_ids_parent_chain_and_version_3() {
    let mut entries = vec![
        json!({ "type": "session", "id": "sess-1", "timestamp": "2025-01-01T00:00:00Z", "cwd": "/tmp" }),
        json!({ "type": "message", "timestamp": "2025-01-01T00:00:01Z", "message": user("hi") }),
        json!({ "type": "message", "timestamp": "2025-01-01T00:00:02Z", "message": assistant("hello") }),
    ];
    let mut e = TestEnv(String::new());
    assert!(migrate_session_entries(&mut entries, &mut e));
    assert_eq!(entries[0]["version"], 3);
    assert_eq!(entries[1]["id"].as_str().unwrap().len(), 8);
    assert_eq!(entries[1]["parentId"], Value::Null);
    assert_eq!(entries[2]["id"].as_str().unwrap().len(), 8);
    assert_eq!(entries[2]["parentId"], entries[1]["id"]);
}

#[test]
fn session_file_migration_skips_current_entries() {
    let mut entries = vec![
        json!({ "type": "session", "id": "sess-1", "version": 2, "timestamp": "t", "cwd": "/tmp" }),
        json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "t", "message": user("hi") }),
        json!({ "type": "message", "id": "def67890", "parentId": "abc12345", "timestamp": "t", "message": assistant("x") }),
    ];
    migrate_session_entries(&mut entries, &mut TestEnv(String::new()));
    assert_eq!(entries[1]["id"], "abc12345");
    assert_eq!(entries[2]["id"], "def67890");
    assert_eq!(entries[2]["parentId"], "abc12345");
    assert!(!migrate_session_entries(&mut entries, &mut TestEnv(String::new())));
}

// ---- save-entry.test.ts, labels.test.ts, tree-traversal.test.ts ----

/// A Pi session's entries written by hand (the writer API beyond `appendMessage` is not ported: nothing in the
/// product writes a Pi session): each entry a child of the one before, `e<n>` ids, timestamps one second apart.
#[derive(Default)]
struct Chain(Vec<Value>);

impl Chain {
    fn push(&mut self, kind: &str, fields: Value) -> String {
        let n = self.0.len() + 1;
        let id = format!("e{n}");
        let parent = self.0.last().map_or(Value::Null, |e| e["id"].clone());
        let mut entry = json!({ "type": kind, "id": id, "parentId": parent,
            "timestamp": format!("2025-01-01T00:00:{n:02}.000Z") });
        entry
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        self.0.push(entry);
        id
    }

    fn message(&mut self, message: Value) -> String {
        self.push("message", json!({ "message": message }))
    }

    fn edit(&mut self, target: &str, replacement: Value) -> String {
        self.push(
            "context_edit",
            json!({ "targetId": target, "replacement": replacement }),
        )
    }

    fn compaction(&mut self, summary: &str, first_kept: Option<&str>) -> String {
        let id = format!("e{}", self.0.len() + 1);
        let first_kept = first_kept.unwrap_or(&id).to_string();
        self.push(
            "compaction",
            json!({ "summary": summary, "firstKeptEntryId": first_kept, "tokensBefore": 100 }),
        )
    }

    fn manager(&self) -> SessionManager {
        SessionManager::in_memory(env(), "/project", None, Some(self.0.clone())).unwrap()
    }
}

#[test]
fn session_file_custom_entries_join_the_tree_but_not_the_context() {
    let mut chain = Chain::default();
    let m1 = chain.message(user("hello"));
    let c = chain.push("custom", json!({ "customType": "my_data", "data": { "foo": "bar" } }));
    let m2 = chain.message(assistant("hi"));
    let s = chain.manager();
    let custom = s.entry(&c).unwrap();
    assert_eq!(custom["customType"], "my_data");
    assert_eq!(custom["data"], json!({ "foo": "bar" }));
    assert_eq!(custom["parentId"], Value::from(m1.as_str()));
    let path: Vec<&Value> = s.branch_entries(None).iter().map(|e| &e["id"]).collect();
    assert_eq!(path, [&id(&m1), &id(&c), &id(&m2)]);
    assert_eq!(s.build_session_context().messages.len(), 2);
}
#[test]
fn session_file_labels_set_clear_and_last_wins_with_timestamps() {
    let mut chain = Chain::default();
    let m = chain.message(user("hello"));
    assert_eq!(chain.manager().label(&m), None);
    let label = |label: Value| json!({ "targetId": m, "label": label });
    chain.push("label", label(json!("checkpoint")));
    assert_eq!(chain.manager().label(&m), Some(&json!("checkpoint")));
    chain.push("label", label(Value::Null));
    assert_eq!(chain.manager().label(&m), None);
    chain.push("label", label(json!("first")));
    let last = chain.push("label", label(json!("third")));
    let s = chain.manager();
    assert_eq!(s.label(&m), Some(&json!("third")));
    let tree = s.tree();
    let node = &tree.nodes[tree.roots[0]];
    assert_eq!(node.label_timestamp, s.entry(&last).map(|e| &e["timestamp"]));
    assert_eq!(s.build_session_context().messages.len(), 1);
}
#[test]
fn session_file_branch_moves_leaf_and_new_appends_fork_the_tree() {
    let mut s = mem();
    assert!(s.branch_entries(None).is_empty());
    assert!(s.tree().roots.is_empty());
    let a = s.append_message(user("1")).unwrap();
    s.append_message(assistant("2")).unwrap();
    s.append_message(user("3")).unwrap();
    s.branch(&a).unwrap();
    assert_eq!(s.leaf_id(), Some(&id(&a)));
    let d = s.append_message(user("4")).unwrap();
    assert_eq!(s.entry(&d).unwrap()["parentId"], Value::from(a.as_str()));
    let tree = s.tree();
    assert_eq!(tree.roots.len(), 1);
    assert_eq!(tree.nodes[tree.roots[0]].children.len(), 2);
    assert_eq!(
        s.branch("nonexistent").unwrap_err().to_string(),
        "Entry nonexistent not found"
    );
}

// ---- load-entries.test.ts ----

#[test]
fn session_file_preloaded_entries_are_adopted_with_leaf_labels_and_tree() {
    let stored = vec![
        msg("m1", None, "user", "hello"),
        msg("m2", Some("m1"), "user", "abandoned"),
        json!({ "type": "label", "id": "l1", "parentId": "m1", "timestamp": "2025-01-01T00:00:01Z",
            "targetId": "m1", "label": "checkpoint" }),
        msg("m3", Some("l1"), "user", "kept"),
    ];
    let mut s = SessionManager::in_memory(env(), "/project", None, Some(stored.clone())).unwrap();
    assert_eq!(s.entries().into_iter().cloned().collect::<Vec<_>>(), stored);
    assert_eq!(s.label("m1"), Some(&json!("checkpoint")));
    assert_eq!(s.tree().nodes[s.tree().roots[0]].children.len(), 2);
    let appended = s.append_message(user("continued")).unwrap();
    assert_eq!(s.entry(&appended).unwrap()["parentId"], json!("m3"));
    assert!(!stored.iter().any(|e| e["id"] == appended.as_str()));
    assert_eq!(s.session_file(), None);
}
#[test]
fn session_file_preloaded_headers_set_identity_and_migrate() {
    let opts = |id: &str| {
        Some(NewSessionOptions {
            id: Some(id.into()),
            parent_session: None,
        })
    };
    let body = vec![
        json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "t",
        "message": { "role": "hookMessage", "content": "from a hook", "timestamp": 1 } }),
    ];
    let headerless =
        SessionManager::in_memory(env(), "/project", opts("restored-session"), Some(body.clone())).unwrap();
    assert_eq!(headerless.session_id(), "restored-session");
    assert_eq!(headerless.header().unwrap()["cwd"], "/project");
    assert_eq!(headerless.entries()[0]["message"]["role"], "hookMessage");
    let mut with_header =
        vec![json!({ "type": "session", "version": 2, "id": "stored-session", "timestamp": "t", "cwd": "/stored" })];
    with_header.extend(body);
    let s = SessionManager::in_memory(env(), "/project", opts("ignored"), Some(with_header)).unwrap();
    assert_eq!(s.session_id(), "stored-session");
    assert_eq!(s.header().unwrap()["cwd"], "/stored");
    assert_eq!(s.header().unwrap()["version"], 3);
    assert_eq!(s.entries()[0]["message"]["role"], "custom");
    assert_eq!(s.entries()[0]["id"], "abc12345");
    let empty = SessionManager::in_memory(env(), "/project", opts("empty-session"), Some(vec![])).unwrap();
    assert_eq!(empty.session_id(), "empty-session");
    assert!(empty.entries().is_empty());
    assert_eq!(empty.leaf_id(), None);
}

// ---- custom-session-id.test.ts ----

fn is_uuid_v7(id: &str) -> bool {
    let b = id.as_bytes();
    id.len() == 36
        && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b[14] == b'7'
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
        && id
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[test]
fn session_file_session_ids_are_validated_or_uuid_v7() {
    let mut s = mem();
    assert!(is_uuid_v7(s.session_id()));
    assert_eq!(s.header().unwrap()["id"], Value::from(s.session_id()));
    for good in ["my-custom-id", "abc-123_def.456"] {
        s.new_session(Some(NewSessionOptions {
            id: Some(good.into()),
            parent_session: None,
        }))
        .unwrap();
        assert_eq!(s.session_id(), good);
    }
    for bad in [
        "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
    ] {
        let err = s
            .new_session(Some(NewSessionOptions {
                id: Some(bad.into()),
                parent_session: None,
            }))
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Session id must be non-empty, contain only alphanumeric characters"),
            "{bad}"
        );
    }
    s.new_session(Some(NewSessionOptions {
        id: None,
        parent_session: Some("parent.jsonl".into()),
    }))
    .unwrap();
    assert!(is_uuid_v7(s.session_id()));
    assert_eq!(s.header().unwrap()["parentSession"], "parent.jsonl");
}

#[test]
fn session_file_created_and_forked_files_are_named_timestamp_underscore_id() {
    let (_dir, path) = tmp();
    let opts = Some(NewSessionOptions {
        id: Some("created-session-id".into()),
        parent_session: None,
    });
    let s = SessionManager::create(env(), &path, Some(&path), opts).unwrap();
    let file = s.session_file().unwrap().rsplit('/').next().unwrap().to_string();
    assert!(
        file.ends_with("Z_created-session-id.jsonl")
            && file.len() == "2025-01-01T00-00-00-000Z_created-session-id.jsonl".len()
    );
    assert!(!fs::exists(s.session_file().unwrap()).unwrap());
    let source = format!("{path}/source.jsonl");
    fs::write(
        &source,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"src\",\"timestamp\":\"t\",\"cwd\":\"{path}\"}}\n"),
    )
    .unwrap();
    let forked = SessionManager::fork_from(env(), &source, &path, Some(&path), None).unwrap();
    assert!(is_uuid_v7(forked.header().unwrap()["id"].as_str().unwrap()));
    assert_eq!(forked.header().unwrap()["parentSession"], Value::from(source.as_str()));
}

// ---- file-operations.test.ts ----

#[test]
fn session_file_load_entries_repairs_only_session_files() {
    let (_dir, path) = tmp();
    let file = |name: &str, content: &str| {
        let p = format!("{path}/{name}");
        fs::write(&p, content).unwrap();
        p
    };
    assert!(
        load_entries_from_file(&SystemEnv, &format!("{path}/nonexistent.jsonl"))
            .unwrap()
            .is_empty()
    );
    assert!(
        load_entries_from_file(&SystemEnv, &file("empty.jsonl", ""))
            .unwrap()
            .is_empty()
    );
    assert!(
        load_entries_from_file(
            &SystemEnv,
            &file("no-header.jsonl", "{\"type\":\"message\",\"id\":\"1\"}\n")
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        load_entries_from_file(&SystemEnv, &file("malformed.jsonl", "not json\n"))
            .unwrap()
            .is_empty()
    );
    let header = "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n";
    let message = "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}";
    let mixed = load_entries_from_file(
        &SystemEnv,
        &file("mixed.jsonl", &format!("{header}not valid json\n{message}\n")),
    )
    .unwrap();
    assert_eq!(mixed.len(), 2);
    let unterminated = file("unterminated.jsonl", &format!("{header}{message}"));
    assert_eq!(load_entries_from_file(&SystemEnv, &unterminated).unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(&unterminated).unwrap(),
        format!("{header}{message}\n")
    );
    let tail = file("malformed-tail.jsonl", &format!("{header}{{\"type\":\"message\""));
    assert_eq!(load_entries_from_file(&SystemEnv, &tail).unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(&tail).unwrap(),
        format!("{header}{{\"type\":\"message\"\n")
    );
    let invalid = file("invalid.jsonl", "{\"type\":\"message\",\"id\":\"1\"}");
    assert!(load_entries_from_file(&SystemEnv, &invalid).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(&invalid).unwrap(),
        "{\"type\":\"message\",\"id\":\"1\"}"
    );
}

#[test]
fn session_file_open_reads_cwd_past_blank_and_malformed_prefixes_and_scan_limit() {
    let (_dir, path) = tmp();
    let stored = format!("{path}/stored-project");
    let write = |name: &str, prefix: &str, id: &str| {
        let p = format!("{path}/{name}.jsonl");
        fs::write(&p, format!("{prefix}{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"{stored}\"}}\n")).unwrap();
        p
    };
    let big = "a".repeat(1024 * 1024 + 1);
    let cases = [
        write("blank", "\n  \n", "leading-blank"),
        write("malformed", "not json\n{broken json\n", "leading-malformed"),
        write("multi", "", &"a".repeat(8192)),
        write("large-header", "", &big),
        write(
            "large-prefix",
            &format!("{}\n", "x".repeat(1024 * 1024 + 1)),
            "large-prefix",
        ),
    ];
    let ids = [
        "leading-blank",
        "leading-malformed",
        &"a".repeat(8192),
        &big,
        "large-prefix",
    ];
    for (file, id) in cases.iter().zip(ids) {
        for cwd_override in [None, Some(format!("{path}/override"))] {
            let s = SessionManager::open(env(), file, Some(&path), cwd_override.as_deref()).unwrap();
            assert_eq!(s.session_id(), id);
            assert_eq!(s.cwd(), cwd_override.unwrap_or_else(|| stored.clone()));
        }
    }
}

#[test]
fn session_file_open_initializes_empty_files_and_refuses_other_content() {
    let (_dir, path) = tmp();
    let empty = format!("{path}/empty.jsonl");
    fs::write(&empty, "").unwrap();
    let s = SessionManager::open(env(), &empty, Some(&path), None).unwrap();
    assert_eq!(s.session_file(), Some(empty.as_str()));
    let lines: Vec<Value> = fs::read_to_string(&empty)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["id"], Value::from(s.session_id()));
    let again = SessionManager::open(env(), &empty, Some(&path), None).unwrap();
    assert_eq!(again.session_id(), s.session_id());
    for (name, content) in [
        (
            "no-header.jsonl",
            "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n",
        ),
        ("not-a-session.log", "{\"type\":\"event\",\"data\":\"not a session\"}\n"),
    ] {
        let file = format!("{path}/{name}");
        fs::write(&file, content).unwrap();
        let err = SessionManager::open(env(), &file, Some(&path), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Session file is not a valid ultron session: {file}")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }
}

fn set_mtime(path: &str, ms: u64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[test]
fn session_file_most_recent_session_skips_invalid_files_and_filters_by_cwd() {
    let (_dir, path) = tmp();
    assert_eq!(find_most_recent_session(&SystemEnv, &path, None), None);
    assert_eq!(
        find_most_recent_session(&SystemEnv, &format!("{path}/nonexistent"), None),
        None
    );
    fs::write(format!("{path}/file.txt"), "hello").unwrap();
    fs::write(format!("{path}/invalid.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    fs::write(format!("{path}/oversized.jsonl"), "x".repeat(1024 * 1024 + 1)).unwrap();
    assert_eq!(find_most_recent_session(&SystemEnv, &path, None), None);
    let write = |name: &str, cwd: &str, mtime: u64| {
        let p = format!("{path}/{name}");
        fs::write(
            &p,
            format!("{{\"type\":\"session\",\"id\":\"{name}\",\"timestamp\":\"t\",\"cwd\":\"{cwd}\"}}\n"),
        )
        .unwrap();
        set_mtime(&p, mtime);
        p
    };
    let a = write("a.jsonl", &format!("{path}/project-a"), 1_000_000);
    let b = write("b.jsonl", &format!("{path}/project-b"), 2_000_000);
    set_mtime(&format!("{path}/invalid.jsonl"), 3_000_000);
    assert_eq!(find_most_recent_session(&SystemEnv, &path, None), Some(b.clone()));
    assert_eq!(
        find_most_recent_session(&SystemEnv, &path, Some(&format!("{path}/project-a"))),
        Some(a)
    );
    assert_eq!(
        find_most_recent_session(&SystemEnv, &path, Some(&format!("{path}/project-b"))),
        Some(b)
    );
}

#[test]
fn session_file_flat_dir_lists_by_cwd() {
    let (_dir, path) = tmp();
    let make = |cwd: &str, label: &str| {
        let mut s = SessionManager::create(env(), cwd, Some(&path), None).unwrap();
        s.append_message(user(label)).unwrap();
        s.append_message(assistant(&format!("reply to {label}"))).unwrap();
        s.session_file().unwrap().to_string()
    };
    let a = make(&format!("{path}/project-a"), "from A");
    let b = make(&format!("{path}/project-b"), "from B");
    set_mtime(&a, 1_000_000);
    set_mtime(&b, 2_000_000);
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let current: Vec<String> = list_sessions(&env_box, &format!("{path}/project-a"), Some(&path))
        .unwrap()
        .into_iter()
        .map(|s| s.path)
        .collect();
    assert_eq!(current, std::slice::from_ref(&a));
    let mut all: Vec<String> = list_all_sessions(&env_box, Some(&path))
        .into_iter()
        .map(|s| s.path)
        .collect();
    all.sort();
    let mut expected = vec![a.clone(), b];
    expected.sort();
    assert_eq!(all, expected);
}

#[test]
fn session_file_session_info_modified_uses_last_message_timestamp() {
    let (_dir, path) = tmp();
    let file = format!("{path}/s.jsonl");
    fs::write(&file, "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    let mut s = SessionManager::open(env(), &file, None, None).unwrap();
    let mut message = assistant("later");
    message["timestamp"] = json!(1_700_000_123_456_i64);
    s.append_message(message).unwrap();
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let info = list_sessions(&env_box, "/tmp", Some(&path)).unwrap();
    assert_eq!(info[0].modified, Some(1_700_000_123_456));
    assert_eq!(info[0].created, Some(1_735_689_600_000));
}

// ---- session-context-edit.test.ts ----

fn tool_result(text: &str) -> Value {
    json!({ "role": "toolResult", "toolCallId": "call-1", "toolName": "read", "content": [{ "type": "text", "text": text }],
        "details": { "path": "large.txt" }, "isError": true, "timestamp": 1 })
}

#[test]
fn session_file_context_edits_omit_replace_and_latest_wins() {
    let mut chain = Chain::default();
    chain.message(user("request"));
    let a = chain.message(assistant("partial"));
    let r = chain.message(tool_result("raw output"));
    chain.edit(&a, Value::Null);
    chain.edit(&r, Value::Null);
    let s = chain.manager();
    assert_eq!(
        s.branch_entries(None).iter().filter(|e| e["type"] == "message").count(),
        3
    );
    assert_eq!(roles(&s.build_session_projection().messages), ["user"]);
    assert_eq!(s.entry(&r).unwrap()["message"], tool_result("raw output"));

    let mut chain = Chain::default();
    let t = chain.message(assistant("original"));
    chain.edit(&t, json!({ "content": [{ "type": "text", "text": "first" }] }));
    chain.edit(&t, Value::Null);
    chain.edit(&t, json!({ "content": [{ "type": "text", "text": "restored" }] }));
    let s = chain.manager();
    let projected = &s.build_session_projection().messages[0];
    assert_eq!(text(projected), "restored");
    assert_eq!(projected["usage"]["totalTokens"], 2);
    assert_eq!(text(&s.entry(&t).unwrap()["message"]), "original");
}
#[test]
fn session_file_edits_are_branch_relative_and_apply_to_retained_entries() {
    let mut chain = Chain::default();
    let t = chain.message(user("original"));
    chain.edit(&t, json!({ "content": "edited" }));
    let mut s = chain.manager();
    assert_eq!(text(&s.build_session_projection().messages[0]), "edited");
    s.branch(&t).unwrap();
    assert_eq!(text(&s.build_session_projection().messages[0]), "original");

    let mut chain = Chain::default();
    chain.message(user("summarized"));
    let kept = chain.message(user("original retained"));
    chain.compaction("summary", Some(&kept));
    chain.edit(&kept, json!({ "content": "edited retained" }));
    let shown: Vec<String> = chain
        .manager()
        .build_session_projection()
        .messages
        .iter()
        .map(|m| {
            m.get("summary")
                .and_then(Value::as_str)
                .map_or_else(|| text(m), str::to_string)
        })
        .collect();
    assert_eq!(shown, ["summary", "edited retained"]);
}
#[test]
fn session_file_retain_none_and_repeated_compactions_show_only_newest_summary() {
    let mut chain = Chain::default();
    chain.message(user("discarded"));
    chain.compaction("exact handoff", None);
    chain.message(user("after"));
    let shown: Vec<String> = chain
        .manager()
        .build_session_projection()
        .messages
        .iter()
        .map(|m| {
            m.get("summary")
                .and_then(Value::as_str)
                .map_or_else(|| text(m), str::to_string)
        })
        .collect();
    assert_eq!(shown, ["exact handoff", "after"]);

    let mut chain = Chain::default();
    chain.message(user("summarized first"));
    let kept = chain.message(user("retained"));
    chain.compaction("first summary", Some(&kept));
    chain.message(assistant("after first compaction"));
    chain.compaction("second summary", Some(&kept));
    let summaries: Vec<Value> = chain
        .manager()
        .build_session_projection()
        .messages
        .iter()
        .filter_map(|m| m.get("summary").cloned())
        .collect();
    assert_eq!(summaries, [json!("second summary")]);
}
#[test]
fn session_file_listing_orders_names_by_locale_compare() {
    // `files.sort((a, b) => b.localeCompare(a))`: ICU sorts "_" before "-" ("a_b".localeCompare("a-b") === -1 in
    // node), so descending is a-b, a_b; the old byte-order stand-in gave a_b first. Equal activity keeps that order.
    let (_dir, path) = tmp();
    let header = "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n";
    for name in ["a_b.jsonl", "a-b.jsonl"] {
        fs::write(format!("{path}/{name}"), header).unwrap();
    }
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let names: Vec<String> = list_sessions(&env_box, "/tmp", Some(&path))
        .unwrap()
        .into_iter()
        .map(|s| s.path.rsplit('/').next().unwrap().to_string())
        .collect();
    assert_eq!(names, ["a-b.jsonl", "a_b.jsonl"]);
}

#[test]
fn session_file_mtime_fallback_uses_node_mtime_ms() {
    // D7: Node's `mtimeMs` is `sec * 1e3 + nsec / 1e6` = 1735689600000.9998 for (1735689600 s, 999810 ns), which
    // TimeClip truncates to ...000; `as_secs_f64() * 1000` rounded it up to ...001.
    let (_dir, path) = tmp();
    let file = format!("{path}/s.jsonl");
    fs::write(
        &file,
        "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"not a date\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    let at = std::time::UNIX_EPOCH + std::time::Duration::new(1_735_689_600, 999_810);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(at)
        .unwrap();
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let info = list_sessions(&env_box, "/tmp", Some(&path)).unwrap();
    assert_eq!(info[0].modified, Some(1_735_689_600_000));
}

/// An [`Env`] with its own home and cwd.
struct Placed;

impl Env for Placed {
    fn now_ms(&mut self) -> i64 {
        SystemEnv.now_ms()
    }
    fn random_uuid(&mut self) -> String {
        SystemEnv.random_uuid()
    }
    fn session_id(&mut self) -> String {
        SystemEnv.session_id()
    }
    fn agent_dir(&self) -> String {
        "/nonexistent-agent-dir".into()
    }
    fn home(&self) -> String {
        "/env-home".into()
    }
    fn cwd(&self) -> String {
        "/env-cwd".into()
    }
}

/// `~` and relative paths name the Env's home and cwd, not the process's.
#[test]
fn session_file_paths_resolve_against_the_env_home_and_cwd() {
    assert_eq!(
        session_file::default_session_dir_path(&Placed, "proj", "~/agent"),
        "/env-home/agent/sessions/--env-cwd-proj--"
    );
}
