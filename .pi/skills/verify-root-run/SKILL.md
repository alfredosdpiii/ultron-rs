---
name: verify-root-run
description: "Model-check the root run's lifecycle across the session actor, the run journal (`ultron.op.*`, its `at` states) and the durable runtime through worker restarts (a run ends once, recovery never loses or invents a run, a finished run is never reported failed, a queued steer reaches the model once) with the Quint spec in specs/root-run. Use when a change touches Run::admit_with / finish, recovery::settle, Core::open's settle-or-reopen choice, reopen_run, catch_up, submit_queued, harness_idle / run_end, or how a delivered queued message is matched to its queue item."
---

# Verify the root run lifecycle

`specs/root-run/root_run.qnt` models one root run and two queued messages with the same text (a follow-up, then a
steer) through worker restarts: the run journal in the session file (the open operation, its `at`, OP_RESULT, the
pending entries of the lane's inbox), the durable runtime (the prompt's submission, the run, its answers, each queued
message's submission: queued, placed, done) and the actor's memory (`Core.run`, the queue items, a held error). A
legacy init starts from a run an older build left open at any `at`, with no durable records. TLC checks every order.

## The rules

- **EndsOnce**: one terminal record (OP_RESULT) and one end reported for the run.
- **NotFailedWhenFinished**: a run whose last answer committed is never reported failed.
- **NoLostRun**: once nothing is left to do, no operation is open (a run open on disk with no one running it is
  lost until the next restart).
- **NoInventedRun**: the durable runtime runs only a run the journal holds open.
- **DeliveredOnce**: a queued message reaches the model at most once.
- **NoLostMessage**: at rest every queued message reached the model.

## Run

From the repo root (about 2 min):

```python
print(await bash('''bash specs/check.sh specs/root-run'''))
```

## Configs

| config | what it is | expected |
| --- | --- | --- |
| `code` | the code at c1976389, one restart | DeliveredOnce breaks |
| `code_faults` | the same, run_end's terminal commit may fail once | EndsOnce, NotFailedWhenFinished, NoLostRun break |
| `code_legacy` | a run an older build left open; settle | NotFailedWhenFinished breaks |
| `fixed` | match by id, settle every `at` by name, commit before forgetting the run; faults, two restarts | all hold |
| `fixed_legacy` | the legacy start with the settle fix | all hold |

## Findings

1. **DeliveredOnce** (A #5): the follow-up m1 and the steer m2 ("continue" twice) are queued; the runtime places m2
   in a generation; the actor matches it by text to m1 and commits m1's entry (m1's pending entry is gone, m2's is
   left); the answer commits (m2's submission is Done); the worker dies; the next worker reads m2 from the inbox,
   finds its submission Done, not Queued/Placed, and submits it again: the model gets m2 twice.
2. **EndsOnce, NotFailedWhenFinished, NoLostRun** (new): the run's last answer commits and the runtime goes idle;
   run_end takes `Core.run`, the terminal commit fails, and the run is reported failed; the operation stays open on
   disk with no run in memory (lost); the next worker reopens it and ends it again, completed.
3. **NotFailedWhenFinished, legacy** (A #2): a run left in its last checkpoint's compaction (`summary.*` resuming a
   may-finish checkpoint) is settled `failed`, although its last answer committed (reopen's may_finish treats the
   same state as finished).

## Bounds and what is left out

One run, two queued messages, answers that end or fail the run, one restart (two in `fixed`), at most one failed
commit. Left out: cancellation, tool rounds and cells, retries and overflow, next-run messages, compaction other
than the legacy checkpoint compaction, the projection's own write failures, and the gap between enqueue's journal
write and its durable submit. A pass covers this model only.

## When the check disagrees

A `DIFF` is either a bug or a model that no longer describes the code. Replay the path step by step against the code
(each action's comment names its code). Never weaken a rule or edit `expected` to make a run pass; when a fix lands,
move `code` to it and keep a config that shows each rule can fail.
