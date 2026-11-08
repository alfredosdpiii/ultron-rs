//! `ultron memory migrate --to memorantado [--dry-run]` through `cli::run`, from a loopback Hindsight that lists its
//! documents to the fake memorantado server on 127.0.0.1: the dry run lists the plan and writes nothing; the copy
//! keeps each document's id as its key, its tags and metadata, makes a summaries document an episode, and a second
//! run copies nothing new.

use crate::memorantado_fake::FakeMemorantado;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

/// A Hindsight bank `ultron` that answers the document list and document reads.
fn hindsight(documents: Vec<Value>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
            }
            let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let path = target.strip_prefix("/v1/default/banks/ultron").unwrap_or("");
            let (status, body) = if let Some(query) = path.strip_prefix("/documents?") {
                let offset: usize = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("offset="))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                // One document per page, to cover paging.
                let items: Vec<Value> = documents
                    .iter()
                    .skip(offset)
                    .take(1)
                    .map(|d| json!({"id": d["id"]}))
                    .collect();
                (
                    200,
                    json!({"items": items, "total": documents.len(), "limit": 1, "offset": offset}),
                )
            } else if let Some(id) = path.strip_prefix("/documents/") {
                let id = id.replace("%3A", ":");
                match documents.iter().find(|d| d["id"] == id.as_str()) {
                    Some(document) => (200, document.clone()),
                    None => (404, json!({})),
                }
            } else {
                (404, json!({}))
            };
            let text = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn env_for(home: &Path, vars: Vec<(&'static str, String)>) -> cli::Env {
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut all: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        (config::paths::ENV_AGENT_DIR.into(), agent.display().to_string()),
    ];
    all.extend(vars.into_iter().map(|(k, v)| (k.to_string(), v)));
    cli::Env {
        cwd: home.to_path_buf(),
        home: home.to_path_buf(),
        var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        package: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent")),
    }
}

fn run(args: &[&str], env: &cli::Env) -> (i32, String, String) {
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&argv, env, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn cli_memory_migrate_dry_run_then_copy_then_nothing_new() {
    let home = tempfile::tempdir().unwrap();
    let documents = vec![
        json!({"id": "doc-1", "original_text": "Use nextest", "tags": ["ultron:project:p1"],
            "document_metadata": {"ultron_type": "procedure", "ultron_situation": "running tests", "ultron_operation": "op-1"}}),
        json!({"id": "ultron:session:s1:summaries", "original_text": "Summary of s1", "tags": ["ultron:project:p1"],
            "document_metadata": {"ultron_source": "auto"}}),
        json!({"id": "empty", "original_text": "", "tags": ["ultron:project:p1"]}),
    ];
    let source = hindsight(documents);
    let fake = FakeMemorantado::new();
    let target = fake.serve();
    let env = env_for(
        home.path(),
        vec![
            ("ULTRON_HINDSIGHT_URL", source.clone()),
            ("ULTRON_MEMORANTADO_URL", target.clone()),
        ],
    );

    let (code, out, err) = run(&["memory", "migrate", "--to", "memorantado", "--dry-run"], &env);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.contains("Dry run: 3 Hindsight documents") && out.contains("nothing was written"),
        "{out}"
    );
    assert!(
        out.contains("  claim: doc-1\n") && out.contains("  episode: ultron:session:s1:summaries\n"),
        "{out}"
    );
    assert!(out.contains("  skip (no text): empty\n"), "{out}");
    assert!(
        out.ends_with("1 claims, 1 episodes, 0 unchanged, 1 skipped.\n"),
        "{out}"
    );
    assert!(fake.lock().memories.is_empty() && fake.lock().episodes.is_empty());

    let (code, out, err) = run(&["memory", "migrate", "--to=memorantado"], &env);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(out.starts_with("Copied 2, failed 0"), "{out}");
    let claim = fake.active("doc-1").unwrap();
    assert_eq!(claim["content"], "Use nextest");
    assert_eq!(claim["tags"], json!(["ultron:project:p1"]));
    assert_eq!(claim["kind"], "procedure");
    assert_eq!(claim["subject"], "running tests");
    assert_eq!(claim["metadata"]["ultron_operation"], "op-1");
    assert_eq!(fake.lock().episodes[0]["session"], "ultron:session:s1:summaries");

    let (code, out, _) = run(&["memory", "migrate", "--to", "memorantado"], &env);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("Copied 0, failed 0 (0 claims, 0 episodes, 2 unchanged, 1 skipped)"),
        "{out}"
    );
    assert_eq!(fake.lock().episodes.len(), 1, "an episode is copied once");

    let (code, _, err) = run(&["memory", "migrate", "--to", "hindsight"], &env);
    assert_eq!((code, err.lines().next()), (1, Some("Error: --to must be memorantado")));
    let down = env_for(
        home.path(),
        vec![
            ("ULTRON_HINDSIGHT_URL", source),
            ("ULTRON_MEMORANTADO_URL", "http://127.0.0.1:9".into()),
        ],
    );
    let (code, _, err) = run(&["memory", "migrate", "--to", "memorantado"], &down);
    assert_eq!(
        (code, err.as_str()),
        (1, "Error: memorantado does not answer at http://127.0.0.1:9.\n")
    );
}
