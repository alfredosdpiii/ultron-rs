// The root run's lifecycle across its three records: the session actor's memory (`Core.run`, the queues), the run
// journal in the session file (`ultron.op.*`: the open operation, its `at`, OP_RESULT, the lane's inbox and pending
// entries) and the durable runtime (the prompt's submission, the run, its answers, the queued messages'
// submissions), with the worker dying (a hand-over or a crash) at any step and the next worker opening the session.
//
// Code (names a reviewer can open):
//   crates/session-journal/src/run/journal.rs  Run::admit_with (two commits: `starting`, then the first checkpoint),
//                                             begin_assistant_with, commit_assistant (an error answer is held in
//                                             memory until the terminal transaction), finish / finish_as
//   crates/session-core/src/core.rs            prompt_with, Core::open (settle when the session has no durable
//                                             records, else reopen_run), run_end (takes `run` before the terminal
//                                             commit; a failed commit reports the run failed), the message_end path
//                                             (a delivered user message is matched to a queued item by its text,
//                                             commit_queued), enqueue
//   crates/session-core/src/core/durable_turns.rs start_turn (submit_run), harness_idle (live.run null and inbox
//                                             empty), generation, reopen_run (submit an unsubmitted prompt, else
//                                             catch_up; then submit_queued: a queued item whose submission is not
//                                             Queued/Placed is submitted again), queued_draft, catch_up
//   crates/session-core/src/recovery.rs        settle (the `at` values it names; every other one ends `failed`), inbox
//
// One step of the model is one critical section (one commit) of the code. Two queued messages with the same text
// (a follow-up, then a steer: "continue" twice). The model's answers end the run or fail it (no tools: a tool round
// is another generation and changes none of the rules here).
// Left out: cancellation (abort, cancel_requested), compaction other than the checkpoint compaction of a legacy
// session, tool calls and cells, retries, overflow, next-run messages, the projection's own write failures, and the
// gap between enqueue's journal write and its durable submit (one step here).
module root_run {
  // The code is all four FALSE.
  const MATCH_BY_ID: bool       // a delivered queued message is matched to its queue item by id (else by text)
  const SETTLE_ALL: bool        // settle ends a run whose last answer committed (`summary.*` after a may-finish
                                // checkpoint too) completed, and a held model error failed with it
  const COMMIT_FIRST: bool      // run_end keeps the run until its terminal transaction commits (a failed commit is
                                // retried at the next idle) and reports the end only then
  const FAULTS: bool            // run_end's terminal commit may fail once
  // A session written by an older build or the TypeScript product: no durable records, a run left open at any `at`.
  const LEGACY: bool
  const MAX_RESTARTS: int
  const NMSG: int

  type R = {
    up: bool, restarts: int,
    // the run journal (session file)
    op: bool, at: str, results: List[str], pending: Set[int], projected: int,
    // the durable runtime (session file)
    hasRecords: bool, submitted: bool, live: bool, need: bool, genOpen: bool, answers: int, lastKind: str,
    msub: int -> str, delivered: int -> int, unproj: List[int],
    // the actor's memory
    running: bool, err: bool, items: List[int], admitting: str, answered: int,
    // bookkeeping
    prompted: bool, enqueued: int, faults: int, reported: List[str], legacyFinished: bool,
  }

  var r: R

  pure val Msgs = 1.to(NMSG)

  // Normal: a live worker, an idle session with or without earlier durable records. LEGACY: a run an older build
  // left open at some `at`, no durable records, no worker yet.
  action init = {
    val base = {
      up: true, restarts: 0, op: false, at: "", results: List(), pending: Set(), projected: 0,
      hasRecords: false, submitted: false, live: false, need: false, genOpen: false, answers: 0, lastKind: "",
      msub: Msgs.mapBy(_ => "none"), delivered: Msgs.mapBy(_ => 0), unproj: List(),
      running: false, err: false, items: List(), admitting: "", answered: 0,
      prompted: false, enqueued: 0, faults: 0, reported: List(), legacyFinished: false,
    }
    if (LEGACY)
      nondet at = Set("starting", "need", "pending", "may_finish", "summary_mf").oneOf()
      r' = { ...base, up: false, op: true, at: at, prompted: true,
             legacyFinished: at == "may_finish" or at == "summary_mf" }
    else
      nondet records = Set(true, false).oneOf()
      r' = { ...base, hasRecords: records }
  }

  // ---- the actor admits and starts the run ----

  // Run::admit_with, first commit: the prompt, the operation, `at: starting`, the lane's current operation.
  action Admit = all {
    r.up, not(r.prompted), not(r.running),
    r' = { ...r, prompted: true, op: true, at: "starting", running: true, admitting: "admitted" },
  }

  // Run::admit_with, second commit: the first checkpoint (need_assistant).
  action Checkpoint = all {
    r.up, r.admitting == "admitted",
    r' = { ...r, at: "need", admitting: "checkpointed" },
  }

  // start_turn: submit_run (the prompt's submission, keyed by its entry) in the durable runtime.
  action SubmitRun = all {
    r.up, r.admitting == "checkpointed",
    r' = { ...r, admitting: "", submitted: true, live: true, need: true, hasRecords: true },
  }

  // enqueue: a message queued while the run runs (pending entry and lane inbox, queue item, durable submission).
  action Enqueue = all {
    r.up, r.running, r.admitting == "", r.enqueued < NMSG,
    val i = r.enqueued + 1
    r' = { ...r, enqueued: i, pending: r.pending.union(Set(i)), items: r.items.append(i), msub: r.msub.set(i, "queued"),
           live: true, need: true },
  }

  // ---- generations ----

  // The runtime starts a generation (it places queued messages into it) and the actor begins the assistant step
  // (`assistant.effect_pending`). Which queued messages a generation takes is left open.
  action GenStart = all {
    r.up, r.running, r.live, r.need, not(r.genOpen), r.admitting == "", r.unproj == List(),
    val queued = Msgs.filter(i => r.msub.get(i) == "queued")
    // The prompt's own generation may take none of them; a later one exists to take at least one.
    nondet take = queued.powerset().filter(t => t != Set() or queued == Set() or r.answers == 0).oneOf()
    r' = { ...r, genOpen: true, need: false, at: "pending",
           msub: Msgs.mapBy(i => if (take.contains(i)) "placed" else r.msub.get(i)),
           delivered: Msgs.mapBy(i => r.delivered.get(i) + (if (take.contains(i)) 1 else 0)),
           unproj: r.unproj.concat(take.fold(List(), (l, i) => l.append(i))) },
  }

  // The actor takes a placed user message (generation / catch_up, then message_end): it matches it to a queue item
  // (by text in the code: the first queued item, as both read the same) and commit_queued commits that item's entry.
  action ProjectMessage = all {
    r.up, r.running, r.unproj != List(),
    val i = r.unproj.head()
    val matched = if (MATCH_BY_ID) (if (r.items.indices().exists(k => r.items[k] == i)) i else 0)
                  else (if (r.items != List()) r.items.head() else 0)
    r' = { ...r, unproj: r.unproj.tail(),
           pending: r.pending.exclude(Set(matched)),
           items: r.items.select(k => k != matched) },
  }

  // The provider answers after the actor took the request's users; the runtime commits the answer (the generation's
  // placed messages are done). Queued input keeps the run going.
  action Answer = all {
    r.up, r.genOpen, r.at == "pending", r.unproj == List(),
    nondet kind = Set("final", "error").oneOf()
    r' = { ...r, genOpen: false, answers: r.answers + 1, lastKind: kind,
           msub: Msgs.mapBy(i => if (r.msub.get(i) == "placed") "done" else r.msub.get(i)),
           need: Msgs.exists(i => r.msub.get(i) == "queued") },
  }

  // The actor projects the committed answer (commit_assistant): a final one to the may-finish checkpoint; an error
  // is held in memory in the code (SETTLE_ALL: committed as the run's failed answer).
  action ProjectAnswer = all {
    r.up, r.running, r.answered < r.answers, r.unproj == List(),
    r' = if (r.lastKind == "final") { ...r, answered: r.answers, projected: r.answers, at: "may_finish", err: false }
         else if (SETTLE_ALL) { ...r, answered: r.answers, projected: r.answers, err: true, at: "failed_answer" }
         else { ...r, answered: r.answers, err: true },
  }

  // The runtime has nothing left for the run (live.run null, inbox empty).
  action RuntimeIdle = all {
    r.up, r.live, not(r.need), not(r.genOpen), r.answers > 0, Msgs.forall(i => r.msub.get(i) != "queued"),
    r' = { ...r, live: false },
  }

  // ---- the run's end ----

  // harness_idle -> run_end: the terminal transaction (OP_RESULT, the operation and the lane's current operation
  // removed) and the run_end event. FAULTS: the commit fails once.
  action RunEnd = all {
    r.up, r.running, not(r.live), r.admitting == "", r.unproj == List(), r.answered == r.answers,
    val status = if (r.err) "failed" else "completed"
    nondet fails = (if (FAULTS and r.faults < 1) Set(true, false) else Set(false)).oneOf()
    if (not(fails))
      r' = { ...r, running: false, err: false, op: false, at: "", results: r.results.append(status),
             reported: r.reported.append(status) }
    else if (COMMIT_FIRST)
      r' = { ...r, faults: r.faults + 1 }
    else
      r' = { ...r, faults: r.faults + 1, running: false, err: false, reported: r.reported.append("failed") },
  }

  // ---- restarts ----

  // hand_over or a crash: the actor's memory is gone; the journal and the runtime's records stay. A generation in
  // flight stays pinned in the runtime.
  action WorkerDies = all {
    r.up, r.restarts < MAX_RESTARTS,
    r' = { ...r, up: false, restarts: r.restarts + 1, running: false, err: false, items: List(), admitting: "",
           answered: 0 },
  }

  // settle's ending for an `at` (SETTLE_ALL: a run whose last answer committed completed, a held error failed).
  pure def settled(at: str): str =
    if (at == "may_finish") "completed"
    else if (SETTLE_ALL and at == "summary_mf") "completed"
    else "failed"

  // Core::open: a run left open is reopened when the session has durable records, else settled (one transaction).
  // The queues come back from the lane's inbox; reopen_run submits an unsubmitted prompt (else catch_up replays
  // what the runtime committed through the actor), then submit_queued submits again every queued item whose
  // submission is not Queued or Placed.
  action WorkerOpens = all {
    not(r.up),
    val items = Msgs.filter(i => r.pending.contains(i)).fold(List(), (l, i) => l.append(i))
    if (r.op and r.hasRecords)
      val again = Msgs.filter(i => r.pending.contains(i) and r.msub.get(i) == "done")
      r' = { ...r, up: true, running: true, items: items,
             answered: r.projected, err: r.at == "failed_answer",
             submitted: true, live: r.live or not(r.submitted) or again != Set(),
             need: r.need or not(r.submitted) or again != Set(),
             msub: Msgs.mapBy(i => if (again.contains(i)) "queued" else r.msub.get(i)) }
    else if (r.op)
      val status = settled(r.at)
      r' = { ...r, up: true, op: false, at: "", items: items, results: r.results.append(status),
             reported: r.reported.append(status) }
    else
      r' = { ...r, up: true, items: items },
  }

  action step = any {
    Admit, Checkpoint, SubmitRun, Enqueue, GenStart, ProjectMessage, Answer, ProjectAnswer, RuntimeIdle, RunEnd,
    WorkerDies, WorkerOpens,
  }

  // The run committed a final answer (it finished), or a legacy run was left after its last answer.
  val Finished = (r.answers > 0 and r.lastKind == "final" and not(r.genOpen) and not(r.need)) or r.legacyFinished
  // The worker is up and nothing is left to do.
  val Quiet = r.up and not(r.running) and r.admitting == "" and not(r.live) and r.unproj == List()

  // TLC's CONSTRAINT: a few answers.
  val StateBound = r.answers <= NMSG + 2

  // The run ends once: one terminal record, one end reported.
  val EndsOnce = r.results.length() <= 1 and r.reported.length() <= 1
  // A run that finished is never reported failed.
  val NotFailedWhenFinished = Finished implies r.reported.indices().forall(k => r.reported[k] != "failed")
  // Recovery never loses a running: once nothing is left to do, no operation is open.
  val NoLostRun = Quiet implies not(r.op)
  // Recovery never invents a running: the runtime runs only a run the journal holds open.
  val NoInventedRun = r.live implies r.op
  // A queued message reaches the model once.
  val DeliveredOnce = Msgs.forall(i => r.delivered.get(i) <= 1)
  // ... and is not lost: at rest every queued message was delivered.
  val NoLostMessage = (Quiet and r.prompted) implies Msgs.forall(i => i > r.enqueued or r.delivered.get(i) >= 1)
}
