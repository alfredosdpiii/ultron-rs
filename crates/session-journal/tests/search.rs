//! `session_journal::search`: spans, grep and read over stored sessions, read-only. New in Rust: literal
//! expectations.

use serde_json::{Value, json};
use session_journal::search::{self, Branch, Matcher, Query, ReadQuery, SpanRef};
use session_journal::{Session, SessionRepo, TestClock, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text, "timestamp": 1})
}

fn assistant(text: &str, code: &str) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": "secret thoughts about needle"},
        {"type": "text", "text": text},
        {"type": "toolCall", "id": "c1", "name": "python", "arguments": {"code": code}},
    ], "timestamp": 2})
}

fn tool_result(text: &str) -> Value {
    json!({"role": "toolResult", "toolCallId": "c1", "toolName": "python",
           "content": [{"type": "text", "text": text}], "isError": false, "timestamp": 3})
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
}

/// A conversation on `main` and a frame lane beside it.
fn conversation(repo: &SessionRepo, cwd: &str, id: &str, topic: &str) -> (Session, Vec<String>) {
    let mut session = repo.create(cwd, Some(id), None).unwrap();
    session
        .commit(&[
            Write::set("ultron.branch.tip", "main", Value::Null),
            Write::set("ultron.branch.tip", "frame-1", Value::Null),
        ])
        .unwrap();
    let mut ids = vec![
        session
            .append_message("main", user(&format!("fix the {topic} please")))
            .unwrap(),
        session
            .append_message(
                "main",
                assistant(&format!("Looking at {topic}."), "print('grep retry')"),
            )
            .unwrap(),
        session
            .append_message("main", tool_result(&format!("{topic}: {}", "x".repeat(50))))
            .unwrap(),
    ];
    session
        .append_message("frame-1", user(&format!("frame-only {topic} text")))
        .unwrap();
    ids.push(session.append_message("main", user("thanks, done")).unwrap());
    (session, ids)
}

fn query(matcher: Matcher) -> Query {
    Query {
        matcher,
        cwd: Some("/w".into()),
        sessions: Vec::new(),
        since_ms: None,
        branch: Branch::Lane("main".into()),
        roles: Vec::new(),
        limit: 50,
        per_session: 10,
        snippet: 40,
    }
}

fn never() -> bool {
    false
}

struct Store {
    _dir: tempfile::TempDir,
    roots: Vec<PathBuf>,
    a: (Session, Vec<String>),
}

fn store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let repo = SessionRepo::new(&root, TestClock::shared(1_790_000_000_000, 1));
    let a = conversation(&repo, "/w", "a", "retry loop");
    let b = conversation(&repo, "/w", "b", "parser");
    let c = conversation(&repo, "/other", "c", "retry elsewhere");
    for (session, seconds) in [(&a.0, 3_000), (&b.0, 2_000), (&c.0, 1_000)] {
        set_mtime(&session.metadata().path, seconds);
    }
    Store {
        roots: vec![dir.path().join("missing"), root],
        _dir: dir,
        a,
    }
}

#[test]
fn session_journal_span_refs_parse_and_print() {
    let span = SpanRef::parse("session://abc#e1..e2").unwrap();
    assert_eq!(
        span,
        SpanRef {
            session: "abc".into(),
            from: Some("e1".into()),
            to: Some("e2".into())
        }
    );
    assert_eq!(span.to_string(), "session://abc#e1..e2");
    assert_eq!(SpanRef::entry("abc", "e1").to_string(), "session://abc#e1");
    assert_eq!(SpanRef::parse("session://abc#e1").unwrap(), SpanRef::entry("abc", "e1"));
    let whole = SpanRef::parse("session://abc").unwrap();
    assert_eq!((whole.from, whole.to), (None, None));
    let open = SpanRef::parse("session://abc#..e2").unwrap();
    assert_eq!((open.from.as_deref(), open.to.as_deref()), (None, Some("e2")));
    assert_eq!(open.to_string(), "session://abc#..e2");
    for bad in ["abc#e1..e2", "session://", "session://#e1", "session://abc#.."] {
        assert!(SpanRef::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn session_journal_grep_searches_the_main_branch_of_a_cwd_newest_first() {
    let store = store();
    let found = search::grep(&store.roots, &query(Matcher::pattern("RETRY", true).unwrap()), &never);
    let hits: Vec<(&str, &str)> = found
        .hits
        .iter()
        .map(|h| (h.session.as_str(), h.role.as_str()))
        .collect();
    // "retry" is in a's prose and tool result and in every assistant's code (`grep retry`), not in b's prose; c
    // is another cwd, the frame lane is not the conversation, thinking is not searched.
    assert_eq!(
        hits,
        [
            ("a", "user"),
            ("a", "assistant"),
            ("a", "toolResult"),
            ("b", "assistant")
        ],
        "{found:?}"
    );
    assert_eq!((found.searched, found.truncated), (2, false));
    let first = &found.hits[0];
    assert_eq!(first.span.to_string(), format!("session://a#{}", store.a.1[0]));
    assert_eq!(first.snippet, "fix the retry loop please");
    assert_eq!(found.hits[1].matches, 2);
    assert_eq!(found.hits[1].tool, None);
    // Case matters when asked.
    let exact = search::grep(&store.roots, &query(Matcher::pattern("RETRY", false).unwrap()), &never);
    assert!(exact.hits.is_empty());
    assert!(Matcher::pattern("(", true).is_err());
}

#[test]
fn session_journal_grep_scopes_by_cwd_branch_role_session_and_time() {
    let store = store();
    let terms = |terms: &[&str]| Matcher::terms(&terms.iter().map(|t| t.to_string()).collect::<Vec<_>>()).unwrap();
    let sessions = |found: search::Found| found.hits.into_iter().map(|h| h.session).collect::<Vec<_>>();

    let mut every_cwd = query(terms(&["retry", "fix"]));
    every_cwd.cwd = None;
    assert_eq!(sessions(search::grep(&store.roots, &every_cwd, &never)), ["a", "c"]);

    let mut all_entries = query(terms(&["frame-only"]));
    assert!(search::grep(&store.roots, &all_entries, &never).hits.is_empty());
    all_entries.branch = Branch::All;
    assert_eq!(sessions(search::grep(&store.roots, &all_entries, &never)), ["a", "b"]);
    all_entries.branch = Branch::Lane("frame-1".into());
    assert_eq!(sessions(search::grep(&store.roots, &all_entries, &never)), ["a", "b"]);

    let mut tools = query(terms(&["xxxx"]));
    tools.roles = vec!["toolResult".into()];
    let found = search::grep(&store.roots, &tools, &never);
    assert_eq!(found.hits.len(), 2);
    assert_eq!(found.hits[0].tool.as_deref(), Some("python"));
    tools.roles = vec!["user".into()];
    assert!(search::grep(&store.roots, &tools, &never).hits.is_empty());

    let mut only_b = query(terms(&["fix"]));
    only_b.sessions = vec!["b".into()];
    assert_eq!(sessions(search::grep(&store.roots, &only_b, &never)), ["b"]);

    let mut recent = query(terms(&["fix"]));
    recent.since_ms = Some(2_500_000.0);
    assert_eq!(sessions(search::grep(&store.roots, &recent, &never)), ["a"]);

    let mut capped = query(Matcher::pattern(".", true).unwrap());
    capped.per_session = 1;
    let found = search::grep(&store.roots, &capped, &never);
    assert_eq!(
        (sessions(found.clone()), found.truncated),
        (vec!["a".to_string(), "b".into()], true)
    );
    capped.limit = 1;
    let found = search::grep(&store.roots, &capped, &never);
    assert_eq!((found.hits.len(), found.searched, found.truncated), (1, 1, true));
    let stopped = search::grep(&store.roots, &query(terms(&["fix"])), &|| true);
    assert_eq!((stopped.hits.len(), stopped.searched, stopped.truncated), (0, 0, true));
    assert!(Matcher::terms(&[" ".to_string()]).is_err());
}

#[test]
fn session_journal_read_returns_a_span_with_margin_and_limit() {
    let store = store();
    let ids = &store.a.1;
    let read = |span: SpanRef, around: usize, limit: usize| {
        search::read(
            &store.roots,
            &ReadQuery {
                span,
                lane: "main".into(),
                around,
                limit,
                tool_chars: 10,
            },
        )
    };
    let span = SpanRef::parse(&format!("session://a#{}..{}", ids[1], ids[2])).unwrap();
    let got = read(span.clone(), 0, 100).unwrap();
    assert_eq!(got.span, span);
    assert_eq!(
        got.entries.iter().map(|e| e.role.as_str()).collect::<Vec<_>>(),
        ["assistant", "toolResult"]
    );
    assert_eq!(
        got.entries[0].text,
        "Looking at retry loop.\n[python]\nprint('grep retry')"
    );
    assert_eq!(
        got.entries[1].text, "retry loop...",
        "tool results are cut to tool_chars"
    );
    assert_eq!(got.entries[1].tool.as_deref(), Some("python"));
    assert_eq!((got.metadata.id.as_str(), got.truncated), ("a", false));

    let around = read(SpanRef::entry("a", &ids[1]), 1, 100).unwrap();
    assert_eq!(around.span.to_string(), format!("session://a#{}..{}", ids[0], ids[2]));
    let whole = read(SpanRef::parse("session://a").unwrap(), 0, 100).unwrap();
    assert_eq!(whole.entries.len(), 4, "the main branch, not the frame lane");
    let cut = read(SpanRef::parse("session://a").unwrap(), 0, 2).unwrap();
    assert_eq!((cut.entries.len(), cut.truncated), (2, true));
    assert_eq!(cut.span.to_string(), format!("session://a#{}..{}", ids[0], ids[1]));

    let backwards = SpanRef::parse(&format!("session://a#{}..{}", ids[2], ids[1])).unwrap();
    assert!(
        read(backwards, 0, 100)
            .unwrap_err()
            .contains("is not on the branch before")
    );
    assert_eq!(
        read(SpanRef::entry("a", "nope"), 0, 100).err(),
        Some("No entry nope in session a".to_string())
    );
    assert_eq!(
        read(SpanRef::entry("zz", "x"), 0, 100).err(),
        Some("No session zz".to_string())
    );
}

#[test]
fn session_journal_read_follows_the_branch_of_an_entry_on_another_lane() {
    let store = store();
    let state = search::replay(&store.a.0.metadata().path).unwrap();
    let frame_tip = state
        .value("ultron.branch.tip", "frame-1")
        .unwrap()
        .value
        .as_str()
        .unwrap()
        .to_string();
    let got = search::read(
        &store.roots,
        &ReadQuery {
            span: SpanRef::entry("a", &frame_tip),
            lane: "main".into(),
            around: 0,
            limit: 100,
            tool_chars: 100,
        },
    )
    .unwrap();
    assert_eq!(got.entries.len(), 1);
    assert_eq!(got.entries[0].text, "frame-only retry loop text");
}

#[test]
fn session_journal_search_reads_pi_named_tips_and_never_writes() {
    // Sessions written before the rename keep `pi.branch.tip`; they read as `ultron.branch.tip`. A torn last line
    // (a writer mid-append) is skipped and left in place.
    let store = store();
    let path = store.a.0.metadata().path.clone();
    let old = std::fs::read_to_string(&path).unwrap().replace("\"ultron.", "\"pi.");
    let torn = format!("{old}[{{\"kind\":\"entry\",\"id\":\"half");
    std::fs::write(&path, &torn).unwrap();
    set_mtime(&path, 3_000);
    let found = search::grep(
        &store.roots,
        &query(Matcher::pattern("retry loop", true).unwrap()),
        &never,
    );
    assert_eq!(found.hits.len(), 3, "{found:?}");
    let whole = search::read(
        &store.roots,
        &ReadQuery {
            span: SpanRef::parse("session://a").unwrap(),
            lane: "main".into(),
            around: 0,
            limit: 100,
            tool_chars: 100,
        },
    )
    .unwrap();
    assert_eq!(whole.entries.len(), 4);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), torn, "the file is untouched");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        UNIX_EPOCH + Duration::from_secs(3_000)
    );
}

#[test]
fn session_journal_search_reads_legacy_v3_files_with_stable_ids() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join(session_journal::catalog::session_directory_name("/w"));
    std::fs::create_dir_all(&folder).unwrap();
    let lines = [
        json!({"type": "session", "version": 3, "id": "old", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/w"}),
        json!({"type": "message", "id": "m1", "parentId": null, "timestamp": "2026-01-01T00:00:01.000Z",
               "message": {"role": "user", "content": "legacy needle", "timestamp": 1}}),
        json!({"type": "message", "id": "m2", "parentId": "m1", "timestamp": "2026-01-01T00:00:02.000Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}], "timestamp": 2}}),
    ];
    let path = folder.join("old.jsonl");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &text).unwrap();
    let roots = vec![dir.path().to_path_buf()];
    let grep = || search::grep(&roots, &query(Matcher::pattern("needle", true).unwrap()), &never);
    let first = grep();
    assert_eq!(first.hits.len(), 1, "{first:?}");
    assert_eq!(
        first.hits[0].span,
        grep().hits[0].span,
        "a legacy file's ids are the same on every read"
    );
    let read = search::read(
        &roots,
        &ReadQuery {
            span: first.hits[0].span.clone(),
            lane: "main".into(),
            around: 1,
            limit: 10,
            tool_chars: 10,
        },
    )
    .unwrap();
    assert_eq!(
        read.entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["legacy needle", "ok"]
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        text,
        "the legacy file is not upgraded"
    );
}

#[test]
fn session_journal_search_lists_across_roots_once_per_id() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("one"), dir.path().join("two"));
    for root in [&first, &second] {
        let repo = SessionRepo::new(root, TestClock::shared(1_790_000_000_000, 1));
        repo.create("/w", Some("same"), None).unwrap();
    }
    SessionRepo::new(&second, TestClock::shared(1_790_000_000_000, 1))
        .create("/w", Some("other"), None)
        .unwrap();
    let listed = search::list(&[first.clone(), second], Some("/w"), None);
    let mut ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["other", "same"]);
    assert!(listed.iter().any(|m| m.id == "same" && m.path.starts_with(&first)));
}
