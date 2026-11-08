//! The summary attempts of a compaction or a summarized navigation (`drive/structural.ts`), as both a run's
//! compaction ([`super::Run`]) and a structural operation outside a run journal them: `summary.deciding` before any
//! request, then per attempt `summary.ready` (`nextAttempt`), `summary.effect_pending` (`attempt`, `usageIds`)
//! before the request and with it (`request{index, usageId}`), and `summary.retry_wait` (`notBefore`,
//! `errorMessage`) between attempts.

use crate::ns::OP_STATE;
use crate::{Session, Write, lock_clock};
use serde_json::{Map, Value, json};

/// A summary request's `streamOptions`: the generation defaults, never deferred.
fn stream_options() -> Value {
    let mut options = super::stream_options();
    options["deferred"] = false.into();
    options
}

pub const DECIDING: &str = "summary.deciding";
pub const READY: &str = "summary.ready";
pub const EFFECT_PENDING: &str = "summary.effect_pending";
pub const RETRY_WAIT: &str = "summary.retry_wait";

/// The operation an attempt's states belong to: its id, the scope every state starts with (`control`, `settings`,
/// `latestAssistantEntryId`) and its task (`Null`: none).
pub struct Scope {
    pub operation_id: String,
    pub base: Map<String, Value>,
    pub task: Value,
}

impl Scope {
    /// The operation's state `at`: the scope, `at`, the task, then `fields`.
    pub fn state(&self, at: &str, fields: Vec<(&str, Value)>) -> Value {
        let mut state = self.base.clone();
        state.insert("at".into(), at.into());
        if !self.task.is_null() {
            state.insert("task".into(), self.task.clone());
        }
        for (key, value) in fields {
            state.insert(key.into(), value);
        }
        Value::Object(state)
    }

    fn write(&self, at: &str, fields: Vec<(&str, Value)>) -> Write {
        Write::set(OP_STATE, &self.operation_id, self.state(at, fields))
    }
}

/// The summary request of one operation: its context, the attempt in progress (1-based) and the usage id of that
/// attempt's request.
#[derive(Clone, Debug)]
pub struct SummaryAttempt {
    context: Value,
    attempt: u64,
    usage_id: Option<String>,
}

impl SummaryAttempt {
    /// The first attempt of a summary that commits as `result_entry_id`, requested with `configuration` (the lane's)
    /// under `retry_policy`.
    pub fn new(result_entry_id: &str, configuration: Value, retry_policy: Value) -> SummaryAttempt {
        SummaryAttempt {
            context: json!({
                "resultEntryId": result_entry_id,
                "configuration": configuration,
                "streamOptions": stream_options(),
                "retryPolicy": retry_policy,
            }),
            attempt: 1,
            usage_id: None,
        }
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// Start the attempt in progress: `summary.ready`, `summary.effect_pending`, then the same with its request
    /// and a new usage id, each its own transaction.
    pub fn start(&mut self, session: &mut Session, scope: &Scope) -> Result<(), String> {
        self.usage_id = None;
        session.commit(&[scope.write(
            READY,
            vec![
                ("summaryContext", self.context.clone()),
                ("nextAttempt", json!(self.attempt)),
            ],
        )])?;
        session.commit(&[scope.write(EFFECT_PENDING, self.pending_fields())])?;
        let usage_id = lock_clock(&session.clock()).next_id();
        let mut requested = self.pending_fields();
        requested.push(("request", json!({"index": 0, "usageId": usage_id})));
        session.commit(&[scope.write(EFFECT_PENDING, requested)])?;
        self.usage_id = Some(usage_id);
        Ok(())
    }

    /// The wait after a failed attempt ended (`runStructuralRetryWait`): start the next one.
    pub fn retry(&mut self, session: &mut Session, scope: &Scope) -> Result<(), String> {
        self.attempt += 1;
        self.start(session, scope)
    }

    /// A retryable failed attempt (`publishAttemptResult`): its response's usage row (when it reported one) and
    /// `summary.retry_wait` until `not_before` with the next attempt and the error.
    pub fn retry_wait(&self, scope: &Scope, usage: Option<&Value>, error: &str, not_before: i64) -> Vec<Write> {
        let mut writes: Vec<Write> = usage.and_then(|usage| self.usage_row(usage)).into_iter().collect();
        writes.push(scope.write(
            RETRY_WAIT,
            vec![
                ("summaryContext", self.context.clone()),
                ("nextAttempt", json!(self.attempt + 1)),
                ("notBefore", json!(not_before)),
                ("errorMessage", json!(error)),
            ],
        ));
        writes
    }

    /// The summary response arrived: its usage row, with the state that lists it.
    pub fn record_usage(&self, scope: &Scope, usage: &Value) -> Vec<Write> {
        match self.usage_row(usage) {
            Some(row) => vec![row, scope.write(EFFECT_PENDING, self.pending_fields())],
            None => Vec::new(),
        }
    }

    /// `summary.effect_pending` of the attempt with the usage ids it recorded.
    pub fn pending(&self, scope: &Scope) -> Value {
        scope.state(EFFECT_PENDING, self.pending_fields())
    }

    fn pending_fields(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("summaryContext", self.context.clone()),
            ("attempt", json!(self.attempt)),
            ("usageIds", json!(self.usage_id.iter().collect::<Vec<_>>())),
        ]
    }

    fn usage_row(&self, usage: &Value) -> Option<Write> {
        Some(Write::usage(self.usage_id.as_deref()?, usage.clone(), None))
    }
}
