//! The durable many-agent APIs under `ultron claude` (swarm_api.py): persistent children (`rlm.spawn(keep=True)`,
//! `rlm.ask`), the swarm (`swarm.*`), the task board (`board.*`) and shared notes (`notes.*`), served by the same
//! code as in a native session (session_core::swarm_ops over `harness::chat::host_ops`), on a durable runtime of the
//! server's own Ultron session.
//!
//! A native session's durable runtime runs the root's turns too; here Claude Code owns the root, so the runtime runs
//! only child conversations (a persistent child, and its own persistent children): the root conversation exists, so
//! the root lane's requests are its conversation's, but it never runs (no request of it reaches the model). The
//! runtime's journal is in the session (session_core::durable_store), so persistent children, the board, the notes
//! and the swarm's messages survive a restart and `claude --resume` reopens them.
//!
//! - A persistent child is a conversation of this runtime, never a Claude Code process. Its model is `model=` of the
//!   spawn, else [`keep_model`]: `--ultron-child-model` / `claudeCode.ultronChildModel`, else `rlm.childModel` when
//!   it names a Claude Code model, else the root's own subscription model (`claude-code/<id>`). A `claude-code`
//!   request that declares tools runs on the Claude subscription only, inside this conversation's transcript
//!   (ai::claude_subscription::claude_code_lane): the child has `rlm` and takes follow-ups (`rlm.ask`), and nothing
//!   falls onto API credits unless the spawn or a setting names such a model.
//! - Its model turns are charged to the session's usage ledger as a native session charges a persistent child (the
//!   ledger's default root), and the session's budget gate stops a spent tree's requests.
//! - `swarm.send(to, text, wake=True)` to the root (Claude Code: the name it registered, else `root`) cannot start
//!   a turn of the runtime's root conversation: the message is sent, and also goes to the root's inbox as a
//!   `<swarm_message>` line, which the next cell result or the wait-events hook brings to Claude Code
//!   (crate::hooks::HookEvent::WaitEvents).
//! - A subagent with no conversation here (an in-memory `--children ultron` subagent, a Claude Code subagent's
//!   server) uses the swarm, board and notes by its spawn name ([`DurableApis::serve_as`], crate::shared_world);
//!   persistent children and `rlm.ask` stay with conversations ([`subagent_refusal`]).
//! - Typed agents' `rlm` lanes run here too, as agent child conversations (crate::typed_agents).

use crate::root_sink::RootSink;
use ai::retry::RetryPolicy;
use durable::{ConversationId, ROOT_CONVERSATION_ID};
use harness::chat::{AgentChange, ChatHarness, ModelRef, Settings};
use serde_json::{Map, Value, json};
use session_core::compose::{Runtime, Stack};
use session_core::core::durable_turns::{ChildTurns, child_chat_options};
use session_core::durable_store::SharedSession;
use session_core::lane::Lane;
use session_core::request_keys::Keys;
use session_core::subagents::{CONVERSATION_LANE_PREFIX, DurableChildren};
use session_core::swarm_ops::SwarmOps;
use session_core::turns::Bridge;
use session_journal::run::MAIN_LANE;
use std::sync::Arc;

/// Whether `payload` of an `rlm.spawn` asks for a persistent child.
pub fn is_keep_spawn(kind: &str, payload: &Map<String, Value>) -> bool {
    let keep = |value: Option<&Value>| value == Some(&Value::Bool(true));
    kind == "rlm.spawn" && (keep(payload.get("keep")) || keep(payload.get("kwargs").and_then(|k| k.get("keep"))))
}

/// The model persistent children run on when the spawn names none (crate::mcp_server::ultron_child_model, over
/// `rlm.childModel` and the root's `subscription` model); a named model that is not `provider/model` falls through
/// to the subscription model.
pub fn keep_model(
    resolved: &crate::mcp_server::Resolved,
    settings_child_model: Option<&str>,
    subscription: &str,
) -> ModelRef {
    let chosen = crate::mcp_server::ultron_child_model(resolved, settings_child_model, subscription);
    let reference =
        |model: &str| crate::mcp_server::split_model(model.trim()).map(|(provider, id)| ModelRef::new(&provider, &id));
    reference(&chosen)
        .or_else(|| reference(subscription))
        .unwrap_or_else(|| ModelRef::new(ai::claude_code::PROVIDER, config::providers::CLAUDE_CODE_DEFAULT_MODEL))
}

/// The durable runtime of an `ultron mcp` root and the APIs served over it.
pub struct DurableApis {
    pub chat: ChatHarness,
    pub children: Arc<DurableChildren>,
    pub swarm: Arc<SwarmOps>,
    /// Where persistent children run when the spawn names no model.
    pub model: ModelRef,
    sink: Arc<RootSink>,
}

/// Open the durable runtime of `children` over `session` for `lane`'s stack and serve the APIs from it (sets
/// `lane.stack.swarm`). `children` is created before the lane: typed agents' `rlm` lanes run on it too
/// (crate::typed_agents).
pub fn open(
    children: Arc<DurableChildren>,
    session: &SharedSession,
    lane: &Lane,
    runtime: &Runtime,
    env: &session_core::RunEnv,
    model: ModelRef,
    sink: Arc<RootSink>,
) -> Result<Arc<DurableApis>, String> {
    let stack = lane.stack.clone();
    // The children's session (their contracts are values of it, session_core::kept_contracts). This server's
    // subagents are Claude Code processes or in-memory loops.
    children.attach_session(session);
    let storage = session_core::durable_store::open(session.clone(), None).map_err(|e| e.message().to_string())?;
    // Nothing reaches an actor: the root conversation never runs, and children's requests never use the bridge.
    let bridge = Bridge::new(Arc::new(|_| {}));
    let provider = Arc::new(session_core::lane::Provider::new().with_live_settings(runtime.config.settings_reader()));
    let retry = RetryPolicy::from_settings(&runtime.config.settings["retry"], true);
    // Requests of no conversation carry this provider session: every request is a child's.
    let root_session = Arc::new(std::sync::OnceLock::new());
    let _ = root_session.set(format!("ultron-claude-root-{}", jsrt::random::random_hex(8)));
    let options = child_chat_options(
        session,
        lane,
        &children,
        env,
        ChildTurns {
            bridge,
            provider,
            keys: Keys::session(None, "", env),
            root_session,
            live: Arc::new(move |settings: &mut Settings| settings.retry = retry),
            charged: None,
            transform: None,
            image_budget: runtime.config.context_images,
            // A child conversation's system prompt is a subagent's.
            fallback_system: |stack: &Stack| stack.child_system(stack.model()),
        },
    );
    let chat = ChatHarness::open(storage, options).map_err(|e| e.to_string())?;
    let root = chat.root(None).map_err(|e| e.to_string())?;
    // A persistent child spawned without `model=` runs on its parent conversation's model.
    root.configure(&AgentChange::model(model.clone()))
        .map_err(|e| e.to_string())?;
    children.attach(chat.clone());
    let swarm = Arc::new(SwarmOps::new(children.clone()).with_contracts(Arc::downgrade(&stack)));
    let _ = stack.swarm.set(swarm.clone());
    // Whatever the runtime holds (persistent children of an earlier server) continues now.
    chat.resume().map_err(|e| e.to_string())?;
    Ok(Arc::new(DurableApis {
        chat,
        children,
        swarm,
        model,
        sink,
    }))
}

/// The name an agent goes by in the swarm: what it registered for `conversation`, else `root` for the root.
fn agent_name(agents: &Value, conversation: ConversationId) -> String {
    agents
        .as_object()
        .and_then(|agents| {
            agents
                .iter()
                .find(|(_, entry)| entry["conversation"].as_u64() == Some(conversation.0))
                .map(|(name, _)| name.clone())
        })
        .unwrap_or_else(|| {
            if conversation == ROOT_CONVERSATION_ID {
                "root".into()
            } else {
                format!("conversation-{}", conversation.0)
            }
        })
}

impl DurableApis {
    /// The conversation `lane` works in, if it has one in this runtime (the root lane, a persistent child's lane).
    pub fn conversation(&self, lane: &str) -> Option<ConversationId> {
        if lane == MAIN_LANE {
            return Some(ROOT_CONVERSATION_ID);
        }
        if let Some(id) = lane.strip_prefix(CONVERSATION_LANE_PREFIX) {
            return id.parse().ok().map(ConversationId);
        }
        self.children.conversation_of_lane(lane)
    }

    /// Why `kind` with `payload` from `lane` cannot be served here, if it cannot; `None` passes it on: persistent
    /// children and `rlm.ask` belong to conversations ([`subagent_refusal`]), on any model (a Claude Code one runs on
    /// the subscription with `rlm`). (The swarm, board and notes of a lane with no conversation here are served by
    /// [`DurableApis::serve_as`].)
    pub fn refusal(&self, lane: &str, kind: &str, payload: &Map<String, Value>) -> Option<String> {
        let durable = kind == "rlm.ask" || is_keep_spawn(kind, payload);
        (durable && self.conversation(lane).is_none()).then(|| subagent_refusal(kind))
    }

    /// `swarm.send(..., wake=True)` from `lane` whose recipient is the root: the message is sent, and the root
    /// (Claude Code) gets it in its inbox instead of a turn of the runtime's root conversation. `None`: not such a
    /// request (pass it on).
    pub fn wake_root(
        &self,
        lane: &str,
        kind: &str,
        payload: &Map<String, Value>,
        next: session_core::request_layer::Next<'_>,
    ) -> Option<Result<Value, String>> {
        if kind != "swarm.send" || payload.get("wake") != Some(&Value::Bool(true)) {
            return None;
        }
        let to = payload.get("to").and_then(Value::as_str)?;
        let agents = self.chat.commit(harness::chat::swarm::agents).unwrap_or(Value::Null);
        // The root's name: what it registered, else `root` (its name in the swarm until it registers another).
        let root = match agents.get(to) {
            Some(entry) => entry["conversation"].as_u64() == Some(ROOT_CONVERSATION_ID.0),
            None => to == "root",
        };
        if !root {
            return None;
        }
        let mut quiet = payload.clone();
        quiet.insert("wake".into(), false.into());
        let sent = match next(quiet) {
            Ok(sent) => sent,
            Err(error) => return Some(Err(error)),
        };
        let from = self
            .conversation(lane)
            .map_or_else(|| lane.to_string(), |conversation| agent_name(&agents, conversation));
        let text = payload.get("text").and_then(Value::as_str).unwrap_or_default();
        self.publish(&from, to, text);
        Some(Ok(json!({"id": sent["id"], "woke": true})))
    }

    /// `kind` (one of [`harness::chat::host_ops::SWARM_KINDS`]) for a subagent with no conversation here, by its `name`: an in-memory
    /// subagent of this server, or a Claude Code subagent whose server forwarded it (crate::shared_world). It is
    /// served as `harness::chat::host_ops` serves a conversation's request, the subagent being the agent called
    /// `name` (what it adds, appends, claims and sends is by `name`; its inbox is `name`'s): a wake to the root goes
    /// to the root's inbox ([`DurableApis::wake_root`]), one to a persistent child starts its next turn.
    pub fn serve_as(&self, name: &str, kind: &str, payload: &Map<String, Value>) -> Result<Value, String> {
        let publish = |from: &str, to: &str, text: &str| self.publish(from, to, text);
        let caller = harness::chat::swarm::Caller {
            name: name.to_string(),
            conversation: None,
            root_inbox: Some(&publish),
        };
        harness::chat::swarm::serve(&self.chat, kind, payload, &caller)
    }

    /// A `<swarm_message>` from `from` to the root (`to`, its name) in the root's inbox.
    fn publish(&self, from: &str, to: &str, text: &str) {
        self.sink.publish(format!(
            "<swarm_message from=\"{}\" to=\"{}\">\n{text}\n</swarm_message>",
            attribute(from),
            attribute(to)
        ));
    }

    /// Stop the runtime (persistent children stay in the journal, for the next server of this session).
    pub fn close(&self) {
        let _ = self.chat.close();
    }
}

/// Why a subagent with no conversation of its own (an in-memory subagent, a Claude Code subagent) cannot use `kind`
/// (`rlm.ask` or `rlm.spawn(keep=True)`).
///
/// Not forwarded to the root: a persistent child belongs to the conversation that spawned it (its answers, `rlm.ask`
/// and `rlm.collect` go there), and a subagent has none: spawned for it, the child would be the root's, outliving
/// the subagent that alone knows why it exists. `rlm.ask` holds the asker's cell until the child answers (up to
/// 20 minutes), longer than a forwarded request should hold the parent's control socket.
pub fn subagent_refusal(kind: &str) -> String {
    let api = if kind == "rlm.ask" {
        "rlm.ask"
    } else {
        "rlm.spawn(keep=True)"
    };
    format!(
        "{api} is not available to a subagent in ultron claude: persistent children belong to the root and its persistent children; use swarm.send, board and notes to reach them, and put what the root needs in your answer"
    )
}

/// Text as an XML attribute value.
pub fn attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
