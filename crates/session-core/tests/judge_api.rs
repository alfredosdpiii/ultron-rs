//! `judge.decide` (judge_api.py, judge, session_core::session_judge) in the product's session: a root cell asks a
//! d1 server (a loopback fake, its URL in the global `judge.d1.url`, its key in `ULTRON_D1_KEY`) one question and
//! gets the verdict; the call is a session usage row with `details.source = "judge"`. Without any backend the
//! verdict abstains and nothing is sent. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// What the fake server saw: path, authorization, body.
type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

/// A d1 server answering every `/decide` with `noul: 0.97`; the requests it saw (path, authorization, body).
fn fake_d1() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let (mut length, mut authorization) = (0, String::new());
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if k.eq_ignore_ascii_case("authorization") {
                        authorization = v.trim().to_string();
                    }
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let path = first.split_whitespace().nth(1).unwrap_or_default().to_string();
            log.lock().unwrap().push((
                path,
                authorization,
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            let answer = json!({"answers": {"q": {"noul": 0.97}}, "ms": 12.0}).to_string();
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

fn printed(output: &str, prefix: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{prefix} ")))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const CELL: &str = r#"import json
v = await judge.decide("MEMORY: a\nEVIDENCE: b", {"q": {"type": "yesno", "instructions": "Still true?"}}, kind="memory")
print('verdict', json.dumps(v))
try:
    await judge.decide("s", {"q": {"type": "maybe"}})
except ValueError as e:
    print('error', json.dumps(str(e)))
print('doc', json.dumps('verdict' in (type(judge).__doc__ or '')))"#;

fn output(a: &p_session_support::Actor) -> String {
    let results = a.requests().last().map(p_session_support::tool_results).unwrap();
    results[0].as_str().unwrap().to_string()
}

#[test]
fn session_core_judge_decide_asks_d1_and_records_a_usage_row() {
    let (base, seen) = fake_d1();
    let d1_key = ["fake", "-d1-", "key"].concat();
    let a = actor(
        in_order(vec![tool("toolu_1", CELL), text("done")]),
        Setup {
            settings: Some(json!({"judge": {"provider": ["d1"], "d1": {"url": base}}})),
            vars: vec![("ULTRON_D1_KEY", d1_key.clone())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("judge it");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let output = output(&a);
    let verdict = &printed(&output, "verdict")[0];
    assert_eq!(
        (
            verdict["rung"].clone(),
            verdict["backend"].clone(),
            verdict["kind"].clone()
        ),
        (json!("first"), json!("d1"), json!("memory")),
        "{output}"
    );
    assert_eq!(verdict["answers"]["q"]["value"], true);
    assert_eq!(
        (verdict["calls"].clone(), verdict["cost_usd"].clone()),
        (json!(1), json!(0.0))
    );
    assert_eq!(
        printed(&output, "error"),
        [json!(
            "judge.decide questions['q'] needs a type: choice, yesno, score or number"
        )]
    );
    assert_eq!(printed(&output, "doc"), [json!(true)]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.clone()),
        ("/decide", format!("Bearer {d1_key}"))
    );
    assert_eq!(seen[0].2["text"], "MEMORY: a\nEVIDENCE: b");
    // The call is a usage row of the session's own.
    let rows: Vec<Value> = a.writes().into_iter().filter(|w| w["kind"] == "usage").collect();
    let judged: Vec<&Value> = rows.iter().filter(|r| r["details"]["source"] == "judge").collect();
    assert_eq!(judged.len(), 1, "{rows:?}");
    let details = &judged[0]["details"];
    assert_eq!(
        (
            details["backend"].clone(),
            details["kind"].clone(),
            details["status"].clone(),
            details["cost"].as_f64()
        ),
        (json!("d1"), json!("memory"), json!("succeeded"), Some(0.0))
    );
    assert!(details["requestKey"].as_str().unwrap().starts_with("judge:"));
    assert!(!a.writes().iter().any(|w| w.to_string().contains(&d1_key)));
}

#[test]
fn session_core_judge_decide_abstains_without_a_backend() {
    let a = actor(in_order(vec![tool("toolu_1", CELL), text("done")]), Setup::default());
    let ended = a.prompt("judge it");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let output = output(&a);
    let verdict = &printed(&output, "verdict")[0];
    assert_eq!(
        (verdict["rung"].clone(), verdict["calls"].clone()),
        (json!("abstained"), json!(0)),
        "{output}"
    );
    assert_eq!(verdict["reason"], "no judge backend answered");
    let skipped: Vec<String> = verdict["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r["backend"].as_str().unwrap(), r["status"].as_str().unwrap()))
        .collect();
    assert_eq!(skipped, ["jev skipped", "d1 skipped"]);
    assert!(a.writes().iter().all(|w| w["details"]["source"] != "judge"));
}
