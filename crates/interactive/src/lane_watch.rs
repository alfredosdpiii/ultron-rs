//! What the TUI shows of its session's lane-watch events ([`protocol::watch::WatchEvent`]): each event as the
//! [`LaneEvent`]s the app handles.

use protocol::watch::{Ended, WatchEvent};
use serde_json::Value;

use crate::lane::{LaneEvent, Queued, SessionListing};

/// The TUI events of a lane-watch event (none for the ones it does not show). A `message_update` shows only when it
/// carries its message: the client folds a frame-only update into the started message first.
pub fn lane_events(event: &WatchEvent) -> Vec<LaneEvent> {
    let assistant = |message: &Value| message["role"] == "assistant";
    vec![match event {
        WatchEvent::RunStart { run_id, .. } => LaneEvent::OperationStarted(run_id.clone()),
        WatchEvent::MessageStart { message, .. } if assistant(message) => LaneEvent::Streaming(message.clone()),
        WatchEvent::MessageUpdate {
            message: Some(message), ..
        } if assistant(message) => LaneEvent::Streaming(message.clone()),
        WatchEvent::MessageEnd { message, .. } => LaneEvent::Message(message.clone()),
        WatchEvent::ToolStart {
            tool_call_id,
            tool_name,
            args,
            ..
        } => LaneEvent::ToolStarted {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
        },
        WatchEvent::ToolEnd { tool_call_id, .. } => LaneEvent::ToolEnded {
            id: tool_call_id.clone(),
        },
        WatchEvent::QueueUpdate { queues, .. } => LaneEvent::Queues(
            queues
                .iter()
                .map(|item| {
                    let message = item.item == "message";
                    let user = message && item.message.as_ref().is_some_and(|m| m["role"] == "user");
                    Queued {
                        entry_id: item.entry_id.clone(),
                        kind: item.kind,
                        // client-tui-chat.ts `#syncQueues`: a message's user text (none for another role), else
                        // `<customType>`.
                        text: match &item.message {
                            Some(m) if user => protocol::pi_message::text(m),
                            _ if message => String::new(),
                            _ => format!("<{}>", item.custom_type.as_deref().unwrap_or("undefined")),
                        },
                        user,
                    }
                })
                .collect(),
        ),
        WatchEvent::SessionName { name, .. } => LaneEvent::Name(name.clone()),
        WatchEvent::CompactionStart { reason, .. } => LaneEvent::CompactionStarted {
            reason: reason_or_manual(reason),
        },
        // A manual compaction's failure answers its operation; an automatic one that left no entry ends here.
        WatchEvent::CompactionEnd {
            reason, status, error, ..
        } if reason != "manual" && *status != Ended::Completed => LaneEvent::CompactionEnded {
            status: status.name().into(),
            error: error.as_ref().map(|e| e.message.clone()),
        },
        WatchEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
            ..
        } => LaneEvent::SummarizationRetryScheduled {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_ms: *delay_ms,
            error: error_message.clone(),
        },
        WatchEvent::SummarizationRetryAttemptStart { source, reason, .. } => LaneEvent::SummarizationRetryStarted {
            branch_summary: source == "branchSummary",
            reason: reason_or_manual(reason.as_deref().unwrap_or_default()),
        },
        WatchEvent::SummarizationRetryFinished { .. } => LaneEvent::SummarizationRetryFinished,
        WatchEvent::RetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => LaneEvent::AutoRetryStart {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_ms: *delay_ms,
        },
        WatchEvent::RetryEnd {
            attempt,
            success,
            final_error,
            ..
        } => LaneEvent::AutoRetryEnd {
            attempt: *attempt,
            failed: (!success).then(|| final_error.clone().unwrap_or_else(|| "Unknown error".into())),
        },
        WatchEvent::EntryAdded { entry, .. } if entry["type"] == "compaction" => {
            LaneEvent::Compacted(Ok(entry.clone()))
        }
        WatchEvent::ConfigUpdate { property, value, .. } if property == "thinkingLevel" => match value.as_str() {
            Some(level) => LaneEvent::Thinking(level.to_string()),
            None => return Vec::new(),
        },
        // An aborted run is not a failure the TUI reports.
        WatchEvent::RunEnd { status, error, .. } => LaneEvent::OperationEnded {
            error: (*status == Ended::Failed)
                .then(|| error.as_ref().map(|e| e.message.clone()))
                .flatten(),
        },
        _ => return Vec::new(),
    }]
}

fn reason_or_manual(reason: &str) -> String {
    if reason.is_empty() { "manual" } else { reason }.to_string()
}

impl SessionListing {
    /// The session listing `listSessions` answers (`[{path, id, name?, modified, messageCount, firstMessage,
    /// allMessagesText, cwd}]`).
    pub fn listed(value: &Value) -> Vec<SessionListing> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| SessionListing {
                path: s["path"].as_str().unwrap_or_default().to_string(),
                id: s["id"].as_str().unwrap_or_default().to_string(),
                name: s["name"].as_str().map(str::to_string),
                modified: s["modified"].as_i64().unwrap_or(0),
                message_count: s["messageCount"].as_u64().unwrap_or(0) as usize,
                first_message: s["firstMessage"].as_str().unwrap_or_default().to_string(),
                all_messages_text: s["allMessagesText"].as_str().unwrap_or_default().to_string(),
                cwd: s["cwd"].as_str().unwrap_or_default().to_string(),
            })
            .collect()
    }
}
