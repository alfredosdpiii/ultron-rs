//! The worker's main lane as presentations see it: the `ultron.transcript` state `{ snapshot, event }`
//! (experimental/services/transcript-provider.ts), folded with the harness reducer (crates/harness) from the
//! lane-watch events the session actor (crates/session-core `core`) reports.

use crate::chord::State;
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
    pub fn publish(&self, harness_event: Value) {
        let entry_added = harness_event["type"] == "entry_added";
        let fold = |state: &mut Value| {
            protocol::watch::reducer::reduce_lane_snapshot(&mut state["snapshot"], &harness_event);
            state["event"] = harness_event;
        };
        if entry_added {
            self.transcript.change(fold);
        } else {
            self.transcript.change_beside("/snapshot/transcript", fold);
        }
    }
}
