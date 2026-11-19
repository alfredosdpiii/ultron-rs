---- MODULE goal_driver ----
\* The session goal driver (`/goal pause | clear | <new objective>`) against the goal job's end callback: who holds
\* the driver's lock while a job is stopped, and what the job's end calls back into.
\*
\* Code (names a reviewer can open):
\*   crates/kernel-apis/src/goal.rs        GoalDriver::command (holds `state` for the whole command; pause, clear and
\*                                         replace call stop_job under it), GoalDriver::task_ended / job_ended (take
\*                                         `state`), start_job, TaskGoalJobs::stop_job (TaskHost::cancel)
\*   crates/delegation/src/agents.rs       Agents::cancel (claims an unfinished task, commits it on the calling thread,
\*                                         then cancels its unfinished children on threads of their own and joins
\*                                         them), Agents::commit (sets the outcome, then calls on_task_end), finish
\*   crates/session-core/src/compose.rs    on_task_end hook: goal.task_ended(id, result) for every task end
\*
\* One step of the model is one critical section of the code. The lock is `GoalDriver.state` (std Mutex, not
\* re-entrant): a thread that asks for it while it holds it waits forever, and so does a thread whose helper waits for
\* it. Tasks: J is the goal's running job, K a task J started (a typed agent task, for example), N the job a replace or
\* a progressing end starts. TLC is run with -deadlock (a stuck thread is not an error by itself), so the rules name
\* the stuck states.
\* Left out: goal.check / complete / blocked (the stuck path already stops its job from another thread), resume,
\* persistence and its write errors, reopen and reconcile, N's own end, more than one command.

CONSTANTS
  \* The code: command() calls stop_job while it holds the lock. FALSE is the fix: the command changes the goal under
  \* the lock, takes the job to stop out of it, releases the lock, then stops the job.
  STOP_UNDER_LOCK,
  \* J has an unfinished child task K when it is stopped.
  CHILD

\* pcs: "idle" or the step a thread is at; "done" when it has returned.
\* g is a record:
\*   status    the goal: "active", "paused" or "none" (cleared)
\*   job       the goal's job: "J", "N" or ""
\*   lock      who holds GoalDriver.state: "", "cmd", "job", "helper", "child"
\*   kind      the command: "", "pause", "clear", "replace"
\*   stopping  the job the command stops ("" when none)
\*   cmd       the /goal command's thread
\*   helper    the thread Agents::cancel starts to cancel K
\*   jobEnd    J's own task thread ending J
\*   childEnd  K's own task thread ending K
\*   tasks     task -> "running", "finishing" (claimed by a party that will commit it), "ended" (outcome set)
VARIABLE g

init == g = [
    status |-> "active",
    job |-> "J",
    lock |-> "",
    kind |-> "",
    stopping |-> "",
    cmd |-> "idle",
    helper |-> "idle",
    jobEnd |-> "idle",
    childEnd |-> "idle",
    tasks |-> [t \in {"J", "K", "N"} |->
                 IF t = "J" THEN "running" ELSE IF t = "K" THEN (IF CHILD THEN "running" ELSE "ended") ELSE "none"]
  ]

\* The state change of the command itself (goal.rs command): pause and clear leave no job; replace makes a new
\* active goal and starts its job (TaskHost::spawn starts the task on another thread: nothing calls back).
applied(s) ==
  IF s.kind = "pause" THEN [s EXCEPT !.status = "paused", !.job = ""]
  ELSE IF s.kind = "clear" THEN [s EXCEPT !.status = "none", !.job = ""]
  ELSE [s EXCEPT !.status = "active", !.job = "N", !.tasks["N"] = "running"]

\* ---- the /goal command thread ----

CmdStarts ==
  /\ g.cmd = "idle"
  /\ \E k \in {"pause", "clear", "replace"} : g' = [g EXCEPT !.cmd = "lock", !.kind = k]

CmdLocks ==
  /\ g.cmd = "lock"
  /\ g.lock = ""
  /\ g' = [g EXCEPT !.cmd = "body", !.lock = "cmd"]

\* command(): `stop_job(goal, ...)` clears goal.job and, in the code, cancels it right there; the fix applies the
\* whole change and releases the lock first.
CmdBody ==
  /\ g.cmd = "body"
  /\ LET target == IF g.kind = "pause" /\ g.status /= "active" THEN "" ELSE g.job
     IN IF STOP_UNDER_LOCK
        THEN g' = [g EXCEPT !.job = "", !.stopping = target, !.cmd = IF target = "" THEN "after" ELSE "cancel"]
        ELSE g' = [applied([g EXCEPT !.job = ""]) EXCEPT
                     !.stopping = target, !.lock = "", !.cmd = IF target = "" THEN "done" ELSE "cancel"]

\* ---- Agents::cancel(id) on the calling thread (the command's, or a cascade helper's) ----

\* Agents::cancel: an unfinished task is claimed (finishing) and committed by this thread; one another party is
\* finishing is waited for (wait_outcome).
cancelStep(s, t) ==
  IF s.tasks[t] = "running" THEN <<"commit", [s.tasks EXCEPT ![t] = "finishing"]>> ELSE <<"wait", s.tasks>>

CmdCancels ==
  /\ g.cmd = "cancel"
  /\ LET r == cancelStep(g, g.stopping) IN g' = [g EXCEPT !.cmd = r[1], !.tasks = r[2]]

\* Agents::commit: the outcome is set (waiters wake), then on_task_end runs on this thread.
CmdCommits ==
  /\ g.cmd = "commit"
  /\ g' = [g EXCEPT !.cmd = "hook", !.tasks[g.stopping] = "ended"]

\* on_task_end -> GoalDriver::task_ended takes the lock. When this thread already holds it, it waits forever.
CmdHookLocks ==
  /\ g.cmd = "hook"
  /\ g.lock = ""
  /\ g' = [g EXCEPT !.cmd = "hookBody", !.lock = "cmd"]

\* job_ended: the stopped job is no longer the goal's job, so nothing changes; the lock is released.
CmdHookEnds ==
  /\ g.cmd = "hookBody"
  /\ g' = [g EXCEPT !.cmd = "cascade", !.lock = ""]

CmdWaitsOutcome ==
  /\ g.cmd = "wait"
  /\ g.tasks[g.stopping] = "ended"
  /\ g' = [g EXCEPT !.cmd = "cascade"]

\* Agents::cancel: every unfinished child is cancelled on a thread of its own, and the caller joins them.
CmdCascades ==
  /\ g.cmd = "cascade"
  /\ IF g.stopping = "J" /\ g.tasks["K"] /= "ended"
     THEN g' = [g EXCEPT !.cmd = "join", !.helper = "cancel"]
     ELSE g' = [g EXCEPT !.cmd = "after"]

CmdJoins ==
  /\ g.cmd = "join"
  /\ g.helper = "done"
  /\ g' = [g EXCEPT !.cmd = "after"]

\* The code: the rest of the command (set_status, the new goal and its start) still under the lock, then return.
CmdReturns ==
  /\ g.cmd = "after"
  /\ IF STOP_UNDER_LOCK
     THEN g' = [applied(g) EXCEPT !.lock = "", !.cmd = "done"]
     ELSE g' = [g EXCEPT !.cmd = "done"]

\* ---- the cascade helper cancelling K ----

HelperCancels ==
  /\ g.helper = "cancel"
  /\ LET r == cancelStep(g, "K") IN g' = [g EXCEPT !.helper = r[1], !.tasks = r[2]]

HelperCommits ==
  /\ g.helper = "commit"
  /\ g' = [g EXCEPT !.helper = "hook", !.tasks["K"] = "ended"]

HelperHookLocks ==
  /\ g.helper = "hook"
  /\ g.lock = ""
  /\ g' = [g EXCEPT !.helper = "hookBody", !.lock = "helper"]

\* job_ended(K): K is not the goal's job; nothing changes.
HelperHookEnds ==
  /\ g.helper = "hookBody"
  /\ g' = [g EXCEPT !.helper = "done", !.lock = ""]

HelperWaits ==
  /\ g.helper = "wait"
  /\ g.tasks["K"] = "ended"
  /\ g' = [g EXCEPT !.helper = "done"]

\* ---- J's own task thread: its run ends (Agents::finish) ----

\* finish: the first party to end a task claims it; when a cancel already claimed it, the run's thread only waits.
JobEnds ==
  /\ g.jobEnd = "idle"
  /\ g' = IF g.tasks["J"] = "running" THEN [g EXCEPT !.jobEnd = "commit", !.tasks["J"] = "finishing"]
          ELSE [g EXCEPT !.jobEnd = "done"]

JobCommits ==
  /\ g.jobEnd = "commit"
  /\ g' = [g EXCEPT !.jobEnd = "hook", !.tasks["J"] = "ended"]

JobHookLocks ==
  /\ g.jobEnd = "hook"
  /\ g.lock = ""
  /\ g' = [g EXCEPT !.jobEnd = "hookBody", !.lock = "job"]

\* job_ended(J): while J is the active goal's job, a run with progress starts the next job, one without pauses the
\* goal (start_job spawns N; its run starts on another thread).
JobHookEnds ==
  /\ g.jobEnd = "hookBody"
  /\ IF g.job = "J" /\ g.status = "active"
     THEN \E progress \in {TRUE, FALSE} :
            g' = IF progress THEN [g EXCEPT !.jobEnd = "done", !.lock = "", !.job = "N", !.tasks["N"] = "running"]
                 ELSE [g EXCEPT !.jobEnd = "done", !.lock = "", !.job = "", !.status = "paused"]
     ELSE g' = [g EXCEPT !.jobEnd = "done", !.lock = "", !.job = IF g.job = "J" THEN "" ELSE g.job]

\* ---- K's own task thread: its run ends ----

ChildEnds ==
  /\ g.childEnd = "idle"
  /\ g' = IF g.tasks["K"] = "running" THEN [g EXCEPT !.childEnd = "commit", !.tasks["K"] = "finishing"]
          ELSE [g EXCEPT !.childEnd = "done"]

ChildCommits ==
  /\ g.childEnd = "commit"
  /\ g' = [g EXCEPT !.childEnd = "hook", !.tasks["K"] = "ended"]

ChildHookLocks ==
  /\ g.childEnd = "hook"
  /\ g.lock = ""
  /\ g' = [g EXCEPT !.childEnd = "hookBody", !.lock = "child"]

ChildHookEnds ==
  /\ g.childEnd = "hookBody"
  /\ g' = [g EXCEPT !.childEnd = "done", !.lock = ""]

step ==
  \/ CmdStarts \/ CmdLocks \/ CmdBody \/ CmdCancels \/ CmdCommits \/ CmdHookLocks \/ CmdHookEnds
  \/ CmdWaitsOutcome \/ CmdCascades \/ CmdJoins \/ CmdReturns
  \/ HelperCancels \/ HelperCommits \/ HelperHookLocks \/ HelperHookEnds \/ HelperWaits
  \/ JobEnds \/ JobCommits \/ JobHookLocks \/ JobHookEnds
  \/ ChildEnds \/ ChildCommits \/ ChildHookLocks \/ ChildHookEnds

\* No thread waits for the goal lock while it holds it (the command's own cancel calling back into task_ended).
NoSelfWait == ~(
    (g.cmd = "hook" /\ g.lock = "cmd") \/ (g.helper = "hook" /\ g.lock = "helper")
    \/ (g.jobEnd = "hook" /\ g.lock = "job") \/ (g.childEnd = "hook" /\ g.lock = "child"))
\* No thread waits for the lock while its holder waits for that thread (the command joining the cascade helper
\* whose task_ended waits for the command's lock).
NoWaitCycle == ~(g.cmd = "join" /\ g.helper = "hook" /\ g.lock = "cmd")
\* Once every thread has returned, the command has taken effect and the only job still running is the goal's own.
AllDone == g.cmd = "done" /\ g.helper /= "cancel" /\ g.helper /= "commit" /\ g.helper /= "hook"
  /\ g.helper /= "hookBody" /\ g.helper /= "wait" /\ g.jobEnd /= "commit" /\ g.jobEnd /= "hook"
  /\ g.jobEnd /= "hookBody"
NoOrphanJob == AllDone => \A t \in {"J", "N"} : g.tasks[t] /= "running" \/ g.job = t
\* A goal that is not active has no job.
JobFollowsStatus == g.status /= "active" => g.job = ""
====
