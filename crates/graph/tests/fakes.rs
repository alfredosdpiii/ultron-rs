//! A fake HTTP world for the mirror: Neo4j's Query API and memorantado's list endpoints, in memory (a
//! [`Transport`]) or on a loopback port (for the real transport).

use blocking_http::{HttpRequest, HttpResponse, Transport, TransportError};
use kernel::Signal;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A request as the fake saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub url: String,
    pub authorization: Option<String>,
    pub body: Value,
}

/// Answers by URL: memorantado's lists from `memories` and `conflicts`, Neo4j's statements with `neo4j_status`
/// (202 by default; another status, or `TransportError::Failed` when `down`).
#[derive(Default)]
pub struct FakeHttp {
    pub seen: Mutex<Vec<Seen>>,
    pub memories: Mutex<Vec<Value>>,
    pub conflicts: Mutex<Vec<Value>>,
    pub neo4j_status: Mutex<Option<u16>>,
    pub neo4j_body: Mutex<Option<Value>>,
    pub down: Mutex<bool>,
}

impl FakeHttp {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The Cypher statements sent, in order, with their parameters.
    pub fn statements(&self) -> Vec<(String, Value)> {
        self.seen()
            .into_iter()
            .filter(|s| s.url.ends_with("/query/v2"))
            .map(|s| {
                (
                    s.body["statement"].as_str().unwrap().to_string(),
                    s.body["parameters"].clone(),
                )
            })
            .collect()
    }
}

fn query(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")).map(str::to_string))
}

impl Transport for FakeHttp {
    fn send(&self, request: HttpRequest, _: usize, _: Duration, _: &Signal) -> Result<HttpResponse, TransportError> {
        let authorization = request
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.clone());
        let body = request
            .body
            .as_deref()
            .map(|b| serde_json::from_slice(b).unwrap())
            .unwrap_or(Value::Null);
        self.seen.lock().unwrap().push(Seen {
            method: request.method.clone(),
            url: request.url.clone(),
            authorization,
            body,
        });
        if *self.down.lock().unwrap() {
            return Err(TransportError::Failed);
        }
        let answer = if request.url.contains("/semantic-memories") {
            let status = query(&request.url, "status").unwrap();
            let offset: usize = query(&request.url, "offset").unwrap().parse().unwrap();
            let limit: usize = query(&request.url, "limit").unwrap().parse().unwrap();
            let all: Vec<Value> = self
                .memories
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m["status"] == status)
                .cloned()
                .collect();
            json!(all.into_iter().skip(offset).take(limit).collect::<Vec<_>>())
        } else if request.url.contains("/memory-conflicts") {
            json!(*self.conflicts.lock().unwrap())
        } else {
            if let Some(status) = *self.neo4j_status.lock().unwrap() {
                return Ok(HttpResponse {
                    status,
                    ..HttpResponse::default()
                });
            }
            self.neo4j_body
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(json!({"data": {"fields": [], "values": []}, "bookmarks": ["b1"]}))
        };
        Ok(HttpResponse {
            status: if request.url.ends_with("/query/v2") { 202 } else { 200 },
            retry_after: None,
            body: answer.to_string().into_bytes(),
        })
    }
}

/// What the loopback server saw: path, authorization, body.
pub type Served = Arc<Mutex<Vec<(String, String, Value)>>>;

/// A loopback server answering every request with 202 and an empty result.
pub fn serve() -> (String, Served) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Served = Arc::default();
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
            let answer = json!({"data": {"fields": [], "values": []}, "bookmarks": []}).to_string();
            let head = format!(
                "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    (base, seen)
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Graph Test",
            "-c",
            "user.email=graph@test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A fake password, built at run time.
pub fn password() -> String {
    ["fake", "-neo4j-", "secret"].concat()
}
