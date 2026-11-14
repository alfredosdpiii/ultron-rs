---
name: verify-conversation-observer
description: Check the conversation observer lifecycle model when changing flight, cancellation, completion identity, publication, or usage settlement.
---

# Verify conversation observer

Run from the repository root:

```python
print(await bash('''bash specs/check.sh specs/conversation-observer'''))
```

`specs/check.sh` pins Quint 0.33.1, Apalache 0.62.4 and TLC 1.7.4; Java and the first-run downloads are required. Fixed rules must hold and broken controls must break. Read each result with `pstack/statechart`. The control counterexamples and boundaries are in `specs/conversation-observer/COVERAGE.md`. A failure in fixed needs a code-level replay: distinguish an implementation bug from a model mismatch before changing either. A passing model does not test Rust; run the real observer tests and the real-code negative controls separately.

Coverage: `crates/session-core/src/core/observer.rs` (launch, abort, worker settlement/send, completion, seen/suggestion and commands), `core.rs` (end, completion dispatch, shutdown), `core/durable_turns.rs` (generation), and `core/compacting.rs` (cancellation/dispatch). Sync: `f251598b` plus the uncommitted observer feature; not yet synchronized to a committed feature. When covered code changes, inspect each mapping and assumption, update model and controls if needed, rerun checks and real-code tests, then advance the sync commit only after confirming the model matches the new commit. Use `pstack/maintain-formal-verification-skill` for maintenance.

## Actor and user surfaces

Use an empty test home and no provider credentials. The tests supply loopback fake providers, private server/profile directories, and the real tmux TUI harness. From the repository root:

```python
print(await bash('''env -i HOME=/tmp/ultron-observer-test-home PATH="$HOME/.cargo/bin:/usr/bin:/bin" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" CARGO_TARGET_DIR=../conversation-observer-target scripts/nextest.sh -p session-core -p cli -p interactive --locked -E 'test(observer) | test(cli_interactive_stream_folds_update_frames)''''))
```

The actor tests cover default-on traffic and explicit disable, no-tool requests, forced secret masking, invalid output, budget exhaustion, usage rows, off/abort/close, branch navigation, duplicate topics, and a stalled old-root request while the main root finishes. The CLI test drives `/observer on`, `show`, `dismiss`, `known`, and `off` in tmux and asserts no model request. The RPC test exercises the dedicated command and slash-prompt interception. The stream-fold check interleaves a custom observer notice with assistant deltas.

These are fake-provider behavior checks, not an evaluation of the observer model's judgment. Unknown provider usage after a dropped connection remains unknown; partial-stream usage retention needs its own deterministic provider check. The tests create and clean their own profiles and processes. Never point verification at the user's live server or credentials.
