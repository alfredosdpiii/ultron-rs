// The session goal driver (`/goal pause | clear | <new objective>`) against the goal job's end callback: who holds
// the driver's lock while a job is stopped, and what the job's end calls back into.
//
// Code (names a reviewer can open):
//   crates/kernel-apis/src/goal.rs        GoalDriver::command (holds `state` for the whole command; pause, clear and
//                                         replace call stop_job under it), GoalDriver::task_ended / job_ended (take
//                                         `state`), start_job, TaskGoalJobs::stop_job (TaskHost::cancel)
//   crates/delegation/src/agents.rs       Agents::cancel (claims an unfinished task, commits it on the calling thread,
//                                         then cancels its unfinished children on threads of their own and joins
//                                         them), Agents::commit (sets the outcome, then calls on_task_end), finish
//   crates/session-core/src/compose.rs    on_task_end hook: goal.task_ended(id, result) for every task end
//
// One step of the model is one critical section of the code. The lock is `GoalDriver.state` (std Mutex, not
// re-entrant): a thread that asks for it while it holds it waits forever, and so does a thread whose helper waits for
// it. Tasks: J is the goal's running job, K a task J started (a typed agent task, for example), N the job a replace or
// a progressing end starts. TLC is run with -deadlock (a stuck thread is not an error by itself), so the rules name
// the stuck states.
// Left out: goal.check / complete / blocked (the stuck path already stops its job from another thread), resume,
// persistence and its write errors, reopen and reconcile, N's own end, more than one command.
module goal_driver {
  // The code: command() calls stop_job while it holds the lock. FALSE is the fix: the command changes the goal under
  // the lock, takes the job to stop out of it, releases the lock, then stops the job.
  const STOP_UNDER_LOCK: bool
  // J has an unfinished child task K when it is stopped.
  const CHILD: bool

  // pcs: "idle" or the step a thread is at; "done" when it has returned.
  type G = {
    status: str,        // the goal: "active", "paused" or "none" (cleared)
    job: str,           // the goal's job: "J", "N" or ""
    lock: str,          // who holds GoalDriver.state: "", "cmd", "job", "helper", "child"
    kind: str,          // the command: "", "pause", "clear", "replace"
    stopping: str,      // the job the command stops ("" when none)
    cmd: str,           // the /goal command's thread
    helper: str,        // the thread Agents::cancel starts to cancel K
    jobEnd: str,        // J's own task thread ending J
    childEnd: str,      // K's own task thread ending K
    tasks: str -> str,  // "running", "finishing" (claimed by a party that will commit it), "ended" (outcome set)
  }

  var g: G

  action init = g' = {
    status: "active",
    job: "J",
    lock: "",
    kind: "",
    stopping: "",
    cmd: "idle",
    helper: "idle",
    jobEnd: "idle",
    childEnd: "idle",
    tasks: Map("J" -> "running", "K" -> (if (CHILD) "running" else "ended"), "N" -> "none"),
  }

  // The state change of the command itself (goal.rs command): pause and clear leave no job; replace makes a new
  // active goal and starts its job (TaskHost::spawn starts the task on another thread: nothing calls back).
  pure def applied(s: G): G =
    if (s.kind == "pause") { ...s, status: "paused", job: "" }
    else if (s.kind == "clear") { ...s, status: "none", job: "" }
    else { ...s, status: "active", job: "N", tasks: s.tasks.set("N", "running") }

  // ---- the /goal command thread ----

  action CmdStarts = all {
    g.cmd == "idle",
    nondet k = Set("pause", "clear", "replace").oneOf()
    g' = { ...g, cmd: "lock", kind: k },
  }

  action CmdLocks = all {
    g.cmd == "lock",
    g.lock == "",
    g' = { ...g, cmd: "body", lock: "cmd" },
  }

  // command(): `stop_job(goal, ...)` clears goal.job and, in the code, cancels it right there; the fix applies the
  // whole change and releases the lock first.
  action CmdBody = all {
    g.cmd == "body",
    val target = if (g.kind == "pause" and g.status != "active") "" else g.job
    if (STOP_UNDER_LOCK)
      g' = { ...g, job: "", stopping: target, cmd: if (target == "") "after" else "cancel" }
    else
      g' = { ...applied({ ...g, job: "" }), stopping: target, lock: "", cmd: if (target == "") "done" else "cancel" },
  }

  // ---- Agents::cancel(id) on the calling thread (the command's, or a cascade helper's) ----

  // Agents::cancel: an unfinished task is claimed (finishing) and committed by this thread; one another party is
  // finishing is waited for (wait_outcome).
  pure def cancelStep(s: G, t: str): (str, str -> str) =
    if (s.tasks.get(t) == "running") ("commit", s.tasks.set(t, "finishing")) else ("wait", s.tasks)

  action CmdCancels = all {
    g.cmd == "cancel",
    val r = cancelStep(g, g.stopping)
    g' = { ...g, cmd: r._1, tasks: r._2 },
  }

  // Agents::commit: the outcome is set (waiters wake), then on_task_end runs on this thread.
  action CmdCommits = all {
    g.cmd == "commit",
    g' = { ...g, cmd: "hook", tasks: g.tasks.set(g.stopping, "ended") },
  }

  // on_task_end -> GoalDriver::task_ended takes the lock. When this thread already holds it, it waits forever.
  action CmdHookLocks = all {
    g.cmd == "hook",
    g.lock == "",
    g' = { ...g, cmd: "hookBody", lock: "cmd" },
  }

  // job_ended: the stopped job is no longer the goal's job, so nothing changes; the lock is released.
  action CmdHookEnds = all {
    g.cmd == "hookBody",
    g' = { ...g, cmd: "cascade", lock: "" },
  }

  action CmdWaitsOutcome = all {
    g.cmd == "wait",
    g.tasks.get(g.stopping) == "ended",
    g' = { ...g, cmd: "cascade" },
  }

  // Agents::cancel: every unfinished child is cancelled on a thread of its own, and the caller joins them.
  action CmdCascades = all {
    g.cmd == "cascade",
    if (g.stopping == "J" and g.tasks.get("K") != "ended")
      g' = { ...g, cmd: "join", helper: "cancel" }
    else
      g' = { ...g, cmd: "after" },
  }

  action CmdJoins = all {
    g.cmd == "join",
    g.helper == "done",
    g' = { ...g, cmd: "after" },
  }

  // The code: the rest of the command (set_status, the new goal and its start) still under the lock, then return.
  action CmdReturns = all {
    g.cmd == "after",
    if (STOP_UNDER_LOCK)
      g' = { ...applied(g), lock: "", cmd: "done" }
    else
      g' = { ...g, cmd: "done" },
  }

  // ---- the cascade helper cancelling K ----

  action HelperCancels = all {
    g.helper == "cancel",
    val r = cancelStep(g, "K")
    g' = { ...g, helper: r._1, tasks: r._2 },
  }

  action HelperCommits = all {
    g.helper == "commit",
    g' = { ...g, helper: "hook", tasks: g.tasks.set("K", "ended") },
  }

  action HelperHookLocks = all {
    g.helper == "hook",
    g.lock == "",
    g' = { ...g, helper: "hookBody", lock: "helper" },
  }

  // job_ended(K): K is not the goal's job; nothing changes.
  action HelperHookEnds = all {
    g.helper == "hookBody",
    g' = { ...g, helper: "done", lock: "" },
  }

  action HelperWaits = all {
    g.helper == "wait",
    g.tasks.get("K") == "ended",
    g' = { ...g, helper: "done" },
  }

  // ---- J's own task thread: its run ends (Agents::finish) ----

  // finish: the first party to end a task claims it; when a cancel already claimed it, the run's thread only waits.
  action JobEnds = all {
    g.jobEnd == "idle",
    g' = if (g.tasks.get("J") == "running") { ...g, jobEnd: "commit", tasks: g.tasks.set("J", "finishing") }
         else { ...g, jobEnd: "done" },
  }

  action JobCommits = all {
    g.jobEnd == "commit",
    g' = { ...g, jobEnd: "hook", tasks: g.tasks.set("J", "ended") },
  }

  action JobHookLocks = all {
    g.jobEnd == "hook",
    g.lock == "",
    g' = { ...g, jobEnd: "hookBody", lock: "job" },
  }

  // job_ended(J): while J is the active goal's job, a run with progress starts the next job, one without pauses the
  // goal (start_job spawns N; its run starts on another thread).
  action JobHookEnds = all {
    g.jobEnd == "hookBody",
    if (g.job == "J" and g.status == "active")
      nondet progress = Set(true, false).oneOf()
      g' = if (progress) { ...g, jobEnd: "done", lock: "", job: "N", tasks: g.tasks.set("N", "running") }
           else { ...g, jobEnd: "done", lock: "", job: "", status: "paused" }
    else
      g' = { ...g, jobEnd: "done", lock: "", job: if (g.job == "J") "" else g.job },
  }

  // ---- K's own task thread: its run ends ----

  action ChildEnds = all {
    g.childEnd == "idle",
    g' = if (g.tasks.get("K") == "running") { ...g, childEnd: "commit", tasks: g.tasks.set("K", "finishing") }
         else { ...g, childEnd: "done" },
  }

  action ChildCommits = all {
    g.childEnd == "commit",
    g' = { ...g, childEnd: "hook", tasks: g.tasks.set("K", "ended") },
  }

  action ChildHookLocks = all {
    g.childEnd == "hook",
    g.lock == "",
    g' = { ...g, childEnd: "hookBody", lock: "child" },
  }

  action ChildHookEnds = all {
    g.childEnd == "hookBody",
    g' = { ...g, childEnd: "done", lock: "" },
  }

  action step = any {
    CmdStarts, CmdLocks, CmdBody, CmdCancels, CmdCommits, CmdHookLocks, CmdHookEnds, CmdWaitsOutcome, CmdCascades,
    CmdJoins, CmdReturns,
    HelperCancels, HelperCommits, HelperHookLocks, HelperHookEnds, HelperWaits,
    JobEnds, JobCommits, JobHookLocks, JobHookEnds,
    ChildEnds, ChildCommits, ChildHookLocks, ChildHookEnds,
  }

  // No thread waits for the goal lock while it holds it (the command's own cancel calling back into task_ended).
  val NoSelfWait = not(
    (g.cmd == "hook" and g.lock == "cmd") or (g.helper == "hook" and g.lock == "helper")
    or (g.jobEnd == "hook" and g.lock == "job") or (g.childEnd == "hook" and g.lock == "child")
  )
  // No thread waits for the lock while its holder waits for that thread (the command joining the cascade helper
  // whose task_ended waits for the command's lock).
  val NoWaitCycle = not(g.cmd == "join" and g.helper == "hook" and g.lock == "cmd")
  // Once every thread has returned, the command has taken effect and the only job still running is the goal's own.
  val AllDone = g.cmd == "done" and g.helper != "cancel" and g.helper != "commit" and g.helper != "hook"
    and g.helper != "hookBody" and g.helper != "wait" and g.jobEnd != "commit" and g.jobEnd != "hook"
    and g.jobEnd != "hookBody"
  val NoOrphanJob = AllDone implies Set("J", "N").forall(t => g.tasks.get(t) != "running" or g.job == t)
  // A goal that is not active has no job.
  val JobFollowsStatus = g.status != "active" implies g.job == ""
}

module goal_driver_code {
  import goal_driver(STOP_UNDER_LOCK = true, CHILD = true).*
}
module goal_driver_fixed {
  import goal_driver(STOP_UNDER_LOCK = false, CHILD = true).*
}
