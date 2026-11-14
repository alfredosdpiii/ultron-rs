# Observer lifecycle model

This is a bounded model of the design, not a proof of Rust. Sync: commit `f251598b` plus the **uncommitted observer feature** in this worktree. Recheck against the final committed code before advancing the sync marker.

## Mapping

- `StartRoot`, `EndRoot`: abstract a live root and its end. `Core::run_end` calls `cancel` (`crates/session-core/src/core.rs:1689`); compaction cancels too (`core/compacting.rs:343`). The model combines root ending and cancellation.
- `Launch`: `Core::observer_generation` gates an idle flight and reserves usage (`core/observer.rs:110-140`, `core/durable_turns.rs:838`). Cadence, budget and reservation failures are nondeterministic non-launches.
- `Disable`, `EndRoot`: `Observer::cancel` aborts the signal but retains `Flight::Running` (`core/observer.rs:56-60`, `:327-336`). The running slot belongs to Core; it is not worker-owned.
- `Settle`, `Send`: worker attempts settlement and usage recording before sending `ObserverCompleted` (`core/observer.rs:256-273`). These are separate steps; settlement success, journal errors and shutdown are not modeled.
- `Complete`: `observer_completed` matches operation **and** request, checks signal, enabled state and live root, joins and releases the flight, then may publish (`core/observer.rs:283-324`). Unique request numbers represent the operation/request pair: each request maps to its operation, so request equality implies pair equality. Stray completions can only arise in the early-release control.
- `Dismiss`, `seen`: commands retain the suggestion across off/root end until dismissed, and accepted lowercase topics stay in a session queue (`core/observer.rs:314-320`, `:327-350`). The two-topic model cannot reach the real 64-entry eviction; `NoRepeatedTopic` applies only while a topic remains in the modeled set.

## Checks and counterexamples

`fixed`: no wrong completion accepted or published, at most one unaccepted worker, no repeated seen topic, and settlement attempted before send. These are safety checks, not eventual completion or proof of successful accounting. All five held across 999 states.

`early_release`: disable clears the slot while worker 1 is running; re-enable and launch worker 2. `AtMostOneWorker` breaks. `broken_identity` combines early release with removal of the identity check: worker 1 settles and sends after worker 2 starts; its completion consumes worker 2's slot and can publish worker 1's suggestion. `IdentitySafe` and `PublicationSafe` break. Identity control needs the early-release fault to produce a mismatched in-process completion; it is not a standalone mutation of the current Rust. These are model counterexamples, not observed Rust failures.

Bounds: two successive roots, three globally unique requests, two nonempty topics, one live suggestion, no process restart. A request's working/settled/queued phases are explicit; no fairness is assumed. All completed work eventually settling is not checked. There is no modeled duplicate transport message, forged completion, topic eviction, parsing/masking, token/cost values, provider behavior, timeout, crash, channel failure, shutdown/join, live-root internals or persistence errors. The model does not execute Rust. The worker can finish physically after sending; the model tracks the slot until Core accepts its completion.

Read the TLC result with `pstack/statechart`. On the combined control its shortest paths are as above; the chart's raw labels and full paths are reproducible with that skill. The model's five invariants do not independently prove every line of the mapping; in particular suggestion retention is encoded by transitions, not a separate checked invariant. A real-code negative control restored the old unconditional TUI MessageEnd reset in an isolated checkout: `cli_interactive_stream_folds_update_frames` failed because the next assistant delta disappeared. The fixed feature version passes. This checks the streaming boundary, not the model identity rule.


Final integration notes: branch navigation calls `Observer::branch_changed`, clearing the current suggestion and topic history while aborting any flight. That branch transition is outside this bounded model. Cancellation keeps any usage already observed through stream events; accounting success and partial-event timing remain outside the model. `/observer` operations and completions are served while compaction runs off-actor. Same-operation suggestions describe their captured context, not necessarily the newest generation; the user documentation states this snapshot limit.

A second real-code negative control removed the seen-topic return in an isolated checkout. `known_or_dismissed_topic_does_not_reappear_in_the_next_root` failed because the duplicate occupied the suggestion slot and suppressed the expected later check. The intact guard passed 15 consecutive focused runs. Both deliberate source mutations were restored after checking.

The observer now defaults on unless `observer.enabled` is explicitly false. The model starts disabled to explore both toggle states; this does not assert a product default. The live `observer.model` setting chooses a catalog provider/model per check; absent or empty inherits the session model. Model resolution and provider credentials are exercised by actor tests, not this lifecycle model.
