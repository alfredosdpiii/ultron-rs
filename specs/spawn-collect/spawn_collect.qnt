// rlm.spawn / rlm.collect across worker restarts: one subagent, its journal record, its durable conversation, and the
// delegation host of whichever worker owns the session.
//
// Code (names a reviewer can open):
//   crates/delegation/src/host.rs          Delegation::spawn (admit, transition "running", then the child starts),
//                                          Delegation::end (journal transition, a reattached task's too),
//                                          handle_with Request::Collect / Results (read the task's outcome)
//   crates/delegation/src/host/restore.rs  Delegation::restore, run_restored
//   crates/harness/src/task_store.rs       TaskJournal::reopen (reopening records every unfinished task interrupted,
//                                          except the reattached ones, kept running), admit / transition
//   crates/session-core/src/subagents.rs   unfinished (read from the stored document before the journal reopens),
//                                          DurableChildren::restore (reattach, abort the conversations not reattached)
//   crates/session-core/src/core.rs        Core start-up (unfinished, then children.restore); hand_over
//
// One step of the model is one critical section of the code: admit and the transition to running are two journal
// writes, so a worker can die between them; the child's conversation starts after the host's lock is released.
// Left out: cancellation, contracts and retry hops, worktrees, nested children, agent (`agents.*`) tasks.
module spawn_collect {
  // How many times the worker may die (hand over or crash) and a new one open the session.
  const MAX_RESTARTS: int
  // Broken variant: restore re-runs an unfinished task instead of reattaching or interrupting it.
  const REPLAY: bool
  // The code: restore keeps a reattached task "running" in the journal, and its end is journaled. FALSE is the code
  // before that fix (the reattached task recorded interrupted at reopening, its end kept in memory only).
  const JOURNAL_REATTACHED: bool

  type Host = { present: bool, outcome: str, reattached: bool }
  pure val NoTask: Host = { present: false, outcome: "", reattached: false }

  // The task's record in the journal (TASKS_ROOT): none, admitted, running, completed, interrupted.
  var journal: str
  // The child's conversation in the session's durable runtime: none, running, done, aborted.
  var conv: str
  // The task in the current worker's delegation host; outcome "" while it runs.
  var host: Host
  // A worker owns the session.
  var up: bool
  var restarts: int
  // How many times the child's work was started (a new conversation).
  var starts: int
  // Every result the parent was given for the child (collect, results, child_done).
  var seen: Set[str]

  pure def unfinished(j: str): bool = j == "admitted" or j == "running"
  // The result a terminal journal record carries.
  pure def resultOf(j: str): str = if (j == "completed") "reply" else j

  action init = all {
    journal' = "none",
    conv' = "none",
    host' = NoTask,
    up' = true,
    restarts' = 0,
    starts' = 0,
    seen' = Set(),
  }

  // Delegation::spawn: TaskJournal::admit writes the record.
  action Admit = all {
    up,
    journal == "none",
    journal' = "admitted",
    host' = { present: true, outcome: "", reattached: false },
    conv' = conv, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  // Delegation::spawn: transition(id, "running") writes the record again, in the same locked section.
  action MarkRunning = all {
    up,
    host.present,
    journal == "admitted",
    journal' = "running",
    conv' = conv, host' = host, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  // Delegation::spawn after the lock: the runner starts the child's conversation.
  action StartChild = all {
    up,
    host.present,
    not(host.reattached),
    journal == "running",
    conv == "none",
    conv' = "running",
    starts' = starts + 1,
    journal' = journal, host' = host, up' = up, restarts' = restarts, seen' = seen,
  }

  // The child's conversation ends with its reply (it runs only while a worker runs the durable runtime).
  action ChildReplies = all {
    up,
    conv == "running",
    conv' = "done",
    journal' = journal, host' = host, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  // Delegation::end for a child this worker started: the journal commits first; a record already terminal wins.
  action HostEnds = all {
    up,
    host.present,
    not(host.reattached),
    host.outcome == "",
    conv == "done",
    val j = if (unfinished(journal)) "completed" else journal
    all {
      journal' = j,
      host' = { ...host, outcome: resultOf(j) },
    },
    conv' = conv, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  // run_restored, then Delegation::end of a reattached task: the journal commits its end, as any child's (before the
  // fix, JOURNAL_REATTACHED = false: the result was kept in memory and the journal kept the record it recovered).
  action ReattachedEnds = all {
    up,
    host.reattached,
    host.outcome == "",
    conv == "done",
    host' = { ...host, outcome: "reply" },
    journal' = if (JOURNAL_REATTACHED) "completed" else journal,
    conv' = conv, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  // rlm.collect / rlm.results / the child_done event: the parent reads the task's outcome.
  action ParentReads = all {
    up,
    host.outcome != "",
    seen' = seen.union(Set(host.outcome)),
    journal' = journal, conv' = conv, host' = host, up' = up, restarts' = restarts, starts' = starts,
  }

  // hand_over or a crash: the worker and its host's memory are gone; the journal and the conversation stay.
  action WorkerDies = all {
    up,
    restarts < MAX_RESTARTS,
    up' = false,
    host' = NoTask,
    restarts' = restarts + 1,
    journal' = journal, conv' = conv, starts' = starts, seen' = seen,
  }

  // The next worker opens the session: subagents::unfinished reads the stored record, Delegation::restore reattaches
  // a child whose conversation the runtime holds while TaskJournal::reopen keeps it running and rewrites every other
  // unfinished record as interrupted (one write), and DurableChildren::restore aborts a running conversation it did
  // not reattach.
  action WorkerStarts = all {
    not(up),
    up' = true,
    val wasUnfinished = unfinished(journal)
    val reattach = wasUnfinished and (conv == "running" or conv == "done")
    if (REPLAY and wasUnfinished)
      all {
        journal' = "running",
        conv' = "running",
        starts' = starts + 1,
        host' = { present: true, outcome: "", reattached: false },
      }
    else
      val j = if (not(wasUnfinished)) journal else if (reattach and JOURNAL_REATTACHED) "running" else "interrupted"
      all {
        journal' = j,
        conv' = if (conv == "running" and not(reattach)) "aborted" else conv,
        starts' = starts,
        host' =
          if (journal == "none") NoTask
          else if (reattach) { present: true, outcome: "", reattached: true }
          else { present: true, outcome: resultOf(j), reattached: false },
      },
    restarts' = restarts, seen' = seen,
  }

  // Nothing left to do: the parent has read the result.
  action Settled = all {
    up,
    host.outcome != "",
    seen.contains(host.outcome),
    journal' = journal, conv' = conv, host' = host, up' = up, restarts' = restarts, starts' = starts, seen' = seen,
  }

  action step = any {
    Admit,
    MarkRunning,
    StartChild,
    ChildReplies,
    HostEnds,
    ReattachedEnds,
    ParentReads,
    WorkerDies,
    WorkerStarts,
    Settled,
  }

  // The child's work starts at most once: a restart never replays it.
  val NeverReplayed = starts <= 1
  // The parent is never given two different results for the child.
  val OneResult = seen.size() <= 1
  // A child whose conversation is still running when a worker opens the session keeps running.
  val KeepsRunning = conv != "aborted"
}

// The code as written, through one restart and through two.
module spawn_collect_one_restart {
  import spawn_collect(MAX_RESTARTS = 1, REPLAY = false, JOURNAL_REATTACHED = true).*
}
module spawn_collect_two_restarts {
  import spawn_collect(MAX_RESTARTS = 2, REPLAY = false, JOURNAL_REATTACHED = true).*
}
// The code before the fix: a reattached task's end kept in memory only.
module spawn_collect_in_memory_two_restarts {
  import spawn_collect(MAX_RESTARTS = 2, REPLAY = false, JOURNAL_REATTACHED = false).*
}
