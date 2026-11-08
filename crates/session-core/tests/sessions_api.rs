//! `sessions.list/search/read` (sessions_api.py over kernel_apis::sessions and session_journal::search) in the
//! product's session: a root cell lists this project's sessions (this one marked current), finds a turn of an
//! earlier session, reads it back by its span with a margin, widens the scope to every project only when asked,
//! and reads its own conversation. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};
use session_journal::{SessionRepo, SystemClock, Write};
use std::path::Path;

fn lines(output: &str, prefix: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{prefix} ")))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// An earlier session in `cwd` under `root`: a user turn and an answer on `main`; returns their entry ids.
fn earlier(root: &Path, cwd: &Path, id: &str, ask: &str, answer: &str) -> Vec<String> {
    let repo = SessionRepo::new(root, SystemClock::shared());
    let mut session = repo.create(&cwd.display().to_string(), Some(id), None).unwrap();
    session
        .commit(&[
            Write::set(session_journal::ns::BRANCH_TIP, "main", Value::Null),
            Write::set(session_journal::ns::SESSION_NAME, "", json!(format!("{id} work"))),
        ])
        .unwrap();
    let message = |role: &str, text: &str| json!({"role": role, "content": [{"type": "text", "text": text}]});
    vec![
        session.append_message("main", message("user", ask)).unwrap(),
        session.append_message("main", message("assistant", answer)).unwrap(),
    ]
}

#[test]
fn session_core_sessions_api_lists_searches_and_reads_spans() {
    let code = [
        "import json",
        "rows = await sessions.list()",
        "print('list', json.dumps([[r['id'] if not r['current'] else 'me', r['name']] for r in rows]))",
        // Built at run time: this cell's own code is in the current session's conversation.
        "word = 'NEE' + 'DLE'",
        "found = await sessions.search(word)",
        "print('hits', json.dumps([[h['session'], h['role'], h['snippet']] for h in found['hits']]))",
        "span = await sessions.read(found['hits'][0]['span'], around=1)",
        "print('span', json.dumps([span['span'], [e['text'] for e in span['entries']], span['session']['name']]))",
        "wide = await sessions.search(terms=[word.lower()], cwd='all')",
        "print('all', json.dumps(sorted({h['session'] for h in wide['hits']})))",
        "me = await sessions.read()",
        "print('me', json.dumps([me['session']['current'], [e['text'] for e in me['entries'] if e['role'] == 'user']]))",
        "for call in [lambda: sessions.search(), lambda: sessions.search('a', terms=['b']),",
        "             lambda: sessions.read('nope'), lambda: sessions.search('(')]:",
        "    try:",
        "        await call()",
        "    except Exception as e:",
        "        print('error', json.dumps([type(e).__name__, str(e)]))",
        "print('doc', json.dumps('session://' in (type(sessions).__doc__ or '')))",
    ]
    .join("\n");
    let a = actor(in_order(vec![tool("toolu_1", &code), text("done")]), Setup::default());
    let root = a.root.join("agent/experimental/sessions");
    let ids = earlier(
        &root,
        &a.cwd,
        "old",
        "where is the needle?",
        "The needle is in the haystack.",
    );
    earlier(&root, &a.root.join("elsewhere"), "far", "another needle", "elsewhere");
    let ended = a.prompt("find the earlier work");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = p_session_support::tool_results(a.requests().last().unwrap());
    let output = results[0].to_string();
    let output: String = serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or(output);
    assert_eq!(
        lines(&output, "list"),
        vec![json!([["me", null], ["old", "old work"]])],
        "{output}"
    );
    assert_eq!(
        lines(&output, "hits"),
        vec![json!([
            ["old", "user", "where is the needle?"],
            ["old", "assistant", "The needle is in the haystack."]
        ])]
    );
    assert_eq!(
        lines(&output, "span"),
        vec![json!([
            format!("session://old#{}..{}", ids[0], ids[1]),
            ["where is the needle?", "The needle is in the haystack."],
            "old work"
        ])]
    );
    assert_eq!(lines(&output, "all"), vec![json!(["far", "old"])]);
    assert_eq!(lines(&output, "me"), vec![json!([true, ["find the earlier work"]])]);
    let errors = lines(&output, "error");
    assert_eq!(errors.len(), 4, "{output}");
    assert_eq!(
        errors[..2],
        [
            json!(["ValueError", "sessions.search takes a pattern or terms=[...], not both"]),
            json!(["ValueError", "sessions.search takes a pattern or terms=[...], not both"]),
        ]
    );
    assert!(
        errors[2][1].as_str().unwrap().contains("Invalid session span \"nope\""),
        "{errors:?}"
    );
    assert!(
        errors[3][1].as_str().unwrap().contains("Invalid pattern \"(\""),
        "{errors:?}"
    );
    assert_eq!(lines(&output, "doc"), vec![json!(true)]);
}

#[test]
fn session_core_recall_sessions_script_reads_through_the_api() {
    // The recall skill's helpers over `sessions`: a session written before the `pi.*` rename keeps its tip under
    // `pi.branch.tip` (the old script looked for that name only, and missed every renamed one); both read.
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/coding-agent/skills/pstack/recall/scripts/sessions.py");
    let code = [
        "import json, runpy".to_string(),
        format!(
            "s = runpy.run_path({:?}, init_globals={{'sessions': sessions}})",
            script.display().to_string()
        ),
        "rows = await s['session_files'](days=1)".into(),
        "print('files', json.dumps([[r['id'], r['current']] for r in rows]))".into(),
        "word = 'hay' + 'stack'".into(),
        "print('grep', json.dumps(await s['grep_sessions'](rows, word)))".into(),
        "x = await s['read_session']('renamed', tool_chars=50)".into(),
        "print('read', json.dumps([x['name'], [[m['role'], m['text']] for m in x['messages']]]))".into(),
        "bare = runpy.run_path(".to_string() + &format!("{:?})", script.display().to_string()),
        "try:".into(),
        "    await bare['session_files']()".into(),
        "except RuntimeError as e:".into(),
        "    print('bare', json.dumps(str(e)))".into(),
    ]
    .join("\n");
    let a = actor(in_order(vec![tool("toolu_1", &code), text("done")]), Setup::default());
    let root = a.root.join("agent/experimental/sessions");
    earlier(&root, &a.cwd, "plain", "a haystack", "one haystack, two haystack");
    earlier(&root, &a.cwd, "renamed", "find the haystack", "Found it.");
    let renamed = session_journal::catalog::find(&root, "renamed").unwrap().unwrap().path;
    let old = std::fs::read_to_string(&renamed).unwrap().replace("\"ultron.", "\"pi.");
    assert!(old.contains("\"pi.branch.tip\""));
    std::fs::write(&renamed, &old).unwrap();
    let ended = a.prompt("recall");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = p_session_support::tool_results(a.requests().last().unwrap());
    let output = results[0].to_string();
    let output: String = serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or(output);
    let files = lines(&output, "files");
    assert_eq!(files.len(), 1, "{output}");
    let mut ids: Vec<(String, bool)> = files[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_bool().unwrap()))
        .collect();
    ids.sort();
    assert_eq!(ids.iter().filter(|(_, current)| *current).count(), 1);
    ids.retain(|(_, current)| !current);
    assert_eq!(ids, [("plain".to_string(), false), ("renamed".to_string(), false)]);
    assert_eq!(lines(&output, "grep"), vec![json!([["plain", 3], ["renamed", 1]])]);
    assert_eq!(
        lines(&output, "read"),
        vec![json!([
            "renamed work",
            [["user", "find the haystack"], ["assistant", "Found it."]]
        ])]
    );
    assert_eq!(
        lines(&output, "bare"),
        vec![json!("load sessions.py with init_globals={\"sessions\": sessions}")]
    );
    assert_eq!(std::fs::read_to_string(&renamed).unwrap(), old, "read-only");
}
