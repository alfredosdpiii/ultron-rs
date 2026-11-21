//! Live agy sessions, the default (`ULTRON_AGY_SESSIONS=0` turns them off). agy reaches the lane's bridge through the `ultron` stdio server
//! of its own config (`~/.gemini/config/mcp_config.json`), the only place that raises its 3-minute MCP call timeout.
//! Ultron's model loop builds a runtime per call, and tokio IO stays on the runtime that created it, so every live
//! agy is driven from one process-lifetime runtime thread.
use super::{Agy, Step, delta_prompt};
use crate::env::ProviderEnv;
use crate::types::{AssistantContent, Message, Model, ToolResultMessage, TranscriptContext, Usage, UserContent};
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Below the `timeoutSeconds` (900) of the config entry, so agy never times out a held call Ultron still owns.
const IDLE: Duration = Duration::from_secs(600);
const MAX_LIVE: usize = 4;

type Outcome = io::Result<(AssistantContent, Usage)>;
type Registry = Arc<Mutex<Vec<Live>>>;

struct Live {
    agy: Agy,
    model: String,
    /// How many transcript messages agy has seen, and their `prefix_hash`.
    seen: usize,
    hash: u64,
    used: Instant,
}

struct Job {
    model: Model,
    context: TranscriptContext,
    env: Option<ProviderEnv>,
    reply: oneshot::Sender<Outcome>,
}

/// The relay agy runs as its `ultron` config server; it reaches the lane bridge named in its environment.
const RELAY: &str = include_str!("relay.py");
const RELAY_TIMEOUT_SECONDS: u64 = 900;

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

/// Writes the relay script and the `ultron` entry of `mcp_config.json` when they differ from what Ultron needs.
fn install_relay(config: &Path, path: &str) -> io::Result<()> {
    let python = jsrt::process::which("python3", path).ok_or_else(|| io::Error::other("python3 is not on PATH"))?;
    std::fs::create_dir_all(config)?;
    let script = config.join("ultron-mcp-relay.py");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(RELAY) {
        write_atomic(&script, RELAY.as_bytes())?;
    }
    let file = config.join("mcp_config.json");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut settings: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|e| io::Error::other(format!("{} is not valid JSON: {e}", file.display())))?
    };
    let entry = json!({
        "disabled": false,
        "command": python,
        "args": [script],
        "timeoutSeconds": RELAY_TIMEOUT_SECONDS,
    });
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
        let mut bytes = serde_json::to_vec_pretty(&settings)?;
        bytes.push(b'\n');
        write_atomic(&file, &bytes)?;
    }
    Ok(())
}

fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = target.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, target)
}

pub(super) async fn submit(model: Model, context: TranscriptContext, env: Option<ProviderEnv>) -> Outcome {
    let (reply, outcome) = oneshot::channel();
    worker()
        .send(Job {
            model,
            context,
            env,
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
        registry
            .lock()
            .expect("agy registry")
            .retain(|live| live.used.elapsed() < IDLE);
    }
}

/// Runs one model call. The session is out of the registry meanwhile, so no two calls drive one agy.
async fn run(mut job: Job, registry: Registry) {
    let messages = &job.context.messages;
    let mut slot = take(&registry, &job.model.id, messages);
    let outcome = tokio::select! {
        outcome = serve(&mut slot, &job.model, messages, job.env.as_ref()) => outcome,
        () = job.reply.closed() => {
            // The caller aborted or timed out: stop agy's turn and drop the session.
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
        let mut lives = registry.lock().expect("agy registry");
        lives.push(live);
        while lives.len() > MAX_LIVE {
            let oldest = (0..lives.len()).min_by_key(|&i| lives[i].used).expect("non-empty");
            lives.swap_remove(oldest);
        }
    }
    // The caller may have gone between the outcome and this send; the session is kept either way.
    let _unread = job.reply.send(outcome);
}

fn take(registry: &Registry, model: &str, messages: &[Message]) -> Option<Live> {
    let mut lives = registry.lock().expect("agy registry");
    let index = (0..lives.len())
        .filter(|&i| {
            let live = &lives[i];
            live.model == model && live.seen < messages.len() && prefix_hash(&messages[..live.seen]) == live.hash
        })
        .max_by_key(|&i| lives[i].seen)?;
    Some(lives.swap_remove(index))
}

async fn serve(slot: &mut Option<Live>, model: &Model, messages: &[Message], env: Option<&ProviderEnv>) -> Outcome {
    if let Some(live) = slot.as_mut()
        && let Some(step) = continuation(live, messages)
    {
        let answered = matches!(step, Step::Answer(_));
        match live.agy.advance(step).await {
            Ok(done) => {
                live.seen = messages.len();
                live.hash = prefix_hash(messages);
                return Ok(done);
            }
            // agy may already have run the answered call's continuation; replaying could run tools twice.
            Err(error) if answered => return Err(slot.take().expect("matched").agy.failed(error).await),
            Err(error) => eprintln!("agy session ended, replaying the transcript to a fresh agy: {error}"),
        }
    }
    // No live session continues this transcript, or the one that did failed: replay it to a fresh agy.
    *slot = None;
    let (agy, step) = Agy::start(model, messages, env, true).await?;
    let live = slot.insert(Live {
        agy,
        model: model.id.clone(),
        seen: messages.len(),
        hash: prefix_hash(messages),
        used: Instant::now(),
    });
    match live.agy.advance(step).await {
        Ok(done) => Ok(done),
        Err(error) => Err(slot.take().expect("just inserted").agy.failed(error).await),
    }
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
    match &live.agy.held {
        // agy timed the call out, so it has already ended the step without this result.
        Some((_, reply)) if reply.is_closed() => None,
        Some((id, _)) => match delta {
            [Message::Assistant(call), Message::ToolResult(result)]
                if result.tool_call_id == *id
                    && call
                        .content
                        .iter()
                        .any(|c| matches!(c, AssistantContent::ToolCall(t) if t.id == *id)) =>
            {
                Some(Step::Answer(tool_result(result)))
            }
            _ => None,
        },
        None if live.agy.turn.is_some() => None,
        None => {
            let start = delta.iter().take_while(|m| matches!(m, Message::Assistant(_))).count();
            (start < delta.len())
                .then(|| delta_prompt(&delta[start..]))
                .flatten()
                .map(Step::Prompt)
        }
    }
}

/// A tool result as an MCP `CallToolResult`.
fn tool_result(result: &ToolResultMessage) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|block| match block {
            UserContent::Text(text) => json!({"type": "text", "text": text.text}),
            UserContent::Image(image) => json!({"type": "image", "data": image.data, "mimeType": image.mime_type}),
        })
        .collect();
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
