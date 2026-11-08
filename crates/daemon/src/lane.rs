//! The worker's main lane as presentations see it: the `ultron.transcript` state `{ snapshot, event }`
//! (experimental/services/transcript-provider.ts), folded with the harness reducer (crates/harness) from the
//! lane-watch events the session actor (crates/session-core `core`) reports.

use crate::chord::State;
use protocol::watch::reducer::{Reduction, reduce_lane_snapshot};
use serde_json::{Value, json};
use std::sync::Arc;

/// The transcript service state of one lane.
pub struct Lane {
    pub transcript: Arc<State>,
}

impl Lane {
    pub fn new(snapshot: Value) -> Lane {
        Lane {
            // The provider starts at `{ snapshot: null }` and publishes the lane snapshot once on activation.
            transcript: State::with_sequence(json!({"snapshot": snapshot, "event": null}), 1),
        }
    }

    pub fn snapshot(&self) -> Value {
        self.transcript.value()["snapshot"].clone()
    }

    /// Fold one harness event into the snapshot and publish it as the current event. Only `entry_added` changes the
    /// transcript (protocol::watch::reducer): every other event leaves it aside rather than copying and comparing it.
    /// [`Reduction::Rebase`] (a navigation ended): the snapshot is stale until [`Lane::rebase`] gives it the session's.
    #[must_use]
    pub fn publish(&self, harness_event: Value) -> Option<Reduction> {
        let entry_added = harness_event["type"] == "entry_added";
        let mut reduction = None;
        let fold = |state: &mut Value| {
            reduction = reduce_lane_snapshot(&mut state["snapshot"], &harness_event);
            state["event"] = harness_event;
        };
        if entry_added {
            self.transcript.change(fold);
        } else {
            self.transcript.change_beside("/snapshot/transcript", fold);
        }
        reduction
    }

    /// Take the session's own lane snapshot (`fresh`, session_core `Op::LaneSnapshot`) after navigation `ended` (its
    /// operation id): its transcript, tip, configuration and stats. What only the events know stays: the queues, the
    /// fault, the last result, and an operation that opened since; the navigation itself is over.
    pub fn rebase(&self, fresh: Value, ended: &Value) {
        self.transcript.change(|state| {
            let current = state["snapshot"].take();
            let mut snapshot = fresh;
            for key in ["queues", "faulted", "lastResult"] {
                if let Some(value) = current.get(key) {
                    snapshot[key] = value.clone();
                }
            }
            snapshot["operation"] = match current.get("operation") {
                Some(operation) if !operation.is_null() && operation["id"] != *ended => operation.clone(),
                _ => Value::Null,
            };
            state["snapshot"] = snapshot;
        });
    }
}
