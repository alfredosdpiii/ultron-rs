//! Lane-watch events: what a session's lane publishes to its watchers (the TUI, the RPC stream, herdr, the daemon's
//! lane replica), one JSON object per event tagged by `type`. The session worker writes them from this enum and every
//! watcher reads them back through [`WatchEvent::decode`], so a field is named in one place.
//!
//! Field order is the order the worker writes. A field a watcher may not get from another producer (an older worker,
//! a remote server) reads as its default.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A queue a message can join while a run streams (`steer`, `followUp`, `nextRun`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueKind {
    Steer,
    /// Held until the next run starts (the agent controller's `nextRun`): the run in progress never takes it, and the
    /// next run's admission places it before its prompt.
    NextRun,
    /// Also what a kind this build does not know reads as.
    #[default]
    #[serde(other)]
    FollowUp,
}

impl QueueKind {
    pub fn name(self) -> &'static str {
        match self {
            QueueKind::Steer => "steer",
            QueueKind::FollowUp => "followUp",
            QueueKind::NextRun => "nextRun",
        }
    }

    pub fn parse(name: &str) -> Option<QueueKind> {
        match name {
            "steer" => Some(QueueKind::Steer),
            "followUp" => Some(QueueKind::FollowUp),
            "nextRun" => Some(QueueKind::NextRun),
            _ => None,
        }
    }
}

/// How a run, a compaction or a navigation ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ended {
    Completed,
    Failed,
    Aborted,
    /// An automatic compaction a hook declined (it trimmed the context instead).
    Declined,
    #[default]
    #[serde(other)]
    Unknown,
}

impl Ended {
    pub fn name(self) -> &'static str {
        match self {
            Ended::Completed => "completed",
            Ended::Failed => "failed",
            Ended::Aborted => "aborted",
            Ended::Declined => "declined",
            Ended::Unknown => "unknown",
        }
    }
}

/// Why something failed (`{code?, message}`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// One item of a [`WatchEvent::QueueUpdate`]: a queued message, or a host item shown as `<customType>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    /// `message`, or a host item's own type.
    #[serde(rename = "type", default)]
    pub item: String,
    #[serde(default)]
    pub kind: QueueKind,
    #[serde(default)]
    pub entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
}

/// One lane-watch event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum WatchEvent {
    RunStart {
        #[serde(default)]
        run_id: String,
        #[serde(default)]
        started_at: i64,
        #[serde(default)]
        lane: String,
    },
    RunEnd {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        run_id: String,
        #[serde(default)]
        status: Ended,
        /// A failed run's error (`code: run_failed`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Failure>,
        #[serde(default)]
        from_tip_id: Option<String>,
        #[serde(default)]
        tip_id: Option<String>,
        #[serde(default)]
        ended_at: i64,
    },
    /// A message started: the run's assistant answer or tool result, or a message outside a run (no `runId`).
    MessageStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        message: Value,
    },
    /// The streaming assistant message changed: the provider frame and the usage so far, not the message (a watcher
    /// folds the frame into the started message). An older worker's update carries the whole `message` instead.
    MessageUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        frame: Value,
        #[serde(default)]
        usage: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Value>,
    },
    MessageEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        message: Value,
    },
    ToolStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        args: Value,
    },
    ToolUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        args: Value,
        #[serde(default)]
        partial_result: Value,
    },
    ToolEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        lane: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        is_error: bool,
    },
    /// The queued items now.
    QueueUpdate {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        queues: Vec<QueueItem>,
    },
    /// The session's name now (`None`: cleared); Rust-only, served as `ultron.session-ops` state.
    SessionName {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// AgentSession `setSessionName` (Pi's RPC event).
    SessionInfoChanged {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A journal entry joined the session (a message, a compaction, a branch summary, ...).
    EntryAdded {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        entry: Value,
    },
    /// A usage row, with the lane's totals after it.
    Usage {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        row: Value,
        #[serde(default)]
        totals: Value,
    },
    /// A generation's request failed and is retried after `delay_ms`.
    RetryScheduled {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        step: String,
        #[serde(default)]
        attempt: u64,
        #[serde(default)]
        max_attempts: u64,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default)]
        not_before: i64,
        #[serde(default)]
        error_message: String,
    },
    /// The retry wait ended: the step's next attempt starts.
    RetryStart {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        step: String,
        #[serde(default)]
        attempt: u64,
    },
    /// A retried request's retries ended; `final_error` when the last attempt failed too.
    RetryEnd {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        step: String,
        #[serde(default)]
        attempt: u64,
        #[serde(default)]
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// A chunk of a user `!` command's output (AgentSession `bash_execution_update`), under the command's id.
    BashExecutionUpdate {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        delta: String,
    },
    /// A lane setting changed (`thinkingLevel`).
    ConfigUpdate {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        property: String,
        #[serde(default)]
        value: Value,
    },
    /// A compaction's or branch summary's request failed and is retried (AgentSession `onRetryScheduled`): the
    /// retry's number, the retries allowed, the delay and the provider's error.
    SummarizationRetryScheduled {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        attempt: u64,
        #[serde(default)]
        max_attempts: u64,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default)]
        error_message: String,
    },
    /// The summary's retry starts (`onRetryAttemptStart`): of a branch summary (`source: branchSummary`), or of a
    /// compaction (`source: compaction`) for `reason`.
    SummarizationRetryAttemptStart {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The summary's retries ended, however they did (`onRetryFinished`).
    SummarizationRetryFinished {
        #[serde(default)]
        lane: String,
    },
    /// A compaction started: `manual`, or `threshold` / `overflow` for one the session runs on its own in a run.
    CompactionStart {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        reason: String,
    },
    CompactionEnd {
        #[serde(default)]
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        status: Ended,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Failure>,
    },
    /// A tree navigation started (`target_id`: the entry, `None`: the root).
    NavigationStart {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        run_id: String,
        #[serde(default)]
        target_id: Option<String>,
        #[serde(default)]
        started_at: i64,
    },
    NavigationEnd {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        run_id: String,
        #[serde(default)]
        status: Ended,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Failure>,
        #[serde(default)]
        from_tip_id: Option<String>,
        #[serde(default)]
        tip_id: Option<String>,
        #[serde(default)]
        ended_at: i64,
    },
    /// Whether detached work of the session is active (the worker does not retire while it is).
    Activity {
        #[serde(default)]
        lane: String,
        #[serde(default)]
        active: bool,
    },
}

impl WatchEvent {
    /// The event a lane-watch JSON object is; `None` for one of a type this build does not know (or a malformed one).
    pub fn decode(event: &Value) -> Option<WatchEvent> {
        serde_json::from_value(event.clone()).ok()
    }

    /// The event as it goes on the wire.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}
