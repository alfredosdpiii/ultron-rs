---
name: verify-schedule-fire
description: "Model-check that a scheduled prompt (schedule.every / schedule.at) fires each slot once and the server keeps one delivery in flight, with the Quint spec in specs/schedule-fire. Use when a change touches the server's schedule timer, ScheduleModule::fire_with, PR watch/loop registration or cancellation, or how the session worker starts a fired prompt."
---

# Verify schedule single-flight

`specs/schedule-fire/schedule_fire.qnt` models one `every` schedule: the stored value's next slot, two server
generations (an old server still firing while a new one reads the journals), their deliveries, the worker's
`fire_with` (check and start, then record), and, in the faults config, a worker that dies or a record that fails
between the start and the record. TLC checks every order of those steps over three slots.

## The rules

- **OneRunPerSlot**: each slot's prompt starts (or is queued) at most once. Harm if broken: the same scheduled turn
  runs twice, and the user pays for both.
- **OneDeliveryInFlight**: a server generation has at most one delivery of a schedule in flight. Harm: the timer piles
  up fires on a slow worker.

## Run

From the repo root (about 20 s; the first run downloads Apalache, about 200 MB, and TLC 1.7.4, and needs Java 11 or
later, Node's `npx` and `python3`):

```python
print(await bash('''bash specs/check.sh specs/schedule-fire'''))
print(await bash('''bash specs/check.sh specs/pr-watch'''))
```

Each line is `ok|DIFF <config> <rule> <verdict>`; exit 0 means every verdict is the one `specs/schedule-fire/expected`
records. Tool versions are pinned in `specs/check.sh` (Quint 0.33.1, Apalache 0.62.4, TLC 1.7.4); `QUINT`,
`TLA2TOOLS` and `JAVA` point it at local copies. While editing the model, a simulation answers in seconds:

```python
print(await bash('''npx --yes @informalsystems/quint@0.33.1 run specs/schedule-fire/schedule_fire.qnt --main schedule_fire_faults --invariant OneRunPerSlot --max-steps 25'''))
```

To see a rule's shortest path in plain steps, read the spec and one of its configs with the statechart skill
(`/skill:statechart`).

## Configs

| config | what it is | expected |
| --- | --- | --- |
| `code` | the code, no faults | both hold |
| `faults` | the code; the worker may die, or the record may fail, after the start | OneRunPerSlot breaks (finding below) |
| `broken_check_due` | `fire_with` without its `next_due != Some(due)` refusal | OneRunPerSlot breaks |
| `broken_unlocked` | `fire_with` without the `writing` lock | OneRunPerSlot breaks |
| `broken_in_flight` | `Scheduler::run` without its `in_flight` skip | OneDeliveryInFlight breaks |

Each `broken_*` config shows the check it names is what keeps its rule.

## Finding (at the sync commit)

**OneRunPerSlot breaks under faults.** `fire_with` starts the run (`deliver`, which is `Core::schedule_start`) before
it records it (`put`). If the put fails, `fire_with` returns the error, the server gets `Retry`, the slot is still the
stored next slot, and the retry starts the run again. A worker that dies between the two steps does the same in the
model (a started run resumes when the session opens; this path was not replayed). Replay of the put failure: a `ScheduleModule` over a store whose next write fails ran
`fire_with(id, due)` twice for one slot; the first returned `Err("journal write failed")`, the second fired, and the
prompt started twice. The module's doc chooses this order on purpose (a fire counts only once its prompt was taken, so
a failed start never loses a slot); the model shows its cost: at-least-once, not exactly-once, when the record fails.
Do not edit product code from this skill; the trade-off is the owner's call.

## Bounds and what is left out

One `every` schedule with an interval of one time unit, three slots, two server generations, at most two copies of a
delivery. Left out: `at` schedules, `until` and `max_runs`, cancellation, the retry delay (`RETRY_MS`; a retried slot
is simply due again), a worker clock behind the server's, sessions that are gone, the queued-run path's loss on a
crash (a queued fire that cannot start is dropped by design), and Claude Code's own timer (`claude-root` schedules,
which calls the same `fire_with`). A pass covers this model only.

## Coverage

Sync base: `7baa7310` plus the uncommitted watch/loop feature. Rechecked the changes since `5b47a9e7` (the only covered-code addition was `skip`, outside unconditional delivery). Move this marker to the feature commit only after it exists. No historical failure was removed.

- `crates/daemon/src/prompt_schedules.rs`: `Scheduler::run` (in-flight skip, due test), `Scheduler::delivered`
  (Done / Retry answers), `Scheduler::start`, `Scheduler::update`.
- `crates/kernel-apis/src/prompt_schedule.rs`: `ScheduleModule::fire_with` (the `writing` lock, the due check,
  deliver then put), `Schedule::next_after`.
- `crates/session-core/src/core/schedule_ops.rs`: `Core::schedule_fire`, `Core::schedule_start`.

## When the check disagrees

A `DIFF` is either a bug or a model that no longer describes the code. Replay the path step by step against the code
(each action's comment names its code). If a step cannot happen in the code, fix the model and say what was wrong. If
every step can happen, it is a bug: report the path in the code's words. Never weaken a rule or edit `expected` to make
a run pass.

When a change to covered code is intended, update the model from the code first, then `expected`, and keep each
`broken_*` config breaking its rule. Move the sync commit only after you have read the covered code at the new commit
and confirmed the model describes it. For drift across many commits, use `/skill:maintain-formal-verification-skill`.


## PR condition and cancellation model

`specs/pr-watch/pr_watch.qnt` covers the watch-specific branch of `ScheduleModule::create`, `fire_with`, `cancel`,
and `prompt_schedule/watch.rs`'s `Watch::observe`. Network reads begin outside the write lock; applying a read
rechecks the durable value, so an in-flight read cannot resurrect a cancelled watch. A restart discards an in-flight
read and retains the durable baseline, error count and slot. The unconditional `schedule-fire` model still describes
loop delivery, including its at-least-once fault finding. The PR model bounds one pinned PR, two open snapshots,
three polls, two duplicate registrations, a terminal snapshot and fetch failures. It abstracts closed, merged and
babysit merge-ready into the same terminal observation. Initial report-only baseline and the second consecutive
failure are quiet; changed status, recovery, first and third failures wake.

- **NoUnchangedWake**: a quiet baseline/identical observation never starts a model turn. Broken filter wakes on baseline.
- **NoWakeAfterCancel**: an in-flight fetch cannot start a turn after cancellation. Broken recheck applies a terminal
  result after cancel and wakes.
- **OneRegistration**: repeating a start for the same active PR retains one registration. Broken dedup creates two.
- **StopsAtTerminal**: closed/merged/merge-ready or the third failure ends polling. Broken stop leaves it active.

Run both specs above; every `broken_*` config must still break its named rule. Read results through the bundled
pstack statechart skill: each failing rule is a shortest-path finding, even when it is an intentional broken variant.
The PR model excludes gh transport, readiness parsing, wall-clock expiry, wake budgets, journal write/delivery faults,
babysit's initial wake, and session actor queueing. Real scripted-gh and Python bridge checks cover those paths:

```python
print(await bash('''scripts/nextest.sh -p kernel-apis -p session-core -p daemon --locked prompt_schedule'''))
```

The code-level negative check removes the complete post-fetch active/due/value guard, runs
`watch_pr_cancel_during_poll_rejects_stale_result_without_holding_write_lock`, observes a failure, then restores the
code. Run that check again after any change to fetch/apply ordering. The queue cancellation test covers a final
one-run loop already counted as done but queued behind an active turn. Cancellation cannot interrupt work that
has already begun. A queued fire lost on crash and at-least-once delivery under journal failure remain limitations.

Offline agents must set `QUINT`, `QUINT_HOME`, `TLA2TOOLS`, `JAVA` to existing installations and put actual Node/Java
binaries on PATH (not a version-manager shim needing trust). Never install tools or change trust to run this skill
without permission. Use an empty HOME/no provider credentials for the Rust tests.
