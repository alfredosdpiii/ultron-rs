//! One language server process: LSP over its stdin/stdout (`Content-Length` framed JSON-RPC), the documents it has
//! open, and the errors it reports for one of them.

use jsrt::sync;
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 4096;
/// ponytail: past this many open documents all are closed and reopened as they are written again; an LRU if the
/// reopening ever shows up.
const MAX_OPEN: usize = 64;
/// How long a server that publishes without document versions gets to report on a change to a file it last called
/// clean. typescript-language-server publishes nothing when a clean file stays clean, so silence is the answer.
const QUIET: Duration = Duration::from_millis(1500);
/// How long a publish without a document version must stay the latest to be the answer: tsserver publishes after
/// its syntax pass (often nothing) and again after its semantic pass.
/// ponytail: a semantic pass slower than this after the syntax one is missed for that write and shows with the
/// next; a server that versions its publishes or answers pulls needs none of it.
const SETTLE: Duration = Duration::from_millis(500);

/// An error a server reported: zero-based position, as in LSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u64,
    pub column: u64,
    pub code: Option<String>,
    pub message: String,
}

/// Why a check has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unready {
    /// The server is still loading the project.
    Loading,
    /// No answer in time.
    Slow,
    Exited,
}

struct Published {
    /// `State::received` when it arrived.
    at: u64,
    version: Option<i64>,
    errors: Vec<Diagnostic>,
}

#[derive(Default)]
struct State {
    responses: HashMap<u64, Value>,
    published: HashMap<PathBuf, Published>,
    /// Messages received so far: orders a publish without a version against what was sent.
    received: u64,
    quiescent: bool,
    exited: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct Doc {
    version: i64,
    hash: String,
    /// `State::received` when this version was sent.
    sent_at: u64,
}

pub struct Client {
    pid: u32,
    /// Frames for the writer thread: a server that stops reading never blocks a check.
    out: Sender<Vec<u8>>,
    shared: Arc<Shared>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: AtomicU64,
    pull: bool,
    status: bool,
    /// Held for a whole check: one at a time per server.
    docs: Mutex<HashMap<PathBuf, Doc>>,
}

/// How a server process is started: `argv` in `cwd` with exactly `env`, for the project at `root`.
pub struct Launch<'a> {
    pub argv: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    pub root: &'a Path,
}

fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

/// The next message, or None at the end of the stream. Text that is no frame (a wrapper's banner) is skipped.
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        if length > MAX_MESSAGE_BYTES {
            return None;
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        if let Ok(message) = serde_json::from_slice(&body) {
            return Some(message);
        }
    }
}

fn errors(items: Option<&Value>) -> Vec<Diagnostic> {
    let number = |item: &Value, at: &str| item.pointer(at).and_then(Value::as_u64).unwrap_or(0);
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("severity").and_then(Value::as_u64).unwrap_or(1) == 1)
        .map(|item| Diagnostic {
            line: number(item, "/range/start/line"),
            column: number(item, "/range/start/character"),
            code: match item.get("code") {
                Some(Value::String(code)) => Some(code.clone()),
                Some(Value::Number(code)) => Some(code.to_string()),
                _ => None,
            },
            message: item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

fn uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(String::from)
}

/// `workspace/configuration`: the value of each asked section (`a.b`) of `settings`.
fn configuration(settings: &Value, params: Option<&Value>) -> Value {
    let items = params.and_then(|p| p.get("items")).and_then(Value::as_array);
    Value::Array(
        items
            .into_iter()
            .flatten()
            .map(|item| {
                let section = item.get("section").and_then(Value::as_str).unwrap_or_default();
                section
                    .split('.')
                    .try_fold(settings, |value, key| value.get(key))
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect(),
    )
}

fn incoming(message: Value, shared: &Shared, out: &Sender<Vec<u8>>, settings: &Value) {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id");
    let mut state = sync::lock(&shared.state);
    state.received += 1;
    match (method, id) {
        (Some(method), Some(id)) => {
            let result = match method {
                "workspace/configuration" => configuration(settings, message.get("params")),
                _ => Value::Null,
            };
            let _ = out.send(frame(&json!({ "jsonrpc": "2.0", "id": id, "result": result })));
        }
        (Some("textDocument/publishDiagnostics"), None) => {
            let file = message
                .pointer("/params/uri")
                .and_then(Value::as_str)
                .and_then(|uri| url::Url::parse(uri).ok())
                .and_then(|uri| uri.to_file_path().ok());
            if let Some(file) = file {
                let published = Published {
                    at: state.received,
                    version: message.pointer("/params/version").and_then(Value::as_i64),
                    errors: errors(message.pointer("/params/diagnostics")),
                };
                state.published.insert(file, published);
            }
        }
        (Some("experimental/serverStatus"), None) => {
            state.quiescent = message.pointer("/params/quiescent") == Some(&Value::Bool(true));
        }
        (None, Some(id)) => {
            if let Some(id) = id.as_u64() {
                state.responses.insert(id, message);
            }
        }
        _ => {}
    }
    drop(state);
    shared.changed.notify_all();
}

impl Client {
    /// Starts the server and initializes it within `limit`. `settings` answers the server's configuration
    /// requests; `init` is the `initializationOptions`.
    pub fn start(
        launch: Launch<'_>,
        status: bool,
        settings: Value,
        init: Value,
        limit: Duration,
    ) -> Result<Client, String> {
        let Launch { argv, env, cwd, root } = launch;
        let (program, args) = argv.split_first().ok_or("no command")?;
        let root_uri = uri(root).ok_or_else(|| format!("{} is not an absolute path", root.display()))?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so the server's helpers end with it.
            .process_group(0)
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let (Some(mut stdin), Some(stdout), Some(mut child_stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(format!("{program}: no pipes"));
        };
        let (out, frames) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for frame in frames {
                if stdin.write_all(&frame).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let tail = stderr.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = child_stderr.read(&mut chunk) {
                let mut tail = sync::lock(&tail);
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(MAX_STDERR_BYTES);
                tail.drain(..excess);
            }
        });
        let shared = Arc::new(Shared::default());
        let (reader_shared, reader_out) = (shared.clone(), out.clone());
        let pid = child.id();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_message(&mut reader) {
                incoming(message, &reader_shared, &reader_out, &settings);
            }
            sync::lock(&reader_shared.state).exited = true;
            reader_shared.changed.notify_all();
            let _ = child.wait();
        });
        let mut client = Client {
            pid,
            out,
            shared,
            stderr,
            next_id: AtomicU64::new(1),
            pull: false,
            status,
            docs: Mutex::default(),
        };
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
            "initializationOptions": init,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "versionSupport": true },
                    "diagnostic": {},
                },
                "workspace": { "configuration": true, "workspaceFolders": true },
                "experimental": { "serverStatusNotification": true },
            },
        });
        let response = client
            .request("initialize", params, Instant::now() + limit)
            .map_err(|unready| {
                let said = client.said();
                match unready {
                    Unready::Exited if said.is_empty() => "it exited while starting".to_string(),
                    Unready::Exited => format!("it exited while starting: {said}"),
                    _ => format!("it did not start within {} s", limit.as_secs()),
                }
            })?;
        let Some(result) = response.get("result") else {
            return Err(format!("it refused to start: {}", response["error"]["message"]));
        };
        client.pull = result
            .pointer("/capabilities/diagnosticProvider")
            .is_some_and(|provider| !matches!(provider, Value::Null | Value::Bool(false)));
        client.notify("initialized", json!({}));
        Ok(client)
    }

    /// The last lines the server wrote to stderr, on one line.
    fn said(&self) -> String {
        let text = String::from_utf8_lossy(&sync::lock(&self.stderr)).into_owned();
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
        lines[lines.len().saturating_sub(3)..].join(" | ")
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(frame(&json!({ "jsonrpc": "2.0", "method": method, "params": params })));
    }

    fn wait<T>(&self, deadline: Instant, mut ready: impl FnMut(&mut State) -> Option<T>) -> Result<T, Unready> {
        let mut state = sync::lock(&self.shared.state);
        loop {
            if let Some(value) = ready(&mut state) {
                return Ok(value);
            }
            if state.exited {
                return Err(Unready::Exited);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Unready::Slow);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn request(&self, method: &str, params: Value, deadline: Instant) -> Result<Value, Unready> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // One request is out at a time: an answer still here belongs to a request that was given up on.
        sync::lock(&self.shared.state).responses.clear();
        let _ = self.out.send(frame(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        ));
        self.wait(deadline, |state| state.responses.remove(&id))
    }

    /// True while a server that reports its status has not finished loading the project.
    pub fn loading(&self) -> bool {
        self.status && !sync::lock(&self.shared.state).quiescent
    }

    /// Gives the server `text` as the content of `path` when it differs from what it has.
    fn sync_doc(&self, docs: &mut HashMap<PathBuf, Doc>, path: &Path, language: &str, text: &str) -> bool {
        let Some(uri) = uri(path) else { return false };
        let hash = jsrt::hash::sha256_hex(text);
        let sent_at = sync::lock(&self.shared.state).received;
        match docs.get_mut(path) {
            Some(doc) if doc.hash == hash => return false,
            Some(doc) => {
                doc.version += 1;
                doc.hash = hash;
                doc.sent_at = sent_at;
                self.notify(
                    "textDocument/didChange",
                    json!({ "textDocument": { "uri": uri, "version": doc.version }, "contentChanges": [{ "text": text }] }),
                );
            }
            None => {
                docs.insert(
                    path.to_path_buf(),
                    Doc {
                        version: 1,
                        hash,
                        sent_at,
                    },
                );
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": { "uri": uri, "languageId": language, "version": 1, "text": text } }),
                );
            }
        }
        true
    }

    fn close(&self, docs: &mut HashMap<PathBuf, Doc>, path: &Path) {
        if docs.remove(path).is_some()
            && let Some(uri) = uri(path)
        {
            self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
        }
    }

    /// The errors the server reports for `path` once its content is `content` (which is not on disk yet).
    pub fn check(
        &self,
        path: &Path,
        language: &str,
        content: &str,
        deadline: Instant,
    ) -> Result<Vec<Diagnostic>, Unready> {
        let mut docs = sync::lock(&self.docs);
        // The other open documents follow the disk: `bash` may have changed one, and a write another guard
        // refused never reached it.
        // ponytail: files the server has not open are left to the server's own watcher (gopls has none); send
        // workspace/didChangeWatchedFiles from after-cell changes if stale answers show up.
        let others: Vec<PathBuf> = docs.keys().filter(|open| *open != path).cloned().collect();
        let reset = others.len() + 1 >= MAX_OPEN;
        for other in others {
            match std::fs::read_to_string(&other) {
                Ok(text) if !reset => {
                    self.sync_doc(&mut docs, &other, language, &text);
                }
                _ => self.close(&mut docs, &other),
            }
        }
        let quiet_is_clean = {
            let state = sync::lock(&self.shared.state);
            state
                .published
                .get(path)
                .is_some_and(|last| last.version.is_none() && last.errors.is_empty())
        };
        let changed = self.sync_doc(&mut docs, path, language, content);
        if self.status {
            self.wait(deadline, |state| state.quiescent.then_some(()))
                .map_err(|unready| {
                    if unready == Unready::Slow {
                        Unready::Loading
                    } else {
                        unready
                    }
                })?;
        }
        if self.pull {
            let Some(uri) = uri(path) else { return Ok(Vec::new()) };
            let params = json!({ "textDocument": { "uri": uri } });
            let response = self.request("textDocument/diagnostic", params, deadline)?;
            return match response.get("result") {
                Some(result) => Ok(errors(result.get("items"))),
                // "Content modified", "server cancelled": no answer, not a clean file.
                None => Err(Unready::Slow),
            };
        }
        let Some(doc) = docs.get(path) else {
            return Ok(Vec::new());
        };
        let (version, sent_at) = (doc.version, doc.sent_at);
        let quiet_is_clean = quiet_is_clean && changed;
        let deadline = if quiet_is_clean {
            deadline.min(Instant::now() + QUIET)
        } else {
            deadline
        };
        // ponytail: a publish without a version counts once it arrives after the change was sent; one computed
        // for the text before it and delivered after would be taken for the answer. Servers that version their
        // publishes (pyright, gopls) cannot be misread this way.
        let answer = self.wait(deadline, |state| {
            let last = state.published.get(path)?;
            let fresh = match last.version {
                Some(theirs) => theirs == version,
                None => last.at > sent_at,
            };
            fresh.then(|| (last.version.is_none().then_some(last.at), last.errors.clone()))
        });
        let (mut unversioned, mut found) = match answer {
            Ok(answer) => answer,
            Err(Unready::Slow) if quiet_is_clean => return Ok(Vec::new()),
            Err(unready) => return Err(unready),
        };
        while let Some(at) = unversioned {
            let later = self.wait(deadline.min(Instant::now() + SETTLE), |state| {
                let last = state.published.get(path)?;
                (last.at > at).then(|| (last.at, last.errors.clone()))
            });
            match later {
                Ok((at, errors)) => (unversioned, found) = (Some(at), errors),
                Err(Unready::Slow) => break,
                Err(unready) => return Err(unready),
            }
        }
        Ok(found)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // An exited server's pid may be someone else's by now.
        if !sync::lock(&self.shared.state).exited
            && let Some(pid) = i32::try_from(self.pid).ok().and_then(Pid::from_raw)
        {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}
