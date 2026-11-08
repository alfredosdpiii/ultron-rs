//! `knowledge.relations` (knowledge_api.py, graph, session_core::session_knowledge) in the product's session: a
//! root cell commits a small repository in the session directory and asks for its relations; plain code settles
//! the environment read, the judge (a loopback d1 server) settles the settings lookup it could not, and the
//! verdict is cached in the knowledge store under the agent directory. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A d1 server answering yes (`noul: 0.97`) to every question it is asked; the bodies it saw.
fn fake_d1() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let answers: serde_json::Map<String, Value> = body["questions"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, _)| (name.clone(), json!({"noul": 0.97})))
                .collect();
            log.lock().unwrap().push(body);
            let answer = json!({"answers": answers, "ms": 12.0}).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    (base, seen)
}

const CELL: &str = r#"import json, os, subprocess
env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
def g(*args):
    subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", *args],
                   check=True, env=env, capture_output=True)
os.makedirs("src", exist_ok=True)
with open("src/lib.rs", "w") as f:
    f.write('pub fn home() -> String {\n    std::env::var("STORE_HOME").unwrap_or_default()\n}\n\npub fn level(settings: &Settings) -> String {\n    settings.get("log.level")\n}\n')
g("init", "-q", "-b", "main"); g("add", "-A"); g("commit", "-q", "-m", "base")
found = await knowledge.relations()
print('found', json.dumps(found))
again = await knowledge.relations(kinds="reads_config")
print('again', json.dumps(again))
try:
    await knowledge.relations(kinds=["calls"])
except ValueError as e:
    print('error', json.dumps(str(e)))
print('doc', json.dumps('reads_config' in (type(knowledge).__doc__ or '')))"#;

fn printed(output: &str, prefix: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{prefix} ")))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn session_core_knowledge_relations_settles_with_the_judge_and_caches_it() {
    let (base, seen) = fake_d1();
    let a = actor(
        in_order(vec![tool("toolu_1", CELL), text("done")]),
        Setup {
            settings: Some(json!({"judge": {"provider": ["d1"], "d1": {"url": base}}})),
            vars: vec![("ULTRON_D1_KEY", ["fake", "-d1-", "key"].concat())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("relations");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = a.requests().last().map(p_session_support::tool_results).unwrap();
    let output = results[0].as_str().unwrap().to_string();
    let found = &printed(&output, "found")[0];
    let summary: Vec<String> = found["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {} [{} by {}]",
                r["subject"].as_str().unwrap(),
                r["object"].as_str().unwrap(),
                r["status"].as_str().unwrap(),
                r["checked_by"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            "src/lib.rs::home STORE_HOME [accepted by code]",
            "src/lib.rs::level log.level [accepted by d1]"
        ],
        "{output}"
    );
    assert_eq!(found["judge"], json!({"calls": 1, "cost_usd": 0.0, "abstained": 0}));
    assert_eq!(found["mirror"], Value::Null);
    let again = &printed(&output, "again")[0];
    assert_eq!(
        (again["read"].clone(), again["judge"]["calls"].clone()),
        (json!(0), json!(0))
    );
    assert_eq!(again["relations"][1]["rung"], "first");
    assert_eq!(
        printed(&output, "error"),
        [json!(
            "knowledge.relations kind 'calls' is not one of reads_config, serves_route, touches_table, tests"
        )]
    );
    assert_eq!(printed(&output, "doc"), [json!(true)]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let text = seen[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("[0] src/lib.rs line 6:\n") && text.contains("settings.get(\"log.level\")"),
        "{text}"
    );
    assert!(
        seen[0]["questions"]["r0"]["instructions"]
            .as_str()
            .unwrap()
            .starts_with("Snippet [0]: Does")
    );
    // The store is the agent directory's, shared with reviews.
    assert!(a.root.join("agent/knowledge/graph.db").exists());
    let judged: Vec<Value> = a
        .writes()
        .into_iter()
        .filter(|w| w["kind"] == "usage" && w["details"]["source"] == "judge")
        .collect();
    assert_eq!(judged.len(), 1);
    assert_eq!(judged[0]["details"]["kind"], "relation");
}
