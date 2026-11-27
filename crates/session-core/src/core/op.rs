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

type OpParser = fn(&str, &Value) -> Option<Result<Op, String>>;

impl Op {
    /// The operation a JSON request names (`{op, ...}`), as clients send it over the worker's control service.
    pub fn from_json(value: &Value) -> Result<Op, String> {
        let op = value["op"].as_str().ok_or("op must be a string")?;
        const PARSERS: &[OpParser] = &[
            parse_prompt_op,
            parse_queue_op,
            parse_model_op,
            parse_config_op,
            parse_execution_op,
            parse_bug_and_custom_op,
            parse_view_op,
            parse_session_nav_op,
            parse_tree_nav_op,
            parse_agent_op,
            parse_conversation_op,
            parse_control_op,
            parse_schedule_and_persona_op,
        ];
        for parse in PARSERS {
            if let Some(result) = parse(op, value) {
                return result;
            }
        }
        Err(format!("Unknown session operation: {op}"))
    }

    /// The JSON request for this operation ([`Op::from_json`] reads it back).
    pub fn to_json(&self) -> Value {
        const SERIALIZERS: &[fn(&Op) -> Option<Value>] = &[
            serialize_prompt_queue_op,
            serialize_model_thinking_op,
            serialize_config_execution_op,
            serialize_bug_and_view_op,
            serialize_session_view_op,
            serialize_navigation_op,
            serialize_agent_op,
            serialize_conversation_op,
            serialize_control_op,
            serialize_schedule_and_persona_op,
        ];
        for serialize in SERIALIZERS {
            if let Some(json) = serialize(self) {
                return json;
            }
        }
        unreachable!("every Op variant has a serializer")
    }
}

fn json_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn json_need(value: &Value, key: &str) -> Result<String, String> {
    json_text(value, key).ok_or_else(|| format!("{key} must be a string"))
}

fn json_images(value: &Value) -> Vec<Value> {
    value["images"].as_array().cloned().unwrap_or_default()
}

fn json_queue_kind(value: &Value, key: &str) -> Option<QueueKind> {
    json_text(value, key).as_deref().and_then(QueueKind::parse)
}

fn parse_prompt_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "prompt" => json_need(value, "message").map(|message| Op::Prompt {
            message,
            images: json_images(value),
            queue: json_queue_kind(value, "queue"),
        }),
        "promptAndWait" => json_need(value, "message").map(|message| Op::PromptAndWait {
            message,
            images: json_images(value),
        }),
        "enqueue" => parse_enqueue(value),
        "cancelQueued" => json_need(value, "entryId").map(|entry_id| Op::CancelQueued { entry_id }),
        _ => return None,
    })
}

fn parse_enqueue(value: &Value) -> Result<Op, String> {
    Ok(Op::Enqueue {
        kind: json_queue_kind(value, "kind").ok_or("kind must be steer, followUp or nextRun")?,
        message: json_need(value, "message")?,
        images: json_images(value),
        entry_id: json_text(value, "entryId"),
    })
}

fn parse_queue_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "clearQueue" => Ok(Op::ClearQueue),
        "abort" => Ok(Op::Abort),
        "state" => Ok(Op::State),
        "setQueueMode" => parse_set_queue_mode(value),
        _ => return None,
    })
}

fn parse_set_queue_mode(value: &Value) -> Result<Op, String> {
    Ok(Op::SetQueueMode {
        kind: json_queue_kind(value, "kind")
            .filter(|kind| *kind != QueueKind::NextRun)
            .ok_or("kind must be steer or followUp")?,
        mode: json_need(value, "mode")?,
    })
}

fn parse_model_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "availableModels" => Ok(Op::AvailableModels),
        "setModel" => parse_set_model(value),
        "cycleModel" => Ok(Op::CycleModel),
        "thinkingLevels" => Ok(Op::ThinkingLevels),
        "setThinking" => json_need(value, "level").map(|level| Op::SetThinking { level }),
        "cycleThinking" => Ok(Op::CycleThinking),
        _ => return None,
    })
}

fn parse_set_model(value: &Value) -> Result<Op, String> {
    Ok(Op::SetModel {
        provider: json_need(value, "provider")?,
        model_id: json_need(value, "modelId")?,
    })
}

fn parse_config_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "compact" => Ok(Op::Compact {
            instructions: json_text(value, "instructions"),
        }),
        "setAutoCompaction" => Ok(Op::SetAutoCompaction(value["enabled"] == true)),
        "setAutoRetry" => Ok(Op::SetAutoRetry(value["enabled"] == true)),
        "setTerminalTheme" => json_need(value, "theme").map(|theme| Op::SetTerminalTheme { theme }),
        "abortRetry" => Ok(Op::AbortRetry),
        _ => return None,
    })
}

fn parse_execution_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "bash" => json_need(value, "command").map(|command| Op::Bash {
            command,
            exclude: value["exclude"] == true,
            id: value["id"].as_str().map(str::to_string),
        }),
        "abortBash" => Ok(Op::AbortBash),
        "checkpoints" => Ok(Op::Checkpoints),
        "restoreFiles" => Ok(Op::RestoreFiles {
            entry_id: json_text(value, "entryId"),
            checkpoint: json_text(value, "checkpoint"),
        }),
        _ => return None,
    })
}

fn parse_bug_and_custom_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "bugSummary" => Ok(Op::BugSummary {
            hint: value["hint"].as_str().map(str::to_string),
        }),
        "abortBugSummary" => Ok(Op::AbortBugSummary),
        "shareContext" => Ok(Op::ShareContext),
        "appendCustom" => json_need(value, "customType").map(|custom_type| Op::AppendCustom {
            custom_type,
            data: value["data"].clone(),
        }),
        _ => return None,
    })
}

fn parse_view_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "stats" => Ok(Op::Stats),
        "forkMessages" => Ok(Op::ForkMessages),
        "entries" => Ok(Op::Entries {
            since: json_text(value, "since"),
        }),
        "tree" => Ok(Op::Tree),
        "lastAssistantText" => Ok(Op::LastAssistantText),
        "setName" => json_need(value, "name").map(|name| Op::SetName { name }),
        "messages" => Ok(Op::Messages),
        _ => return None,
    })
}

fn parse_session_nav_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "forkSource" => json_need(value, "entryId").map(|entry_id| Op::ForkSource { entry_id }),
        "cloneSource" => Ok(Op::CloneSource),
        "forked" => json_need(value, "target").map(|target| Op::Forked { target }),
        "describe" => Ok(Op::Describe),
        "laneSnapshot" => Ok(Op::LaneSnapshot),
        "listSessions" => Ok(Op::ListSessions),
        "recover" => Ok(Op::Recover),
        _ => return None,
    })
}

fn parse_tree_nav_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "setLabel" => json_need(value, "entryId").map(|entry_id| Op::SetLabel {
            entry_id,
            label: json_text(value, "label"),
        }),
        "navigate" => Ok(Op::Navigate {
            target_id: json_text(value, "targetId"),
            summarize: value["summarize"] == true,
            instructions: json_text(value, "customInstructions"),
            label: json_text(value, "label"),
        }),
        _ => return None,
    })
}

fn parse_agent_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "agents" => Ok(Op::Agents),
        "killAgent" => json_need(value, "id").map(|id| Op::KillAgent { id }),
        "steerAgent" => parse_steer_agent(value),
        "reviveAgent" => json_need(value, "id").map(|id| Op::ReviveAgent { id }),
        "agentTranscript" => json_need(value, "id").map(|id| Op::AgentTranscript { id }),
        _ => return None,
    })
}

fn parse_steer_agent(value: &Value) -> Result<Op, String> {
    Ok(Op::SteerAgent {
        id: json_need(value, "id")?,
        message: json_need(value, "message")?,
    })
}

fn parse_conversation_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "startConversation" => json_need(value, "message").map(|message| Op::StartConversation { message }),
        "conversationInput" => parse_conversation_input(value),
        "conversations" => Ok(Op::Conversations {
            id: value["id"].as_u64(),
        }),
        _ => return None,
    })
}

fn parse_conversation_input(value: &Value) -> Result<Op, String> {
    Ok(Op::ConversationInput {
        id: value["id"].as_u64().ok_or("id must be a conversation id")?,
        message: json_need(value, "message")?,
    })
}

fn parse_control_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "observer" => Ok(Op::Observer {
            args: json_text(value, "args").unwrap_or_default(),
        }),
        "goal" => Ok(Op::Goal {
            args: json_text(value, "args").unwrap_or_default(),
        }),
        "asyncPending" => Ok(Op::AsyncPending),
        "commands" => Ok(Op::Commands),
        "inspect" => json_need(value, "request").map(|request| Op::Inspect {
            request,
            payload: value.get("payload").cloned().unwrap_or(Value::Null),
        }),
        _ => return None,
    })
}

fn parse_schedule_and_persona_op(op: &str, value: &Value) -> Option<Result<Op, String>> {
    Some(match op {
        "scheduleFire" => parse_schedule_fire(value),
        "scheduleCancel" => json_need(value, "id").map(|id| Op::ScheduleCancel { id }),
        "persona" => Ok(Op::Persona {
            args: json_text(value, "args").unwrap_or_default(),
        }),
        "keepGoing" => Ok(Op::KeepGoing {
            args: json_text(value, "args").unwrap_or_default(),
        }),
        "continueFrom" => json_need(value, "session").map(|session| Op::ContinueFrom { session }),
        _ => return None,
    })
}

fn parse_schedule_fire(value: &Value) -> Result<Op, String> {
    Ok(Op::ScheduleFire {
        id: json_need(value, "id")?,
        due: value["due"].as_i64().ok_or("due must be a time in ms")?,
    })
}

fn serialize_prompt_queue_op(op: &Op) -> Option<Value> {
    Some(match op {
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
        _ => return None,
    })
}

fn serialize_model_thinking_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::AvailableModels => json!({"op": "availableModels"}),
        Op::SetModel { provider, model_id } => json!({"op": "setModel", "provider": provider, "modelId": model_id}),
        Op::CycleModel => json!({"op": "cycleModel"}),
        Op::ThinkingLevels => json!({"op": "thinkingLevels"}),
        Op::SetThinking { level } => json!({"op": "setThinking", "level": level}),
        Op::CycleThinking => json!({"op": "cycleThinking"}),
        Op::SetQueueMode { kind, mode } => json!({"op": "setQueueMode", "kind": kind.name(), "mode": mode}),
        _ => return None,
    })
}

fn serialize_config_execution_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::Compact { instructions } => json!({"op": "compact", "instructions": instructions}),
        Op::SetAutoCompaction(enabled) => json!({"op": "setAutoCompaction", "enabled": enabled}),
        Op::SetAutoRetry(enabled) => json!({"op": "setAutoRetry", "enabled": enabled}),
        Op::SetTerminalTheme { theme } => json!({"op": "setTerminalTheme", "theme": theme}),
        Op::AbortRetry => json!({"op": "abortRetry"}),
        Op::Bash { command, exclude, id } => serialize_bash_op(command, *exclude, id.as_deref()),
        Op::AbortBash => json!({"op": "abortBash"}),
        _ => return None,
    })
}

fn serialize_bash_op(command: &str, exclude: bool, id: Option<&str>) -> Value {
    let mut op = json!({"op": "bash", "command": command, "exclude": exclude});
    if let Some(id) = id {
        op["id"] = json!(id);
    }
    op
}

fn serialize_bug_and_view_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::BugSummary { hint } => json!({"op": "bugSummary", "hint": hint}),
        Op::AbortBugSummary => json!({"op": "abortBugSummary"}),
        Op::ShareContext => json!({"op": "shareContext"}),
        Op::AppendCustom { custom_type, data } => {
            json!({"op": "appendCustom", "customType": custom_type, "data": data})
        }
        Op::Stats => json!({"op": "stats"}),
        Op::ForkMessages => json!({"op": "forkMessages"}),
        Op::Entries { since } => json!({"op": "entries", "since": since}),
        _ => return None,
    })
}

fn serialize_session_view_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::Tree => json!({"op": "tree"}),
        Op::LastAssistantText => json!({"op": "lastAssistantText"}),
        Op::SetName { name } => json!({"op": "setName", "name": name}),
        Op::Messages => json!({"op": "messages"}),
        Op::ForkSource { entry_id } => json!({"op": "forkSource", "entryId": entry_id}),
        Op::CloneSource => json!({"op": "cloneSource"}),
        Op::Forked { target } => json!({"op": "forked", "target": target}),
        _ => return None,
    })
}

fn serialize_navigation_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::Describe => json!({"op": "describe"}),
        Op::LaneSnapshot => json!({"op": "laneSnapshot"}),
        Op::ListSessions => json!({"op": "listSessions"}),
        Op::Recover => json!({"op": "recover"}),
        Op::SetLabel { entry_id, label } => json!({"op": "setLabel", "entryId": entry_id, "label": label}),
        Op::Navigate {
            target_id,
            summarize,
            instructions,
            label,
        } => {
            json!({"op": "navigate", "targetId": target_id, "summarize": summarize, "customInstructions": instructions, "label": label})
        }
        _ => return None,
    })
}

fn serialize_agent_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::Agents => json!({"op": "agents"}),
        Op::KillAgent { id } => json!({"op": "killAgent", "id": id}),
        Op::SteerAgent { id, message } => json!({"op": "steerAgent", "id": id, "message": message}),
        Op::ReviveAgent { id } => json!({"op": "reviveAgent", "id": id}),
        Op::AgentTranscript { id } => json!({"op": "agentTranscript", "id": id}),
        _ => return None,
    })
}

fn serialize_conversation_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::StartConversation { message } => json!({"op": "startConversation", "message": message}),
        Op::ConversationInput { id, message } => json!({"op": "conversationInput", "id": id, "message": message}),
        Op::Conversations { id } => json!({"op": "conversations", "id": id}),
        Op::Checkpoints => json!({"op": "checkpoints"}),
        Op::RestoreFiles { entry_id, checkpoint } => {
            json!({"op": "restoreFiles", "entryId": entry_id, "checkpoint": checkpoint})
        }
        _ => return None,
    })
}

fn serialize_control_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::Goal { args } => json!({"op": "goal", "args": args}),
        Op::Observer { args } => json!({"op": "observer", "args": args}),
        Op::AsyncPending => json!({"op": "asyncPending"}),
        Op::Inspect { request, payload } => json!({"op": "inspect", "request": request, "payload": payload}),
        Op::Commands => json!({"op": "commands"}),
        _ => return None,
    })
}

fn serialize_schedule_and_persona_op(op: &Op) -> Option<Value> {
    Some(match op {
        Op::ScheduleFire { id, due } => json!({"op": "scheduleFire", "id": id, "due": due}),
        Op::ScheduleCancel { id } => json!({"op": "scheduleCancel", "id": id}),
        Op::Persona { args } => json!({"op": "persona", "args": args}),
        Op::KeepGoing { args } => json!({"op": "keepGoing", "args": args}),
        Op::ContinueFrom { session } => json!({"op": "continueFrom", "session": session}),
        _ => return None,
    })
}
