---
name: verify-spawn-collect
description: "Model-check rlm.spawn / rlm.collect across worker restarts (a subagent's work never replayed, its result stable, a reattached child kept running) with the Quint spec in specs/spawn-collect. Use when a change touches the delegation host's spawn, end, collect or restore, the task journal's recovery, or how a new worker reattaches subagents."
---

# Verify spawn/collect across restarts

`specs/spawn-collect/spawn_collect.qnt` models one subagent through worker restarts (a hand-over or a crash, then the
next worker opening the session): its record in the task journal, its conversation in the durable runtime, and the
delegation host of whichever worker owns the session. TLC checks every order of those steps.

## The rules

- **NeverReplayed** (`starts <= 1`): a subagent's work starts at most once. Harm if broken: a restart runs the same
  task twice (double edits, double spend).
- **OneResult** (`seen.size() <= 1`): the parent is never given two different results for one subagent. Harm: the
  parent acts on a result that a later collect contradicts.
- **KeepsRunning** (`conv != "aborted"`): a subagent whose conversation still runs when a worker opens the session keeps
  running (restore.rs promises it). Harm: finished-for-nothing work and an `interrupted` result for a live child.

## Run

From the repo root (about 20 s; the first run downloads Apalache, about 200 MB, and TLC 1.7.4, and needs Java 11 or
later, Node's `npx` and `python3`):

```python
print(await bash('''bash specs/check.sh specs/spawn-collect'''))
```

Each line is `ok|DIFF <config> <rule> <verdict>`; exit 0 means every verdict is the one `specs/spawn-collect/expected`
records. Tool versions are pinned in `specs/check.sh` (Quint 0.33.1, Apalache 0.62.4, TLC 1.7.4); `QUINT`,
`TLA2TOOLS` and `JAVA` point it at local copies. While editing the model, a simulation answers in seconds:

```python
print(await bash('''npx --yes @informalsystems/quint@0.33.1 run specs/spawn-collect/spawn_collect.qnt --main spawn_collect_two_restarts --invariant OneResult --max-steps 20'''))
```

To see a rule's shortest path in plain steps, or a chart of the model, read the spec and one of its configs with the
statechart skill (`/skill:statechart`).

## Configs

| config | what it is | expected |
| --- | --- | --- |
| `code_one_restart` | the code, one restart | all hold |
| `code_two_restarts` | the code, two restarts | all hold |
| `broken_replay` | restore re-runs an unfinished task (`REPLAY`) | NeverReplayed breaks |
| `broken_in_memory` | the code before the fix: a reattached task recorded `interrupted` when the journal reopens and its end kept in memory only (`JOURNAL_REATTACHED = FALSE`) | NeverReplayed holds; OneResult and KeepsRunning break |

The broken variants show every rule can fail: `broken_replay` NeverReplayed, `broken_in_memory` the other two.

## Findings (fixed)

Both needed two restarts while one subagent lives; they were found at `5b47a9e7` and replayed against the code with
session-core's `reopen_support` workers (three workers over one session, two hand-overs). `broken_in_memory` keeps
them as the model's evidence.

1. **KeepsRunning broke.** The root spawns a child; the worker hands over while the child runs; the next worker
   reattached it (`Delegation::restore`) but `TaskJournal::load` had already rewritten its record `interrupted`; that
   worker handed over too while the child still ran; the third worker's `subagents::unfinished` read `interrupted`, so
   the child was not reattached and `DurableChildren::restore` aborted its conversation.
2. **OneResult broke.** Same start; in the second worker the reattached child ended and the root's `rlm.collect`
   returned `succeeded` (kept in memory only: `Delegation::end` did not journal a reattached task); after the next
   hand-over the third worker's `rlm.collect` returned `interrupted` for the same child.

Fix: `TaskJournal::reopen` (crates/harness/src/task_store.rs) keeps the tasks `Delegation::restore` reattaches
`running`, in the same write that records the others `interrupted`, and `Delegation::end` journals a reattached
task's end as any child's. Regression tests: crates/session-core/tests/spawn_two_restarts.rs (both traces, real
workers) and crates/harness/tests/task_reopen.rs.

## Bounds and what is left out

One subagent, up to two restarts. Left out: cancellation (`delete_subagent`, cancelling ancestors), contracts and
retry hops, worktrees, nested children, typed agent tasks (`agents.*`, never reattached), the child_done event channel
as distinct from collect (both read the same outcome), and journal write failures. A pass covers this model only.

## Coverage

Sync commit: `5b47a9e7` plus the reattached-journal fix (N2 T4; move this to its commit once it lands). The model was
last confirmed to describe the code there.

- `crates/delegation/src/host.rs`: `Delegation::spawn` (admit, transition to running, child start), `Delegation::end`
  (journal commit, a reattached task's too), `handle_with` `Request::Collect` / `Request::Results`.
- `crates/delegation/src/host/restore.rs`: `Delegation::restore`, `run_restored`.
- `crates/harness/src/task_store.rs`: `TaskJournal::reopen` / `load` (unfinished → interrupted, reattached ones
  kept running), `admit`, `transition`.
- `crates/session-core/src/subagents.rs`: `unfinished`, `DurableChildren::restore`.
- `crates/session-core/src/core.rs`: start-up order (`unfinished` before `children.restore`), `hand_over`.

## When the check disagrees

A `DIFF` is either a bug or a model that no longer describes the code. Replay the path step by step against the code
(each action's comment names its code). If a step cannot happen in the code, fix the model and say what was wrong. If
every step can happen, it is a bug: report the path in the code's words. Never weaken a rule or edit `expected` to make
a run pass.

When a change to covered code is intended (for example the fix for a finding), update the model from the code first,
then the config's line in `expected`, and keep `broken_replay` breaking NeverReplayed. Move the sync commit only after
you have read the covered code at the new commit and confirmed the model describes it. For drift across many commits,
use `/skill:maintain-formal-verification-skill`.
