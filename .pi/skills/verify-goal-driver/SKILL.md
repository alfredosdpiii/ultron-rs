---
name: verify-goal-driver
description: "Model-check that /goal pause, clear and replace never deadlock on the goal driver's lock (the stopped job's end calls back into the driver) with the Quint spec in specs/goal-driver. Use when a change touches GoalDriver::command, task_ended / job_ended, TaskGoalJobs, Agents::cancel or commit, or the on_task_end hook."
---

# Verify the goal driver's lock

`specs/goal-driver/goal_driver.qnt` models one `/goal pause | clear | <new objective>` against the goal's running job
J: the command's thread, J's own task thread ending J, a child task K of J with its own thread, and the helper thread
`Agents::cancel` starts to cancel K. The goal driver's lock (`GoalDriver.state`, a std Mutex, not re-entrant) and each
task's state (running, finishing, ended) are explicit. TLC checks every order of those steps.

## The rules

- **NoSelfWait**: no thread asks for the goal lock while it holds it. Harm if broken: the `/goal` command never
  returns, and every later `goal.*` request and goal task end waits forever.
- **NoWaitCycle**: the command never joins the cascade helper while the helper waits for the command's lock. Harm: the
  same hang, through a second thread.
- **NoOrphanJob**: once every thread has returned, the only goal job still running is the goal's own.
- **JobFollowsStatus**: a goal that is not active has no job.

## Run

From the repo root (about 1 min; the first run downloads Apalache and TLC, see `specs/check.sh`):

```python
print(await bash('''bash specs/check.sh specs/goal-driver'''))
```

## Configs

| config | what it is | expected |
| --- | --- | --- |
| `code` | the code at c1976389: stop_job under the lock, J has a child | NoSelfWait and NoWaitCycle break |
| `code_no_child` | the same, J has no child | NoSelfWait breaks, NoWaitCycle holds |
| `fixed` | the fix: change the goal under the lock, release it, then stop the job | all hold |

## Findings

1. **NoSelfWait breaks** (B report #1): `command("pause")` takes the lock, `stop_job` -> `TaskHost::cancel` ->
   `Agents::cancel` claims the running J and commits it on the same thread -> `on_task_end` -> `goal.task_ended` asks
   for the lock the thread holds.
2. **NoWaitCycle breaks** (new): J's run is already ending on its own thread when the user pauses. The command takes
   the lock; `Agents::cancel(J)` finds J finishing and waits for its outcome (J's thread sets it before its hook, so
   this wait ends); then it cancels J's unfinished child K on a helper thread and joins it. The helper commits K and
   its `task_ended` waits for the command's lock: the command waits for the helper forever. Releasing the lock
   before stopping the job fixes both.

## Bounds and what is left out

One command, one job with one child, the next job N started but not ended. Left out: goal.check / complete /
blocked (the stuck path already stops its job from a thread of its own after 100 ms), resume, persistence and its
write errors, reopen and reconcile. A pass covers this model only.

## When the check disagrees

A `DIFF` is either a bug or a model that no longer describes the code. Replay the path step by step against the code
(each action's comment names its code). Never weaken a rule or edit `expected` to make a run pass; when the fix lands,
move `code` to the fixed behaviour and keep a broken config that shows each rule can fail.
