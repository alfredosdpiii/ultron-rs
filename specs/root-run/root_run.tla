---- MODULE root_run ----
\* The root run's lifecycle across its three records: the session actor's memory (`Core.run`, the queues), the run
\* journal in the session file (`ultron.op.*`: the open operation, its `at`, OP_RESULT, the lane's inbox and pending
\* entries) and the durable runtime (the prompt's submission, the run, its answers, the queued messages'
\* submissions), with the worker dying (a hand-over or a crash) at any step and the next worker opening the session.
\*
\* Code (names a reviewer can open):
\*   crates/session-journal/src/run/journal.rs  Run::admit_with (two commits: `starting`, then the first checkpoint),
\*                                             begin_assistant_with, commit_assistant (an error answer is held in
\*                                             memory until the terminal transaction), finish / finish_as
\*   crates/session-core/src/core.rs            prompt_with, Core::open (settle when the session has no durable
\*                                             records, else reopen_run), run_end (takes `run` before the terminal
\*                                             commit; a failed commit reports the run failed), the message_end path
\*                                             (a delivered user message is matched to a queued item by its text,
\*                                             commit_queued), enqueue
\*   crates/session-core/src/core/durable_turns.rs start_turn (submit_run), harness_idle (live.run null and inbox
\*                                             empty), generation, reopen_run (submit an unsubmitted prompt, else
\*                                             catch_up; then submit_queued: a queued item whose submission is not
\*                                             Queued/Placed is submitted again), queued_draft, catch_up
\*   crates/session-core/src/recovery.rs        settle (the `at` values it names; every other one ends `failed`), inbox
\*
\* One step of the model is one critical section (one commit) of the code. Two queued messages with the same text
\* (a follow-up, then a steer: "continue" twice). The model's answers end the run or fail it (no tools: a tool round
\* is another generation and changes none of the rules here).
\* Left out: cancellation (abort, cancel_requested), compaction other than the checkpoint compaction of a legacy
\* session, tool calls and cells, retries, overflow, next-run messages, the projection's own write failures, and the
\* gap between enqueue's journal write and its durable submit (one step here).
EXTENDS Naturals, Sequences, FiniteSets

\* The code is all four FALSE.
CONSTANTS
  MATCH_BY_ID,   \* a delivered queued message is matched to its queue item by id (else by text)
  SETTLE_ALL,    \* settle ends a run whose last answer committed (`summary.*` after a may-finish
                 \* checkpoint too) completed, and a held model error failed with it
  COMMIT_FIRST,  \* run_end keeps the run until its terminal transaction commits (a failed commit is
                 \* retried at the next idle) and reports the end only then
  FAULTS,        \* run_end's terminal commit may fail once
  \* A session written by an older build or the TypeScript product: no durable records, a run left open at any `at`.
  LEGACY,
  MAX_RESTARTS,
  NMSG

\* r is a record with the fields:
\*   up, restarts,
\*   the run journal (session file): op, at, results (a sequence), pending (a set), projected,
\*   the durable runtime (session file): hasRecords, submitted, live, need, genOpen, answers, lastKind,
\*     msub (Msgs -> string), delivered (Msgs -> Nat), unproj (a sequence),
\*   the actor's memory: running, err, items (a sequence), admitting, answered,
\*   bookkeeping: prompted, enqueued, faults, reported (a sequence), legacyFinished.
VARIABLE r

Msgs == 1..NMSG

\* The members of S (a subset of Msgs) in ascending order.
AsSeq(S) == SelectSeq([i \in Msgs |-> i], LAMBDA i : i \in S)

\* Normal: a live worker, an idle session with or without earlier durable records. LEGACY: a run an older build
\* left open at some `at`, no durable records, no worker yet.
init ==
  LET base == [
        up |-> TRUE, restarts |-> 0, op |-> FALSE, at |-> "", results |-> <<>>, pending |-> {}, projected |-> 0,
        hasRecords |-> FALSE, submitted |-> FALSE, live |-> FALSE, need |-> FALSE, genOpen |-> FALSE, answers |-> 0,
        lastKind |-> "",
        msub |-> [i \in Msgs |-> "none"], delivered |-> [i \in Msgs |-> 0], unproj |-> <<>>,
        running |-> FALSE, err |-> FALSE, items |-> <<>>, admitting |-> "", answered |-> 0,
        prompted |-> FALSE, enqueued |-> 0, faults |-> 0, reported |-> <<>>, legacyFinished |-> FALSE]
  IN IF LEGACY
     THEN \E at \in {"starting", "need", "pending", "may_finish", "summary_mf"} :
            r = [base EXCEPT !.up = FALSE, !.op = TRUE, !.at = at, !.prompted = TRUE,
                             !.legacyFinished = (at = "may_finish" \/ at = "summary_mf")]
     ELSE \E records \in {TRUE, FALSE} :
            r = [base EXCEPT !.hasRecords = records]

\* ---- the actor admits and starts the run ----

\* Run::admit_with, first commit: the prompt, the operation, `at: starting`, the lane's current operation.
Admit ==
  /\ r.up /\ ~r.prompted /\ ~r.running
  /\ r' = [r EXCEPT !.prompted = TRUE, !.op = TRUE, !.at = "starting", !.running = TRUE, !.admitting = "admitted"]

\* Run::admit_with, second commit: the first checkpoint (need_assistant).
Checkpoint ==
  /\ r.up /\ r.admitting = "admitted"
  /\ r' = [r EXCEPT !.at = "need", !.admitting = "checkpointed"]

\* start_turn: submit_run (the prompt's submission, keyed by its entry) in the durable runtime.
SubmitRun ==
  /\ r.up /\ r.admitting = "checkpointed"
  /\ r' = [r EXCEPT !.admitting = "", !.submitted = TRUE, !.live = TRUE, !.need = TRUE, !.hasRecords = TRUE]

\* enqueue: a message queued while the run runs (pending entry and lane inbox, queue item, durable submission).
Enqueue ==
  /\ r.up /\ r.running /\ r.admitting = "" /\ r.enqueued < NMSG
  /\ LET i == r.enqueued + 1
     IN r' = [r EXCEPT !.enqueued = i, !.pending = r.pending \cup {i}, !.items = Append(r.items, i),
                       !.msub = [r.msub EXCEPT ![i] = "queued"], !.live = TRUE, !.need = TRUE]

\* ---- generations ----

\* The runtime starts a generation (it places queued messages into it) and the actor begins the assistant step
\* (`assistant.effect_pending`). Which queued messages a generation takes is left open.
GenStart ==
  /\ r.up /\ r.running /\ r.live /\ r.need /\ ~r.genOpen /\ r.admitting = "" /\ r.unproj = <<>>
  /\ LET queued == {i \in Msgs : r.msub[i] = "queued"}
     IN \* The prompt's own generation may take none of them; a later one exists to take at least one.
        \E take \in {t \in SUBSET queued : t # {} \/ queued = {} \/ r.answers = 0} :
          r' = [r EXCEPT !.genOpen = TRUE, !.need = FALSE, !.at = "pending",
                         !.msub = [i \in Msgs |-> IF i \in take THEN "placed" ELSE r.msub[i]],
                         !.delivered = [i \in Msgs |-> r.delivered[i] + (IF i \in take THEN 1 ELSE 0)],
                         !.unproj = r.unproj \o AsSeq(take)]

\* The actor takes a placed user message (generation / catch_up, then message_end): it matches it to a queue item
\* (by text in the code: the first queued item, as both read the same) and commit_queued commits that item's entry.
ProjectMessage ==
  /\ r.up /\ r.running /\ r.unproj # <<>>
  /\ LET i == Head(r.unproj)
         matched == IF MATCH_BY_ID
                    THEN (IF \E k \in 1..Len(r.items) : r.items[k] = i THEN i ELSE 0)
                    ELSE (IF r.items # <<>> THEN Head(r.items) ELSE 0)
     IN r' = [r EXCEPT !.unproj = Tail(r.unproj),
                       !.pending = r.pending \ {matched},
                       !.items = SelectSeq(r.items, LAMBDA k : k # matched)]

\* The provider answers after the actor took the request's users; the runtime commits the answer (the generation's
\* placed messages are done). Queued input keeps the run going.
Answer ==
  /\ r.up /\ r.genOpen /\ r.at = "pending" /\ r.unproj = <<>>
  /\ \E kind \in {"final", "error"} :
       r' = [r EXCEPT !.genOpen = FALSE, !.answers = r.answers + 1, !.lastKind = kind,
                      !.msub = [i \in Msgs |-> IF r.msub[i] = "placed" THEN "done" ELSE r.msub[i]],
                      !.need = \E i \in Msgs : r.msub[i] = "queued"]

\* The actor projects the committed answer (commit_assistant): a final one to the may-finish checkpoint; an error
\* is held in memory in the code (SETTLE_ALL: committed as the run's failed answer).
ProjectAnswer ==
  /\ r.up /\ r.running /\ r.answered < r.answers /\ r.unproj = <<>>
  /\ r' = IF r.lastKind = "final"
          THEN [r EXCEPT !.answered = r.answers, !.projected = r.answers, !.at = "may_finish", !.err = FALSE]
          ELSE IF SETTLE_ALL
          THEN [r EXCEPT !.answered = r.answers, !.projected = r.answers, !.err = TRUE, !.at = "failed_answer"]
          ELSE [r EXCEPT !.answered = r.answers, !.err = TRUE]

\* The runtime has nothing left for the run (live.run null, inbox empty).
RuntimeIdle ==
  /\ r.up /\ r.live /\ ~r.need /\ ~r.genOpen /\ r.answers > 0 /\ \A i \in Msgs : r.msub[i] # "queued"
  /\ r' = [r EXCEPT !.live = FALSE]

\* ---- the run's end ----

\* harness_idle -> run_end: the terminal transaction (OP_RESULT, the operation and the lane's current operation
\* removed) and the run_end event. FAULTS: the commit fails once.
RunEnd ==
  /\ r.up /\ r.running /\ ~r.live /\ r.admitting = "" /\ r.unproj = <<>> /\ r.answered = r.answers
  /\ LET status == IF r.err THEN "failed" ELSE "completed"
     IN \E fails \in (IF FAULTS /\ r.faults < 1 THEN {TRUE, FALSE} ELSE {FALSE}) :
          IF ~fails
          THEN r' = [r EXCEPT !.running = FALSE, !.err = FALSE, !.op = FALSE, !.at = "",
                              !.results = Append(r.results, status), !.reported = Append(r.reported, status)]
          ELSE IF COMMIT_FIRST
          THEN r' = [r EXCEPT !.faults = r.faults + 1]
          ELSE r' = [r EXCEPT !.faults = r.faults + 1, !.running = FALSE, !.err = FALSE,
                              !.reported = Append(r.reported, "failed")]

\* ---- restarts ----

\* hand_over or a crash: the actor's memory is gone; the journal and the runtime's records stay. A generation in
\* flight stays pinned in the runtime.
WorkerDies ==
  /\ r.up /\ r.restarts < MAX_RESTARTS
  /\ r' = [r EXCEPT !.up = FALSE, !.restarts = r.restarts + 1, !.running = FALSE, !.err = FALSE, !.items = <<>>,
                    !.admitting = "", !.answered = 0]

\* settle's ending for an `at` (SETTLE_ALL: a run whose last answer committed completed, a held error failed).
settled(at) ==
  IF at = "may_finish" THEN "completed"
  ELSE IF SETTLE_ALL /\ at = "summary_mf" THEN "completed"
  ELSE "failed"

\* Core::open: a run left open is reopened when the session has durable records, else settled (one transaction).
\* The queues come back from the lane's inbox; reopen_run submits an unsubmitted prompt (else catch_up replays
\* what the runtime committed through the actor), then submit_queued submits again every queued item whose
\* submission is not Queued or Placed.
WorkerOpens ==
  /\ ~r.up
  /\ LET items == AsSeq({i \in Msgs : i \in r.pending})
     IN IF r.op /\ r.hasRecords
        THEN LET again == {i \in Msgs : i \in r.pending /\ r.msub[i] = "done"}
             IN r' = [r EXCEPT !.up = TRUE, !.running = TRUE, !.items = items,
                               !.answered = r.projected, !.err = (r.at = "failed_answer"),
                               !.submitted = TRUE, !.live = (r.live \/ ~r.submitted \/ again # {}),
                               !.need = (r.need \/ ~r.submitted \/ again # {}),
                               !.msub = [i \in Msgs |-> IF i \in again THEN "queued" ELSE r.msub[i]]]
        ELSE IF r.op
        THEN LET status == settled(r.at)
             IN r' = [r EXCEPT !.up = TRUE, !.op = FALSE, !.at = "", !.items = items,
                               !.results = Append(r.results, status), !.reported = Append(r.reported, status)]
        ELSE r' = [r EXCEPT !.up = TRUE, !.items = items]

step ==
  \/ Admit \/ Checkpoint \/ SubmitRun \/ Enqueue \/ GenStart \/ ProjectMessage \/ Answer \/ ProjectAnswer
  \/ RuntimeIdle \/ RunEnd \/ WorkerDies \/ WorkerOpens

\* The run committed a final answer (it finished), or a legacy run was left after its last answer.
Finished == (r.answers > 0 /\ r.lastKind = "final" /\ ~r.genOpen /\ ~r.need) \/ r.legacyFinished
\* The worker is up and nothing is left to do.
Quiet == r.up /\ ~r.running /\ r.admitting = "" /\ ~r.live /\ r.unproj = <<>>

\* TLC's CONSTRAINT: a few answers.
StateBound == r.answers <= NMSG + 2

\* The run ends once: one terminal record, one end reported.
EndsOnce == Len(r.results) <= 1 /\ Len(r.reported) <= 1
\* A run that finished is never reported failed.
NotFailedWhenFinished == Finished => \A k \in 1..Len(r.reported) : r.reported[k] # "failed"
\* Recovery never loses a running: once nothing is left to do, no operation is open.
NoLostRun == Quiet => ~r.op
\* Recovery never invents a running: the runtime runs only a run the journal holds open.
NoInventedRun == r.live => r.op
\* A queued message reaches the model once.
DeliveredOnce == \A i \in Msgs : r.delivered[i] <= 1
\* ... and is not lost: at rest every queued message was delivered.
NoLostMessage == (Quiet /\ r.prompted) => \A i \in Msgs : i > r.enqueued \/ r.delivered[i] >= 1
====
