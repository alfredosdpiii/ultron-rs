---
name: verify-worker-ownership
description: "Model-check session worker ownership (attachments and the worker's demand ledger, session_demand timeouts, retirement, re-attach after a crash restart) with the Quint spec in specs/worker-ownership: a client is never moved to a session it left, no demand leaks, a worker without clients retires. Use when a change touches Manager::attach_client / release_attachment / apply_demand, the worker's demand handling or Lifecycle::reconcile, or Hub::attach / detach / invalidate / reattach."
---

# Verify worker ownership and re-attach

`specs/worker-ownership/worker_ownership.qnt` models one client connection, two sessions A and B, one server
generation: the router (`by_conn`, `reconnecting`, the `serialized` lock, the attach procedure and the reattach
thread), the manager's `apply_demand` with its timeout, each session's worker (demand set, initial grace timer,
retirement, a crash with a restart) and the FIFO channel to it. TLC checks every order of those steps.

## The rules

- **StaysWhereItChose**: outside the client's own request, the client is attached only to the session it chose last.
  Harm if broken: a client is moved back to a session it left (its calls fail `session_not_attached`, the session it
  chose loses its demand and may retire).
- **NoDemandLeak**: at rest, a worker holds demand only for attachments the server still has.
- **RetiresWhenIdle**: at rest, every live worker has a client attached. Harm if broken: a worker nobody uses never
  retires while the server lives.

## Run

From the repo root (about 15 min, most of it the `fixed` config; one config: `bash specs/check.sh
specs/worker-ownership code`):

```python
print(await bash('''bash specs/check.sh specs/worker-ownership'''))
```

## Configs

| config | what it is | expected |
| --- | --- | --- |
| `code` | the code at c1976389, reliable transport | StaysWhereItChose breaks |
| `code_lossy` | the same, one server -> worker message may be lost | NoDemandLeak, RetiresWhenIdle break |
| `attach_clears` | attach removes `reconnecting` at its start; reattach checks before `serialized` | StaysWhereItChose breaks |
| `fixed_start_only` | also: reattach checks again under `serialized`, a detach ledger; clears only at the start | StaysWhereItChose breaks |
| `fixed` | the full fix: clears at the end too, recheck under the lock, detach ledger; lossy | all hold |

## Findings

1. **StaysWhereItChose, code** (C #2, a shorter path than the report's): the client is on A; A's worker crashes with a
   restart pending; invalidate puts the client in `reconnecting`; the restart comes back and reattach reads
   `still == true`; then the client detaches (which removes `reconnecting`); reattach takes `serialized` and attaches
   it to A anyway. The report's path (the client attaches to B, reattach moves it back to A) breaks the rule too.
2. **StaysWhereItChose, attach_clears** (new): the fix "attach removes `reconnecting`" alone does not close it,
   because reattach checks `reconnecting` before it takes `serialized`: the same detach path.
3. **StaysWhereItChose, fixed_start_only** (new): with the check repeated under the lock, clearing `reconnecting`
   only at the start of attach still breaks: the client on A asks for B; A's worker crashes before the attach takes
   by_conn off A, so invalidate marks the client reconnecting to A; the attach to B fails (its new worker retired);
   reattach then attaches the client to A. Clearing `reconnecting` when the client's attach returns closes it.
4. **NoDemandLeak and RetiresWhenIdle, code_lossy** (C #1): an attach times out; its compensating detach times out
   too and counts as applied; the slow worker applies the attach; the detach is lost. The worker keeps a demand no
   attachment backs and never retires. With a reliable channel (`code`) both hold: a timed-out detach is still
   delivered, in order, and applied late. The ledger (`fixed`) holds with the loss.

## Bounds and what is left out

One client, two sessions, at most two workers per session, one crash, one lost message, three attachments, seven
demand messages (`StateBound`). Left out: server generations, discovery and adoption, orphan grace, release on
shutdown, operations and retirement holds, heartbeats, restart backoff and give-up, subscriptions, more clients.
A pass covers this model only.

## When the check disagrees

A `DIFF` is either a bug or a model that no longer describes the code. Replay the path step by step against the code
(each action's comment names its code). Never weaken a rule or edit `expected` to make a run pass.
