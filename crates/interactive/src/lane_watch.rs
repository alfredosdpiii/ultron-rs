//! What the TUI shows of its session's lane-watch events ([`protocol::watch::WatchEvent`]): each event as the
//! [`LaneEvent`]s the app handles.

use protocol::watch::{Ended, WatchEvent};
use serde_json::Value;

use crate::lane::{LaneEvent, Queued, SessionListing};

fn is_assistant(message: &Value) -> bool {
    message["role"] == "assistant"
}

fn map_queued_item(item: &protocol::watch::QueueItem) -> Queued {
    let message = item.item == "message";
    let user = message && item.message.as_ref().is_some_and(|m| m["role"] == "user");
    Queued {
        entry_id: item.entry_id.clone(),
        kind: item.kind,
        text: match &item.message {
            Some(m) if user => protocol::pi_message::text(m),
            _ if message => String::new(),
            _ => format!("<{}>", item.custom_type.as_deref().unwrap_or("undefined")),
        },
        user,
    }
}

fn run_or_tool_event(event: &WatchEvent) -> Option<LaneEvent> {
    match event {
        WatchEvent::RunStart { run_id, .. } => Some(LaneEvent::OperationStarted(run_id.clone())),
        WatchEvent::RunEnd { status, error, .. } => Some(LaneEvent::OperationEnded {
            error: (*status == Ended::Failed)
                .then(|| error.as_ref().map(|e| e.message.clone()))
                .flatten(),
        }),
        WatchEvent::ToolStart {
            tool_call_id,
            tool_name,
            args,
            ..
        } => Some(LaneEvent::ToolStarted {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
        }),
        WatchEvent::ToolEnd { tool_call_id, .. } => Some(LaneEvent::ToolEnded {
            id: tool_call_id.clone(),
        }),
        _ => None,
    }
}

fn message_event(event: &WatchEvent) -> Option<LaneEvent> {
    match event {
        WatchEvent::MessageStart { message, .. } if is_assistant(message) => {
            Some(LaneEvent::Streaming(message.clone()))
        }
        WatchEvent::MessageUpdate {
            message: Some(message), ..
        } if is_assistant(message) => Some(LaneEvent::Streaming(message.clone())),
        WatchEvent::MessageEnd { message, .. } => Some(LaneEvent::Message(message.clone())),
        _ => None,
    }
}

fn session_or_queue_event(event: &WatchEvent) -> Option<LaneEvent> {
    match event {
        WatchEvent::QueueUpdate { queues, .. } => Some(LaneEvent::Queues(queues.iter().map(map_queued_item).collect())),
        WatchEvent::SessionName { name, .. } => Some(LaneEvent::Name(name.clone())),
        WatchEvent::EntryAdded { entry, .. } if entry["type"] == "compaction" => {
            Some(LaneEvent::Compacted(Ok(entry.clone())))
        }
        WatchEvent::ConfigUpdate { property, value, .. } if property == "thinkingLevel" => {
            value.as_str().map(|level| LaneEvent::Thinking(level.to_string()))
        }
        _ => None,
    }
}

fn compaction_event(event: &WatchEvent) -> Option<LaneEvent> {
    match event {
        WatchEvent::CompactionStart { reason, .. } => Some(LaneEvent::CompactionStarted {
            reason: reason_or_manual(reason),
        }),
        WatchEvent::CompactionEnd {
            reason, status, error, ..
        } if reason != "manual" && *status != Ended::Completed => Some(LaneEvent::CompactionEnded {
            status: status.name().into(),
            error: error.as_ref().map(|e| e.message.clone()),
        }),
        _ => None,
    }
}

fn retry_event(event: &WatchEvent) -> Option<LaneEvent> {
    match event {
        WatchEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
            ..
        } => Some(LaneEvent::SummarizationRetryScheduled {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_ms: *delay_ms,
            error: error_message.clone(),
        }),
        WatchEvent::SummarizationRetryAttemptStart { source, reason, .. } => {
            Some(LaneEvent::SummarizationRetryStarted {
                branch_summary: source == "branchSummary",
                reason: reason_or_manual(reason.as_deref().unwrap_or_default()),
            })
        }
        WatchEvent::SummarizationRetryFinished { .. } => Some(LaneEvent::SummarizationRetryFinished),
        WatchEvent::RetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => Some(LaneEvent::AutoRetryStart {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_ms: *delay_ms,
        }),
        WatchEvent::RetryEnd {
            attempt,
            success,
            final_error,
            ..
        } => Some(LaneEvent::AutoRetryEnd {
            attempt: *attempt,
            failed: (!success).then(|| final_error.clone().unwrap_or_else(|| "Unknown error".into())),
        }),
        _ => None,
    }
}

/// The TUI events of a lane-watch event (none for the ones it does not show). A `message_update` shows only when it
/// carries its message: the client folds a frame-only update into the started message first.
pub fn lane_events(event: &WatchEvent) -> Vec<LaneEvent> {
    run_or_tool_event(event)
        .or_else(|| message_event(event))
        .or_else(|| session_or_queue_event(event))
        .or_else(|| compaction_event(event))
        .or_else(|| retry_event(event))
        .into_iter()
        .collect()
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
