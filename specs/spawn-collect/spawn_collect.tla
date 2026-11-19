---- MODULE spawn_collect ----
\* rlm.spawn / rlm.collect across worker restarts: one subagent, its journal record, its durable conversation, and the
\* delegation host of whichever worker owns the session.
\*
\* Code (names a reviewer can open):
\*   crates/delegation/src/host.rs          Delegation::spawn (admit, transition "running", then the child starts),
\*                                          Delegation::end (journal transition, a reattached task's too),
\*                                          handle_with Request::Collect / Results (read the task's outcome)
\*   crates/delegation/src/host/restore.rs  Delegation::restore, run_restored
\*   crates/harness/src/task_store.rs       TaskJournal::reopen (reopening records every unfinished task interrupted,
\*                                          except the reattached ones, kept running), admit / transition
\*   crates/session-core/src/subagents.rs   unfinished (read from the stored document before the journal reopens),
\*                                          DurableChildren::restore (reattach, abort the conversations not reattached)
\*   crates/session-core/src/core.rs        Core start-up (unfinished, then children.restore); hand_over
\*
\* One step of the model is one critical section of the code: admit and the transition to running are two journal
\* writes, so a worker can die between them; the child's conversation starts after the host's lock is released.
\* Left out: cancellation, contracts and retry hops, worktrees, nested children, agent (`agents.*`) tasks.
EXTENDS Naturals, FiniteSets

CONSTANTS
  \* How many times the worker may die (hand over or crash) and a new one open the session.
  MAX_RESTARTS,
  \* Broken variant: restore re-runs an unfinished task instead of reattaching or interrupting it.
  REPLAY,
  \* The code: restore keeps a reattached task "running" in the journal, and its end is journaled. FALSE is the code
  \* before that fix (the reattached task recorded interrupted at reopening, its end kept in memory only).
  JOURNAL_REATTACHED

\* Host: [present: BOOLEAN, outcome: STRING, reattached: BOOLEAN]
NoTask == [present |-> FALSE, outcome |-> "", reattached |-> FALSE]

VARIABLES
  \* The task's record in the journal (TASKS_ROOT): none, admitted, running, completed, interrupted.
  journal,
  \* The child's conversation in the session's durable runtime: none, running, done, aborted.
  conv,
  \* The task in the current worker's delegation host; outcome "" while it runs.
  host,
  \* A worker owns the session.
  up,
  restarts,
  \* How many times the child's work was started (a new conversation).
  starts,
  \* Every result the parent was given for the child (collect, results, child_done).
  seen

unfinished(j) == j = "admitted" \/ j = "running"
\* The result a terminal journal record carries.
resultOf(j) == IF j = "completed" THEN "reply" ELSE j

init ==
  /\ journal = "none"
  /\ conv = "none"
  /\ host = NoTask
  /\ up = TRUE
  /\ restarts = 0
  /\ starts = 0
  /\ seen = {}

\* Delegation::spawn: TaskJournal::admit writes the record.
Admit ==
  /\ up
  /\ journal = "none"
  /\ journal' = "admitted"
  /\ host' = [present |-> TRUE, outcome |-> "", reattached |-> FALSE]
  /\ UNCHANGED <<conv, up, restarts, starts, seen>>

\* Delegation::spawn: transition(id, "running") writes the record again, in the same locked section.
MarkRunning ==
  /\ up
  /\ host.present
  /\ journal = "admitted"
  /\ journal' = "running"
  /\ UNCHANGED <<conv, host, up, restarts, starts, seen>>

\* Delegation::spawn after the lock: the runner starts the child's conversation.
StartChild ==
  /\ up
  /\ host.present
  /\ ~host.reattached
  /\ journal = "running"
  /\ conv = "none"
  /\ conv' = "running"
  /\ starts' = starts + 1
  /\ UNCHANGED <<journal, host, up, restarts, seen>>

\* The child's conversation ends with its reply (it runs only while a worker runs the durable runtime).
ChildReplies ==
  /\ up
  /\ conv = "running"
  /\ conv' = "done"
  /\ UNCHANGED <<journal, host, up, restarts, starts, seen>>

\* Delegation::end for a child this worker started: the journal commits first; a record already terminal wins.
HostEnds ==
  /\ up
  /\ host.present
  /\ ~host.reattached
  /\ host.outcome = ""
  /\ conv = "done"
  /\ LET j == IF unfinished(journal) THEN "completed" ELSE journal
     IN /\ journal' = j
        /\ host' = [host EXCEPT !.outcome = resultOf(j)]
  /\ UNCHANGED <<conv, up, restarts, starts, seen>>

\* run_restored, then Delegation::end of a reattached task: the journal commits its end, as any child's (before the
\* fix, JOURNAL_REATTACHED = false: the result was kept in memory and the journal kept the record it recovered).
ReattachedEnds ==
  /\ up
  /\ host.reattached
  /\ host.outcome = ""
  /\ conv = "done"
  /\ host' = [host EXCEPT !.outcome = "reply"]
  /\ journal' = IF JOURNAL_REATTACHED THEN "completed" ELSE journal
  /\ UNCHANGED <<conv, up, restarts, starts, seen>>

\* rlm.collect / rlm.results / the child_done event: the parent reads the task's outcome.
ParentReads ==
  /\ up
  /\ host.outcome # ""
  /\ seen' = seen \cup {host.outcome}
  /\ UNCHANGED <<journal, conv, host, up, restarts, starts>>

\* hand_over or a crash: the worker and its host's memory are gone; the journal and the conversation stay.
WorkerDies ==
  /\ up
  /\ restarts < MAX_RESTARTS
  /\ up' = FALSE
  /\ host' = NoTask
  /\ restarts' = restarts + 1
  /\ UNCHANGED <<journal, conv, starts, seen>>

\* The next worker opens the session: subagents::unfinished reads the stored record, Delegation::restore reattaches
\* a child whose conversation the runtime holds while TaskJournal::reopen keeps it running and rewrites every other
\* unfinished record as interrupted (one write), and DurableChildren::restore aborts a running conversation it did
\* not reattach.
WorkerStarts ==
  /\ ~up
  /\ up' = TRUE
  /\ LET wasUnfinished == unfinished(journal)
         reattach == wasUnfinished /\ (conv = "running" \/ conv = "done")
     IN IF REPLAY /\ wasUnfinished
        THEN /\ journal' = "running"
             /\ conv' = "running"
             /\ starts' = starts + 1
             /\ host' = [present |-> TRUE, outcome |-> "", reattached |-> FALSE]
        ELSE LET j == IF ~wasUnfinished THEN journal
                      ELSE IF reattach /\ JOURNAL_REATTACHED THEN "running" ELSE "interrupted"
             IN /\ journal' = j
                /\ conv' = IF conv = "running" /\ ~reattach THEN "aborted" ELSE conv
                /\ starts' = starts
                /\ host' = IF journal = "none" THEN NoTask
                           ELSE IF reattach THEN [present |-> TRUE, outcome |-> "", reattached |-> TRUE]
                           ELSE [present |-> TRUE, outcome |-> resultOf(j), reattached |-> FALSE]
  /\ UNCHANGED <<restarts, seen>>

\* Nothing left to do: the parent has read the result.
Settled ==
  /\ up
  /\ host.outcome # ""
  /\ host.outcome \in seen
  /\ UNCHANGED <<journal, conv, host, up, restarts, starts, seen>>

step ==
  \/ Admit
  \/ MarkRunning
  \/ StartChild
  \/ ChildReplies
  \/ HostEnds
  \/ ReattachedEnds
  \/ ParentReads
  \/ WorkerDies
  \/ WorkerStarts
  \/ Settled

\* The child's work starts at most once: a restart never replays it.
NeverReplayed == starts <= 1
\* The parent is never given two different results for the child.
OneResult == Cardinality(seen) <= 1
\* A child whose conversation is still running when a worker opens the session keeps running.
KeepsRunning == conv # "aborted"
====
