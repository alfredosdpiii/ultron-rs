//! Live agy sessions, the default (`ULTRON_AGY_SESSIONS=0` turns them off). agy reaches the lane's bridge through the `ultron` stdio server
//! of its own config (`~/.gemini/config/mcp_config.json`), the only place that raises its 3-minute MCP call timeout.
//! Ultron's model loop builds a runtime per call, and tokio IO stays on the runtime that created it, so every live
//! agy is driven from one process-lifetime runtime thread.
use super::{Agy, AgyDelta, Step, WorkspaceDir, delta_prompt, normalize_mime_type};
use crate::env::ProviderEnv;
use crate::types::{
    AssistantContent, Message, Model, ToolResultMessage, TranscriptContext, Usage, UserContent, UserMessageContent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Idle eviction for live `agy` processes (2 min); evicted sessions free memory and reattach via ACP `session/load`.
const IDLE: Duration = Duration::from_mins(2);
const MAX_LIVE: usize = 8;
/// Archived (process-stopped) sessions kept for ACP `session/load` reattachment.
const ARCHIVE_TTL: Duration = Duration::from_hours(24);
const MAX_ARCHIVED: usize = 32;

type Outcome = io::Result<(Vec<AssistantContent>, Usage)>;

#[derive(Default)]
struct Pool {
    live: Vec<Live>,
    archived: Vec<Archived>,
}

type Registry = Arc<Mutex<Pool>>;

struct Live {
    agy: Agy,
    model: String,
    /// How many transcript messages agy has seen, and their `prefix_hash`.
    seen: usize,
    hash: u64,
    used: Instant,
}

/// An evicted or persisted `agy` session whose process was stopped, keeping its ACP `session_id` and workspace so a
/// later turn (including after an Ultron restart) can reattach via `session/load` and send only the delta messages.
struct Archived {
    session_id: String,
    workspace: WorkspaceDir,
    model: String,
    seen: usize,
    hash: u64,
    used: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSession {
    session_id: String,
    workspace: String,
    model: String,
    seen: usize,
    hash: u64,
    saved_ms: u64,
}

fn now_ms() -> u64 {
    jsrt::date::now_ms().max(0) as u64
}

fn store_dir(env: Option<&ProviderEnv>) -> Option<PathBuf> {
    let var = |key: &str| {
        env.and_then(|e| e.get(key).cloned())
            .or_else(|| std::env::var(key).ok())
            .filter(|v| !v.trim().is_empty())
    };
    if let Some(agent_dir) = var("ULTRON_CODING_AGENT_DIR") {
        return Some(PathBuf::from(agent_dir));
    }
    var("HOME").map(|h| PathBuf::from(h).join(".ultron/agent"))
}

impl Live {
    fn is_idle(&self) -> bool {
        self.agy.held.is_none() && self.agy.turn.is_none() && !self.agy.session.is_empty()
    }
}

fn prune_persisted(entries: &mut Vec<PersistedSession>, current_session: &str, now: u64, ttl_ms: u64) {
    entries.retain(|e| {
        let keep = now.saturating_sub(e.saved_ms) < ttl_ms && Path::new(&e.workspace).is_dir();
        if !keep && e.session_id != current_session {
            let _ = std::fs::remove_dir_all(&e.workspace);
        }
        keep && e.session_id != current_session
    });
}

fn trim_persisted(entries: &mut Vec<PersistedSession>) {
    entries.sort_by_key(|e| e.saved_ms);
    while entries.len() > MAX_ARCHIVED {
        let evicted = entries.remove(0);
        let _ = std::fs::remove_dir_all(&evicted.workspace);
    }
}

fn persist_live(live: &Live, env: Option<&ProviderEnv>) {
    if !live.is_idle() {
        return;
    }
    let Some(WorkspaceDir::Persistent(workspace)) = live.agy.workspace.as_ref() else {
        return;
    };
    let Some(dir) = store_dir(env) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file = dir.join("agy-sessions.json");
    let now = now_ms();
    let ttl_ms = ARCHIVE_TTL.as_millis() as u64;
    let mut entries: Vec<PersistedSession> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    prune_persisted(&mut entries, &live.agy.session, now, ttl_ms);
    entries.push(PersistedSession {
        session_id: live.agy.session.clone(),
        workspace: workspace.display().to_string(),
        model: live.model.clone(),
        seen: live.seen,
        hash: live.hash,
        saved_ms: now,
    });
    trim_persisted(&mut entries);
    if let Ok(bytes) = serde_json::to_vec_pretty(&entries) {
        let _ = write_atomic(&file, &bytes);
    }
}

fn load_persisted(model: &str, messages: &[Message], env: Option<&ProviderEnv>) -> Option<Archived> {
    let dir = store_dir(env)?;
    let file = dir.join("agy-sessions.json");
    let entries: Vec<PersistedSession> = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let now = now_ms();
    let ttl_ms = ARCHIVE_TTL.as_millis() as u64;
    entries
        .into_iter()
        .filter(|e| {
            e.model == model
                && now.saturating_sub(e.saved_ms) < ttl_ms
                && e.seen < messages.len()
                && Path::new(&e.workspace).is_dir()
                && prefix_hash(&messages[..e.seen]) == e.hash
        })
        .max_by_key(|e| e.seen)
        .map(|e| Archived {
            session_id: e.session_id,
            workspace: WorkspaceDir::Persistent(PathBuf::from(e.workspace)),
            model: e.model,
            seen: e.seen,
            hash: e.hash,
            used: Instant::now(),
        })
}

fn session_workspace(env: Option<&ProviderEnv>) -> io::Result<WorkspaceDir> {
    if let Some(dir) = store_dir(env) {
        let path = dir.join("agy-workspaces").join(jsrt::random::random_hex(8));
        std::fs::create_dir_all(&path)?;
        return Ok(WorkspaceDir::Persistent(path));
    }
    Ok(WorkspaceDir::Temp(tempfile::tempdir()?))
}

impl Pool {
    fn archive(&mut self, mut live: Live) {
        if live.agy.held.is_some() || live.agy.turn.is_some() || live.agy.session.is_empty() {
            return;
        }
        let Some(workspace) = live.agy.workspace.take() else {
            return;
        };
        self.archived.push(Archived {
            session_id: std::mem::take(&mut live.agy.session),
            workspace,
            model: live.model,
            seen: live.seen,
            hash: live.hash,
            used: live.used,
        });
        while self.archived.len() > MAX_ARCHIVED {
            let oldest = (0..self.archived.len())
                .min_by_key(|&i| self.archived[i].used)
                .expect("non-empty");
            self.archived.swap_remove(oldest);
        }
    }
}

struct Job {
    model: Model,
    context: TranscriptContext,
    env: Option<ProviderEnv>,
    deltas: mpsc::UnboundedSender<AgyDelta>,
    reply: oneshot::Sender<Outcome>,
}

/// The relay agy runs as its `ultron` config server; it reaches the lane bridge named in its environment.
const RELAY: &str = include_str!("relay.py");
/// Negative `timeoutSeconds` disables agy's MCP connect and per-tool-call timeouts (`docs/mcp.md`), so long `rlm`
/// cells never get cancelled by agy.
const RELAY_TIMEOUT_SECONDS: i64 = -1;

/// Session mode is the default; `ULTRON_AGY_SESSIONS=0` turns it off. It needs Ultron's relay entry in agy's config,
/// which this writes (leaving every other entry alone); when it cannot, agy runs per model call.
pub(super) fn enabled(env: Option<&ProviderEnv>) -> bool {
    let var = |key: &str| {
        env.and_then(|e| e.get(key).cloned())
            .or_else(|| std::env::var(key).ok())
    };
    if var("ULTRON_AGY_SESSIONS").as_deref() == Some("0") {
        return false;
    }
    let config = Path::new(&var("HOME").unwrap_or_default()).join(".gemini/config");
    match install_relay(&config, &var("PATH").unwrap_or_default()) {
        Ok(()) => true,
        Err(error) => {
            static WARNED: Once = Once::new();
            WARNED.call_once(|| eprintln!("agy session mode is off ({error}); starting agy per model call"));
            false
        }
    }
}

/// Ensures `~/.gemini/config/ultron-mcp-relay.py` and the `ultron` entry of `~/.gemini/config/mcp_config.json` are
/// installed for both `agy acp` session mode and `ultron agy`.
pub fn ensure_relay(home: &Path, path: &str) -> io::Result<()> {
    install_relay(&home.join(".gemini/config"), path)
}

fn relay_entry(python: PathBuf, script: &Path) -> Value {
    json!({
        "disabled": false,
        "command": python,
        "args": [script],
        "timeoutSeconds": RELAY_TIMEOUT_SECONDS,
        "tools": {
            "rlm": {
                "eager": true,
                "background": "off",
            }
        },
    })
}

fn load_mcp_config(file: &Path) -> io::Result<Value> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error),
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| io::Error::other(format!("{} is not valid JSON: {e}", file.display())))
}

fn upsert_ultron_entry(file: &Path, settings: &mut Value, entry: Value) -> io::Result<()> {
    let servers = settings
        .as_object_mut()
        .ok_or_else(|| io::Error::other(format!("{} is not a JSON object", file.display())))?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| io::Error::other(format!("mcpServers in {} is not an object", file.display())))?;
    if servers.get("ultron") != Some(&entry) {
        servers.insert("ultron".into(), entry);
        let mut bytes = serde_json::to_vec_pretty(settings)?;
        bytes.push(b'\n');
        write_atomic(file, &bytes)?;
    }
    Ok(())
}

/// Writes the relay script and the `ultron` entry of `mcp_config.json` when they differ from what Ultron needs.
fn install_relay(config: &Path, path: &str) -> io::Result<()> {
    let python = jsrt::process::which("python3", path).ok_or_else(|| io::Error::other("python3 is not on PATH"))?;
    std::fs::create_dir_all(config)?;
    let script = config.join("ultron-mcp-relay.py");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(RELAY) {
        write_atomic(&script, RELAY.as_bytes())?;
    }
    let file = config.join("mcp_config.json");
    let mut settings = load_mcp_config(&file)?;
    upsert_ultron_entry(&file, &mut settings, relay_entry(python, &script))
}

fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    jsrt::fs::atomic_write(target, bytes)
}

pub(super) async fn submit(
    model: Model,
    context: TranscriptContext,
    env: Option<ProviderEnv>,
    deltas: mpsc::UnboundedSender<AgyDelta>,
) -> Outcome {
    let (reply, outcome) = oneshot::channel();
    worker()
        .send(Job {
            model,
            context,
            env,
            deltas,
            reply,
        })
        .map_err(|_| io::Error::other("the agy session thread stopped"))?;
    outcome
        .await
        .map_err(|_| io::Error::other("the agy session thread dropped the request"))?
}

fn worker() -> &'static mpsc::UnboundedSender<Job> {
    static WORKER: OnceLock<mpsc::UnboundedSender<Job>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, mut jobs) = mpsc::unbounded_channel::<Job>();
        std::thread::Builder::new()
            .name("agy-sessions".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("agy session runtime");
                runtime.block_on(async move {
                    let registry = Registry::default();
                    tokio::spawn(reap(registry.clone()));
                    while let Some(job) = jobs.recv().await {
                        tokio::spawn(run(job, registry.clone()));
                    }
                });
            })
            .expect("spawn the agy session thread");
        sender
    })
}

async fn reap(registry: Registry) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        let mut pool = jsrt::sync::lock(&registry);
        let mut i = 0;
        while i < pool.live.len() {
            if pool.live[i].used.elapsed() >= IDLE {
                let evicted = pool.live.swap_remove(i);
                pool.archive(evicted);
            } else {
                i += 1;
            }
        }
        pool.archived.retain(|a| a.used.elapsed() < ARCHIVE_TTL);
    }
}

/// Runs one model call. The session is out of the registry meanwhile, so no two calls drive one agy.
async fn run(mut job: Job, registry: Registry) {
    let messages = &job.context.messages;
    let (mut slot, archived) = take(&registry, &job.model.id, messages, job.env.as_ref());
    let outcome = tokio::select! {
        outcome = serve(&mut slot, archived, &job.model, messages, job.env.as_ref(), &job.deltas) => outcome,
        () = job.reply.closed() => {
            if let Some(live) = slot.as_mut() {
                let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":live.agy.session}});
                match tokio::time::timeout(Duration::from_secs(2), live.agy.send(cancel)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("agy session/cancel: {error}"),
                    Err(_) => eprintln!("agy session/cancel: no room in agy's stdin after 2 s"),
                }
            }
            return;
        }
    };
    if outcome.is_ok()
        && !job.reply.is_closed()
        && let Some(mut live) = slot
    {
        live.used = Instant::now();
        persist_live(&live, job.env.as_ref());
        let mut pool = jsrt::sync::lock(&registry);
        pool.live.push(live);
        while pool.live.len() > MAX_LIVE {
            let oldest = (0..pool.live.len())
                .min_by_key(|&i| pool.live[i].used)
                .expect("non-empty");
            let evicted = pool.live.swap_remove(oldest);
            pool.archive(evicted);
        }
    }
    let _unread = job.reply.send(outcome);
}

fn take(
    registry: &Registry,
    model: &str,
    messages: &[Message],
    env: Option<&ProviderEnv>,
) -> (Option<Live>, Option<Archived>) {
    let mut pool = jsrt::sync::lock(registry);
    if let Some(index) = (0..pool.live.len())
        .filter(|&i| {
            let live = &pool.live[i];
            live.model == model && live.seen < messages.len() && prefix_hash(&messages[..live.seen]) == live.hash
        })
        .max_by_key(|&i| pool.live[i].seen)
    {
        return (Some(pool.live.swap_remove(index)), None);
    }
    let archived = (0..pool.archived.len())
        .filter(|&i| {
            let a = &pool.archived[i];
            a.model == model && a.seen < messages.len() && prefix_hash(&messages[..a.seen]) == a.hash
        })
        .max_by_key(|&i| pool.archived[i].seen)
        .map(|index| pool.archived.swap_remove(index));
    drop(pool);
    (None, archived.or_else(|| load_persisted(model, messages, env)))
}

async fn try_continue_live(
    slot: &mut Option<Live>,
    messages: &[Message],
    deltas: &mpsc::UnboundedSender<AgyDelta>,
) -> Option<Outcome> {
    let live = slot.as_mut()?;
    let step = continuation(live, messages)?;
    let answered = matches!(step, Step::Answer(_));
    match live.agy.advance(step, Some(deltas)).await {
        Ok(done) => {
            live.seen = messages.len();
            live.hash = prefix_hash(messages);
            Some(Ok(done))
        }
        Err(error) if answered => Some(Err(slot.take().expect("matched").agy.failed(error).await)),
        Err(error) => {
            eprintln!("agy session ended, replaying the transcript to a fresh agy: {error}");
            None
        }
    }
}

async fn try_resume_archived(
    slot: &mut Option<Live>,
    archived: Archived,
    model: &Model,
    messages: &[Message],
    env: Option<&ProviderEnv>,
    deltas: &mpsc::UnboundedSender<AgyDelta>,
) -> Option<Outcome> {
    let delta = &messages[archived.seen..];
    if delta.iter().any(|m| matches!(m, Message::System(_))) {
        return None;
    }
    let prompt = delta_prompt_after_assistant(delta)?;
    let agy = match Agy::resume(model, messages, env, archived.session_id, archived.workspace).await {
        Ok(agy) => agy,
        Err(error) => {
            eprintln!("agy session/load failed, replaying to a fresh agy: {error}");
            return None;
        }
    };
    let live = slot.insert(Live {
        agy,
        model: model.id.clone(),
        seen: messages.len(),
        hash: prefix_hash(messages),
        used: Instant::now(),
    });
    match live.agy.advance(Step::Prompt(prompt), Some(deltas)).await {
        Ok(done) => Some(Ok(done)),
        Err(error) => {
            eprintln!("agy resumed session failed, replaying to a fresh agy: {error}");
            None
        }
    }
}

async fn serve(
    slot: &mut Option<Live>,
    archived: Option<Archived>,
    model: &Model,
    messages: &[Message],
    env: Option<&ProviderEnv>,
    deltas: &mpsc::UnboundedSender<AgyDelta>,
) -> Outcome {
    if let Some(outcome) = try_continue_live(slot, messages, deltas).await {
        return outcome;
    }
    if let Some(archived) = archived
        && let Some(outcome) = try_resume_archived(slot, archived, model, messages, env, deltas).await
    {
        return outcome;
    }
    *slot = None;
    let workspace = session_workspace(env)?;
    let (agy, step) = Agy::start_in(model, messages, env, true, workspace).await?;
    let live = slot.insert(Live {
        agy,
        model: model.id.clone(),
        seen: messages.len(),
        hash: prefix_hash(messages),
        used: Instant::now(),
    });
    match live.agy.advance(step, Some(deltas)).await {
        Ok(done) => Ok(done),
        Err(error) => Err(slot.take().expect("just inserted").agy.failed(error).await),
    }
}

fn answer_held_call(id: &str, delta: &[Message]) -> Option<Step> {
    let [Message::Assistant(call), Message::ToolResult(result), trailing @ ..] = delta else {
        return None;
    };
    let matches_call = result.tool_call_id == id
        && call
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(t) if t.id == id));
    if !matches_call || !trailing.iter().all(|m| matches!(m, Message::User(_))) {
        return None;
    }
    Some(Step::Answer(tool_result(result, trailing)))
}

fn delta_prompt_after_assistant(delta: &[Message]) -> Option<Vec<Value>> {
    let start = delta.iter().take_while(|m| matches!(m, Message::Assistant(_))).count();
    (start < delta.len()).then(|| delta_prompt(&delta[start..])).flatten()
}

/// The step that feeds agy the messages after those it has seen, if they continue its session: the result of its
/// held tool call, or new messages after its finished turn.
fn continuation(live: &mut Live, messages: &[Message]) -> Option<Step> {
    if !matches!(live.agy.child.try_wait(), Ok(None)) {
        return None;
    }
    let delta = &messages[live.seen..];
    if delta.iter().any(|m| matches!(m, Message::System(_))) {
        return None;
    }
    if let Some((id, reply)) = &live.agy.held {
        return (!reply.is_closed()).then(|| answer_held_call(id, delta)).flatten();
    }
    if live.agy.turn.is_some() {
        return None;
    }
    delta_prompt_after_assistant(delta).map(Step::Prompt)
}

/// A tool result as an MCP `CallToolResult`, carrying any `User` messages (runtime events, steering) that arrived
/// while the tool ran so the live `agy` turn continues instead of restarting with a truncated replay.
fn tool_result(result: &ToolResultMessage, trailing: &[Message]) -> Value {
    let mut content: Vec<Value> = result
        .content
        .iter()
        .map(|block| match block {
            UserContent::Text(text) => json!({"type": "text", "text": text.text}),
            UserContent::Image(image) => json!({
                "type": "image",
                "data": image.data,
                "mimeType": normalize_mime_type(&image.mime_type)
            }),
        })
        .collect();
    for message in trailing {
        let Message::User(user) = message else {
            continue;
        };
        match &user.content {
            UserMessageContent::Text(text) => {
                content.push(json!({"type": "text", "text": format!("[Message received while tool ran]\n{text}")}));
            }
            UserMessageContent::Blocks(blocks) => {
                content.push(json!({"type": "text", "text": "[Message received while tool ran]"}));
                for block in blocks {
                    match block {
                        UserContent::Text(text) => content.push(json!({"type": "text", "text": text.text})),
                        UserContent::Image(image) => {
                            content.push(json!({
                                "type": "image",
                                "data": image.data,
                                "mimeType": normalize_mime_type(&image.mime_type)
                            }));
                        }
                    }
                }
            }
        }
    }
    json!({"content": content, "isError": result.is_error})
}

/// What identifies a transcript prefix. Assistant and tool messages count by role, content and call id only, since
/// Ultron annotates stored messages (usage, timestamps) after agy produced them.
fn prefix_hash(messages: &[Message]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for message in messages {
        let value = serde_json::to_value(message).expect("transcript messages serialize");
        let key = match message {
            Message::System(_) => value,
            _ => json!([value["role"], value["content"], value["toolCallId"]]),
        };
        key.to_string().hash(&mut hasher);
    }
    hasher.finish()
}
