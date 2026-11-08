//! The states an open operation's `ultron.op.state` is `at` (`drive/*.ts`), as one enum: every writer names its
//! state through it, and every reader (recovery, reopen, the may-finish check) matches on it, so a new state is a
//! compile error where it is not handled instead of a string literal that falls to a default.

/// Where an open operation stands (`OP_STATE.at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpAt {
    /// Admitted; its first checkpoint is not committed yet.
    Starting,
    /// Between steps: `continuation.kind` says whether the run needs an answer or may finish.
    Checkpoint,
    /// A generation is prepared; no request is out.
    AssistantReady,
    /// A model request is out; its frames commit as they stream.
    AssistantEffectPending,
    /// An error answer committed and the step is retried after `notBefore`.
    AssistantRetryWait,
    /// An error answer that ends the run committed (Rust only: the TypeScript drive holds it for the terminal
    /// transaction); the terminal transaction follows.
    AssistantFailed,
    /// A tool batch runs (per call `planned | effect_pending | outcome_ready`).
    Tools,
    /// A compaction decides whether to summarize.
    SummaryDeciding,
    /// A summary attempt is prepared.
    SummaryReady,
    /// A summary request is out.
    SummaryEffectPending,
    /// A summary attempt failed and is retried after `notBefore`.
    SummaryRetryWait,
    /// A navigation without a summary is admitted and commits as is.
    NavigationReadyToCommit,
}

impl OpAt {
    pub const ALL: [OpAt; 12] = [
        OpAt::Starting,
        OpAt::Checkpoint,
        OpAt::AssistantReady,
        OpAt::AssistantEffectPending,
        OpAt::AssistantRetryWait,
        OpAt::AssistantFailed,
        OpAt::Tools,
        OpAt::SummaryDeciding,
        OpAt::SummaryReady,
        OpAt::SummaryEffectPending,
        OpAt::SummaryRetryWait,
        OpAt::NavigationReadyToCommit,
    ];

    /// The stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            OpAt::Starting => "starting",
            OpAt::Checkpoint => "checkpoint",
            OpAt::AssistantReady => "assistant.ready",
            OpAt::AssistantEffectPending => "assistant.effect_pending",
            OpAt::AssistantRetryWait => "assistant.retry_wait",
            OpAt::AssistantFailed => "assistant.failed",
            OpAt::Tools => "tools",
            OpAt::SummaryDeciding => "summary.deciding",
            OpAt::SummaryReady => "summary.ready",
            OpAt::SummaryEffectPending => "summary.effect_pending",
            OpAt::SummaryRetryWait => "summary.retry_wait",
            OpAt::NavigationReadyToCommit => "navigation.ready_to_commit",
        }
    }

    /// The state a stored `at` names; `None` for a value no writer of this build produces.
    pub fn parse(at: &str) -> Option<OpAt> {
        OpAt::ALL.into_iter().find(|state| state.as_str() == at)
    }

    /// The state of a stored `ultron.op.state` value (`None`: absent or unknown `at`).
    pub fn of(state: &serde_json::Value) -> Option<OpAt> {
        state["at"].as_str().and_then(OpAt::parse)
    }

    /// One of a compaction's `summary.*` states.
    pub fn is_summary(self) -> bool {
        matches!(
            self,
            OpAt::SummaryDeciding | OpAt::SummaryReady | OpAt::SummaryEffectPending | OpAt::SummaryRetryWait
        )
    }
}

impl From<OpAt> for serde_json::Value {
    fn from(at: OpAt) -> Self {
        at.as_str().into()
    }
}
