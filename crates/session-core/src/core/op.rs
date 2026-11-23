//! Client operations ([`Op`]) served by the session actor and their JSON wire representation.

use protocol::watch::QueueKind;
use serde_json::{Value, json};

/// The operations a client sends (the commands of `rpc-native.ts` the session worker serves, and the agent
/// controller's members). Session selection (new, switch, fork, clone) is the server's: clients re-attach.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Start a run; with a run in progress the message joins `queue` (none: refused).
    Prompt {
        message: String,
        images: Vec<Value>,
        queue: Option<QueueKind>,
    },
    /// Start a run and answer when it ends (`{accepted, operationId, error}`), as the agent controller does.
    PromptAndWait {
        message: String,
        images: Vec<Value>,
    },
    /// Queue a message (as `entry_id` when the client chose it); answers its entry id.
    Enqueue {
        kind: QueueKind,
        message: String,
        images: Vec<Value>,
        entry_id: Option<String>,
    },
    /// Drop one queued message; answers whether it was queued.
    CancelQueued {
        entry_id: String,
    },
    /// Empty the queues: `{steering, followUp}` texts.
    ClearQueue,
    Abort,
    State,
    AvailableModels,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    ThinkingLevels,
    SetThinking {
        level: String,
    },
    CycleThinking,
    SetQueueMode {
        kind: QueueKind,
        mode: String,
    },
    Compact {
        instructions: Option<String>,
    },
    SetAutoCompaction(bool),
    SetAutoRetry(bool),
    /// The terminal appearance the attached TUI detected (`"light"` / `"dark"`: OSC 11, a scheme report, else its
    /// `COLORFGBG`): what `diagram.mermaid`'s `auto` theme follows ([`crate::compose::Config::terminal_theme`]).
    SetTerminalTheme {
        theme: String,
    },
    AbortRetry,
    /// A user's own command (RPC `bash`, the TUI's `!`/`!!`), run by the shell.
    /// `id` tags its streamed output (`bash_execution_update`), as AgentSession's `executeBash` `options.id`.
    Bash {
        command: String,
        exclude: bool,
        id: Option<String>,
    },
    AbortBash,
    /// `/bug`'s summary written by the session's model (AgentSession `summarizeForBugReport`): the report text.
    BugSummary {
        hint: Option<String>,
    },
    /// Cancel the bug report summary in progress (its loader's Esc).
    AbortBugSummary,
    /// What a shared or reported session carries besides its entries (session-share.ts
    /// `createShareTrailingEntries`): `{ systemPrompt, tools: [{ name, description, parameters }] }`.
    ShareContext,
    /// `sessionManager.appendCustomEntry(customType, data)` (`/bug` records its report): after the run in progress.
    AppendCustom {
        custom_type: String,
        data: Value,
    },
    Stats,
    ForkMessages,
    Entries {
        since: Option<String>,
    },
    Tree,
    LastAssistantText,
    SetName {
        name: String,
    },
    Messages,
    /// The text of the user message `entry_id`, which a fork starts before (an error for any other entry).
    ForkSource {
        entry_id: String,
    },
    /// The current Pi leaf (an error without one), which a clone copies to.
    CloneSource,
    /// Record the session-start marker in the forked session file `target` (`control.forked`), as TypeScript
    /// records it: its first worker removes it (TypeScript's tells its extensions the session started as a fork).
    Forked {
        target: String,
    },
    /// The whole session as the TUI shows it: `{sessionId, sessionFile, name, messages, model, thinkingLevel}`.
    Describe,
    /// The main lane's snapshot as transcript state starts from ([`super::Core::snapshot`]): what a watcher rebases on after
    /// a navigation, which its events cannot fold.
    LaneSnapshot,
    /// Sessions of this working directory, newest first (`{path, id, name, modified, messageCount, firstMessage,
    /// allMessagesText, cwd}`).
    ListSessions,
    /// Continue what a dead worker left (crate::recovery): the first queued message runs, else an interrupted run
    /// continues. Answers `{resumed, runId, reason, recovered}`; nothing starts while a run is in progress.
    Recover,
    /// The subagents of the root lane (`rlm.list_subagents` as the root sees them: `{subagents: [...]}`), for the
    /// TUI's agent hub.
    Agents,
    /// Move `main`'s tip to `target_id` (`None`: the root), optionally summarizing the branch left (`/tree`), and
    /// label the target (or the branch summary).
    Navigate {
        target_id: Option<String>,
        summarize: bool,
        instructions: Option<String>,
        label: Option<String>,
    },
    /// Label a tree entry (`None` removes the label).
    SetLabel {
        entry_id: String,
        label: Option<String>,
    },
    /// Cancel a subagent and its unfinished descendants (the agent hub's kill).
    KillAgent {
        id: String,
    },
    /// Steer a subagent with `message` (the agent hub's steer: `agent_message.send` from the root lane with
    /// `receiver_role: "child"` and `steer: true`); answers the family module's `{id, duplicate, state, steered}`.
    SteerAgent {
        id: String,
        message: String,
    },
    /// Start an interrupted subagent again from its brief (the agent hub's revive); answers `{id, revived}`.
    ReviveAgent {
        id: String,
    },
    /// A subagent's transcript (`agents.transcript`): `{id, busy, messages}`, its child conversation's messages.
    AgentTranscript {
        id: String,
    },
    /// `/goal <args>`: the session goal's command (set, pause, resume, clear, check); answers its text.
    Goal {
        args: String,
    },
    Observer {
        args: String,
    },
    /// Start another root conversation in the session's durable runtime with `message` (Pi Durable's ownerless
    /// conversations: each runs, survives restarts and resumes on its own); answers `{conversationId}`.
    StartConversation {
        message: String,
    },
    /// Give conversation `id` another input (queued while it runs).
    ConversationInput {
        id: u64,
        message: String,
    },
    /// Conversation `id`: `{id, busy, messages: [{role, text}]}`; without an id, every root conversation's.
    Conversations {
        id: Option<u64>,
    },
    /// Root-owned work whose completion may still re-invoke the root (`async.pending`): running jobs and subagents
    /// of the root lane, events waiting for delivery, a run in progress.
    AsyncPending,
    /// A read-only inspection request (crate::inspect: `agents.status`, `memory.list`, `rlm.frames`, ...) answered
    /// by the session's host stack as the root lane's; anything else is refused.
    Inspect {
        request: String,
        payload: Value,
    },
    /// The session's slash commands (RPC `get_commands`: crate::commands).
    Commands,
    /// The session's working-tree checkpoints (core::checkpoints): `{checkpoints: [...], enabled}`.
    Checkpoints,
    /// Restore the working tree's files from checkpoint `checkpoint`, else from the checkpoint of entry `entry_id`
    /// (the start of its turn), saving the current files as another checkpoint first.
    RestoreFiles {
        entry_id: Option<String>,
        checkpoint: Option<String>,
    },
    /// Run scheduled prompt `id` for its slot `due` (the server's timer, crates/daemon `prompt_schedules`): the
    /// prompt starts a run, or waits for the run in progress to end (core::schedule_ops). Answers
    /// `{fired, run, schedules}`.
    ScheduleFire {
        id: String,
        due: i64,
    },
    /// Cancel the active schedule `id` (or name); answers it.
    ScheduleCancel {
        id: String,
    },
    /// `/persona on|off|auto` (empty: report): the session's own persona toggle over the settings'
    /// (resources::keep_going). Answers `{text, persona, effective, keepGoing}`.
    Persona {
        args: String,
    },
    /// `/keepgoing on|off` (empty: report): the session's keep-going mode. Answers as [`Op::Persona`].
    KeepGoing {
        args: String,
    },
    /// `/continue`: this new session continues session `session` by searching its log (crate::continuation).
    /// Answers `{session, name}`.
    ContinueFrom {
        session: String,
    },
}

/// The JSON request a client sends for an operation ([`Op::to_json`]).
impl From<&Op> for Value {
    fn from(op: &Op) -> Value {
        op.to_json()
    }
}

impl Op {
    /// The operation a JSON request names (`{op, ...}`), as clients send it over the worker's control service.
    pub fn from_json(value: &Value) -> Result<Op, String> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let need = |key: &str| text(key).ok_or_else(|| format!("{key} must be a string"));
        let images = || value["images"].as_array().cloned().unwrap_or_default();
        let kind = |key: &str| text(key).as_deref().and_then(QueueKind::parse);
        let op = value["op"].as_str().ok_or("op must be a string")?;
        Ok(match op {
            "prompt" => Op::Prompt {
                message: need("message")?,
                images: images(),
                queue: kind("queue"),
            },
            "promptAndWait" => Op::PromptAndWait {
                message: need("message")?,
                images: images(),
            },
            "enqueue" => Op::Enqueue {
                kind: kind("kind").ok_or("kind must be steer, followUp or nextRun")?,
                message: need("message")?,
                images: images(),
                entry_id: text("entryId"),
            },
            "cancelQueued" => Op::CancelQueued {
                entry_id: need("entryId")?,
            },
            "clearQueue" => Op::ClearQueue,
            "abort" => Op::Abort,
            "state" => Op::State,
            "availableModels" => Op::AvailableModels,
            "setModel" => Op::SetModel {
                provider: need("provider")?,
                model_id: need("modelId")?,
            },
            "cycleModel" => Op::CycleModel,
            "thinkingLevels" => Op::ThinkingLevels,
            "setThinking" => Op::SetThinking { level: need("level")? },
            "cycleThinking" => Op::CycleThinking,
            "setQueueMode" => Op::SetQueueMode {
                kind: kind("kind")
                    .filter(|kind| *kind != QueueKind::NextRun)
                    .ok_or("kind must be steer or followUp")?,
                mode: need("mode")?,
            },
            "compact" => Op::Compact {
                instructions: text("instructions"),
            },
            "setAutoCompaction" => Op::SetAutoCompaction(value["enabled"] == true),
            "setAutoRetry" => Op::SetAutoRetry(value["enabled"] == true),
            "setTerminalTheme" => Op::SetTerminalTheme { theme: need("theme")? },
            "abortRetry" => Op::AbortRetry,
            "bash" => Op::Bash {
                command: need("command")?,
                exclude: value["exclude"] == true,
                id: value["id"].as_str().map(str::to_string),
            },
            "abortBash" => Op::AbortBash,
            "checkpoints" => Op::Checkpoints,
            "restoreFiles" => Op::RestoreFiles {
                entry_id: text("entryId"),
                checkpoint: text("checkpoint"),
            },
            "bugSummary" => Op::BugSummary {
                hint: value["hint"].as_str().map(str::to_string),
            },
            "abortBugSummary" => Op::AbortBugSummary,
            "shareContext" => Op::ShareContext,
            "appendCustom" => Op::AppendCustom {
                custom_type: need("customType")?,
                data: value["data"].clone(),
            },
            "stats" => Op::Stats,
            "forkMessages" => Op::ForkMessages,
            "entries" => Op::Entries { since: text("since") },
            "tree" => Op::Tree,
            "lastAssistantText" => Op::LastAssistantText,
            "setName" => Op::SetName { name: need("name")? },
            "messages" => Op::Messages,
            "forkSource" => Op::ForkSource {
                entry_id: need("entryId")?,
            },
            "cloneSource" => Op::CloneSource,
            "forked" => Op::Forked {
                target: need("target")?,
            },
            "describe" => Op::Describe,
            "laneSnapshot" => Op::LaneSnapshot,
            "listSessions" => Op::ListSessions,
            "recover" => Op::Recover,
            "agents" => Op::Agents,
            "setLabel" => Op::SetLabel {
                entry_id: need("entryId")?,
                label: text("label"),
            },
            "navigate" => Op::Navigate {
                target_id: text("targetId"),
                summarize: value["summarize"] == true,
                instructions: text("customInstructions"),
                label: text("label"),
            },
            "killAgent" => Op::KillAgent { id: need("id")? },
            "steerAgent" => Op::SteerAgent {
                id: need("id")?,
                message: need("message")?,
            },
            "reviveAgent" => Op::ReviveAgent { id: need("id")? },
            "agentTranscript" => Op::AgentTranscript { id: need("id")? },
            "observer" => Op::Observer {
                args: text("args").unwrap_or_default(),
            },
            "goal" => Op::Goal {
                args: text("args").unwrap_or_default(),
            },
            "startConversation" => Op::StartConversation {
                message: need("message")?,
            },
            "conversationInput" => Op::ConversationInput {
                id: value["id"].as_u64().ok_or("id must be a conversation id")?,
                message: need("message")?,
            },
            "conversations" => Op::Conversations {
                id: value["id"].as_u64(),
            },
            "asyncPending" => Op::AsyncPending,
            "commands" => Op::Commands,
            "inspect" => Op::Inspect {
                request: need("request")?,
                payload: value.get("payload").cloned().unwrap_or(Value::Null),
            },
            "scheduleFire" => Op::ScheduleFire {
                id: need("id")?,
                due: value["due"].as_i64().ok_or("due must be a time in ms")?,
            },
            "scheduleCancel" => Op::ScheduleCancel { id: need("id")? },
            "persona" => Op::Persona {
                args: text("args").unwrap_or_default(),
            },
            "keepGoing" => Op::KeepGoing {
                args: text("args").unwrap_or_default(),
            },
            "continueFrom" => Op::ContinueFrom {
                session: need("session")?,
            },
            other => return Err(format!("Unknown session operation: {other}")),
        })
    }

    /// The JSON request for this operation ([`Op::from_json`] reads it back).
    pub fn to_json(&self) -> Value {
        match self {
            Op::Prompt { message, images, queue } => {
                json!({"op": "prompt", "message": message, "images": images, "queue": queue.map(QueueKind::name)})
            }
            Op::PromptAndWait { message, images } => {
                json!({"op": "promptAndWait", "message": message, "images": images})
            }
            Op::Enqueue {
                kind,
                message,
                images,
                entry_id,
            } => {
                json!({"op": "enqueue", "kind": kind.name(), "message": message, "images": images, "entryId": entry_id})
            }
            Op::CancelQueued { entry_id } => json!({"op": "cancelQueued", "entryId": entry_id}),
            Op::ClearQueue => json!({"op": "clearQueue"}),
            Op::Abort => json!({"op": "abort"}),
            Op::State => json!({"op": "state"}),
            Op::AvailableModels => json!({"op": "availableModels"}),
            Op::SetModel { provider, model_id } => json!({"op": "setModel", "provider": provider, "modelId": model_id}),
            Op::CycleModel => json!({"op": "cycleModel"}),
            Op::ThinkingLevels => json!({"op": "thinkingLevels"}),
            Op::SetThinking { level } => json!({"op": "setThinking", "level": level}),
            Op::CycleThinking => json!({"op": "cycleThinking"}),
            Op::SetQueueMode { kind, mode } => json!({"op": "setQueueMode", "kind": kind.name(), "mode": mode}),
            Op::Compact { instructions } => json!({"op": "compact", "instructions": instructions}),
            Op::SetAutoCompaction(enabled) => json!({"op": "setAutoCompaction", "enabled": enabled}),
            Op::SetAutoRetry(enabled) => json!({"op": "setAutoRetry", "enabled": enabled}),
            Op::SetTerminalTheme { theme } => json!({"op": "setTerminalTheme", "theme": theme}),
            Op::AbortRetry => json!({"op": "abortRetry"}),
            Op::Bash { command, exclude, id } => {
                let mut op = json!({"op": "bash", "command": command, "exclude": exclude});
                if let Some(id) = id {
                    op["id"] = json!(id);
                }
                op
            }
            Op::AbortBash => json!({"op": "abortBash"}),
            Op::BugSummary { hint } => json!({"op": "bugSummary", "hint": hint}),
            Op::AbortBugSummary => json!({"op": "abortBugSummary"}),
            Op::ShareContext => json!({"op": "shareContext"}),
            Op::AppendCustom { custom_type, data } => {
                json!({"op": "appendCustom", "customType": custom_type, "data": data})
            }
            Op::Stats => json!({"op": "stats"}),
            Op::ForkMessages => json!({"op": "forkMessages"}),
            Op::Entries { since } => json!({"op": "entries", "since": since}),
            Op::Tree => json!({"op": "tree"}),
            Op::LastAssistantText => json!({"op": "lastAssistantText"}),
            Op::SetName { name } => json!({"op": "setName", "name": name}),
            Op::Messages => json!({"op": "messages"}),
            Op::ForkSource { entry_id } => json!({"op": "forkSource", "entryId": entry_id}),
            Op::CloneSource => json!({"op": "cloneSource"}),
            Op::Forked { target } => json!({"op": "forked", "target": target}),
            Op::Describe => json!({"op": "describe"}),
            Op::LaneSnapshot => json!({"op": "laneSnapshot"}),
            Op::ListSessions => json!({"op": "listSessions"}),
            Op::Recover => json!({"op": "recover"}),
            Op::Agents => json!({"op": "agents"}),
            Op::SetLabel { entry_id, label } => json!({"op": "setLabel", "entryId": entry_id, "label": label}),
            Op::Navigate {
                target_id,
                summarize,
                instructions,
                label,
            } => {
                json!({"op": "navigate", "targetId": target_id, "summarize": summarize, "customInstructions": instructions, "label": label})
            }
            Op::KillAgent { id } => json!({"op": "killAgent", "id": id}),
            Op::SteerAgent { id, message } => json!({"op": "steerAgent", "id": id, "message": message}),
            Op::ReviveAgent { id } => json!({"op": "reviveAgent", "id": id}),
            Op::AgentTranscript { id } => json!({"op": "agentTranscript", "id": id}),
            Op::Goal { args } => json!({"op": "goal", "args": args}),
            Op::Observer { args } => json!({"op": "observer", "args": args}),
            Op::StartConversation { message } => json!({"op": "startConversation", "message": message}),
            Op::ConversationInput { id, message } => json!({"op": "conversationInput", "id": id, "message": message}),
            Op::Conversations { id } => json!({"op": "conversations", "id": id}),
            Op::AsyncPending => json!({"op": "asyncPending"}),
            Op::Inspect { request, payload } => json!({"op": "inspect", "request": request, "payload": payload}),
            Op::Commands => json!({"op": "commands"}),
            Op::Checkpoints => json!({"op": "checkpoints"}),
            Op::RestoreFiles { entry_id, checkpoint } => {
                json!({"op": "restoreFiles", "entryId": entry_id, "checkpoint": checkpoint})
            }
            Op::ScheduleFire { id, due } => json!({"op": "scheduleFire", "id": id, "due": due}),
            Op::ScheduleCancel { id } => json!({"op": "scheduleCancel", "id": id}),
            Op::Persona { args } => json!({"op": "persona", "args": args}),
            Op::KeepGoing { args } => json!({"op": "keepGoing", "args": args}),
            Op::ContinueFrom { session } => json!({"op": "continueFrom", "session": session}),
        }
    }
}
