// Session worker ownership: the server's attachments and the worker's demand ledger (session_demand attach/detach,
// its timeout), retirement, and the router's re-attach of a client after its session's worker crashed and restarted.
//
// Code (names a reviewer can open):
//   crates/daemon/src/workers.rs    Manager::attach_client, release_attachment, apply_demand (wait for the answer up to
//                                   WORKER_DEMAND_TIMEOUT: a timed-out attach is withdrawn by a detach, a timed-out
//                                   detach "counts as applied")
//   crates/daemon/src/worker.rs     handle_input / demand (the worker's single reader thread answers demand_applied or
//                                   demand_rejected), the on_retire callback (worker_retiring, then exit)
//   crates/daemon/src/lifecycle.rs  Lifecycle::set_demand, reconcile (retire once initialized with no demand, hold or
//                                   operation), the initial grace timer
//   crates/daemon/src/routed.rs     Hub::attach (acquire, release the previous attachment, attach_client), detach,
//                                   invalidate (a worker that ended with a restart pending: its clients go to
//                                   `reconnecting`), reattach (wait for the restart, check `reconnecting`, then
//                                   attach under `serialized`)
//   crates/daemon/src/coordinator.rs route (per-connection order; a message is dropped only when its target is gone)
//
// One step of the model is one critical section of the code. One client connection, two sessions A and B, one
// server generation. The server -> worker channel is FIFO; LOSSY lets one message be dropped while both ends stay up
// (the coordinator's transport does not do that today: see the report). A demand answer that does not come within the
// timeout may still be applied later (the worker was slow).
// Left out: server generations and discovery/adoption (announce, orphan grace, release on shutdown), operations and
// retirement holds, heartbeats and stuck workers, restart backoff and give-up (the restart always comes back),
// subscriptions, more than one client.
module worker_ownership {
  // The code: Hub::attach leaves `reconnecting[conn]` as it is. TRUE: attach removes it.
  const ATTACH_CLEARS: bool
  // The code: reattach checks `reconnecting` before it takes `serialized`. TRUE: it checks again under it.
  const RECHECK: bool
  // The code: a timed-out detach counts as applied. TRUE: the server keeps it in a ledger and sends it again until
  // the worker answers or is gone.
  const LEDGER: bool
  // One server -> worker message may be lost while both ends stay connected.
  const LOSSY: bool
  // With ATTACH_CLEARS: the client's attach also removes `reconnecting[conn]` when it returns (under `serialized`),
  // not only when it starts: a crash of the session it is leaving can mark it reconnecting in between.
  const CLEAR_AT_END: bool

  pure val Sessions = Set("A", "B")
  pure val MAX_ATT = 3
  pure val MAX_CLIENT_OPS = 2

  type Msg = { att: int, attached: bool, req: int }
  type W = { state: str, gen: int, demands: Set[int], init: bool, timer: bool, chan: List[Msg] }
  // The router procedure that holds `serialized`: who runs it, where it is, the session it attaches.
  type P = { who: str, phase: str, target: str, prev: str, prevAtt: int, att: int }
  // The manager's apply_demand in flight (one at a time: every caller holds `serialized`).
  type Op = { active: bool, session: str, att: int, attached: bool, req: int, gen: int, timedOut: bool }

  type M = {
    want: str,                // the session the client last chose ("" after it detached)
    conn: str,                // by_conn[conn]: the session it is attached to ("" none)
    connAtt: int,
    reconnecting: str,        // reconnecting[conn]
    mutex: str,               // `serialized`: "", "client" or "reattach"
    proc: P,
    rpc: str,                 // the reattach thread: "idle", "waiting" (restart), "lock" (checked, wants the mutex)
    reSession: str,           // the session reattach waits for
    op: Op,
    workers: str -> W,
    restartPending: str -> bool,
    srvAtt: str -> Set[int],  // Worker.attachments on the server
    replies: Set[(int, bool)],
    owed: Set[(str, int)],    // LEDGER: detaches not yet answered
    nextAtt: int,
    nextReq: int,
    clientOps: int,
    crashes: int,
    losses: int,
  }

  var m: M

  pure val NoProc: P = { who: "", phase: "", target: "", prev: "", prevAtt: 0, att: 0 }
  pure val NoOp: Op = { active: false, session: "", att: 0, attached: false, req: 0, gen: 0, timedOut: false }
  pure val Gone: W = { state: "none", gen: 0, demands: Set(), init: false, timer: false, chan: List() }

  action init = m' = {
    want: "", conn: "", connAtt: 0, reconnecting: "", mutex: "", proc: NoProc, rpc: "idle", reSession: "",
    op: NoOp,
    workers: Sessions.mapBy(_ => Gone),
    restartPending: Sessions.mapBy(_ => false),
    srvAtt: Sessions.mapBy(_ => Set()),
    replies: Set(), owed: Set(), nextAtt: 1, nextReq: 1, clientOps: 0, crashes: 0, losses: 0,
  }

  // Manager::open_session_with: a new worker for the session (initial grace timer armed, no demand yet).
  pure def launch(s: M, x: str): M =
    { ...s, workers: s.workers.set(x, { ...Gone, state: "up", gen: s.workers.get(x).gen + 1, timer: true }) }

  // apply_demand sends session_demand and waits.
  pure def send(s: M, x: str, att: int, attached: bool): M =
    val w = s.workers.get(x)
    { ...s,
      workers: s.workers.set(x, { ...w, chan: w.chan.append({ att: att, attached: attached, req: s.nextReq }) }),
      op: { active: true, session: x, att: att, attached: attached, req: s.nextReq, gen: w.gen, timedOut: false },
      nextReq: s.nextReq + 1 }

  // How the op in flight stands: "ok", "rejected", "gone" (the worker ended), "timeout", or "" (still waiting).
  // A worker that ended counts first: Hub::attach finds its session no longer hosted and releases what it got.
  pure def outcome(s: M): str =
    if (s.workers.get(s.op.session).gen != s.op.gen or s.workers.get(s.op.session).state == "none") "gone"
    else if (s.replies.contains((s.op.req, true))) "ok"
    else if (s.replies.contains((s.op.req, false))) "rejected"
    else if (s.op.timedOut) "timeout"
    else ""

  // A detach that did not get its answer: the code counts it as applied; LEDGER keeps it owed.
  pure def settleDetach(s: M): M =
    if (LEDGER and outcome(s) == "timeout") { ...s, op: NoOp, owed: s.owed.union(Set((s.op.session, s.op.att))) }
    else { ...s, op: NoOp }

  pure def removeAtt(s: M, x: str, att: int): M = { ...s, srvAtt: s.srvAtt.set(x, s.srvAtt.get(x).exclude(Set(att))) }

  // ---- the client's requests (each runs under `serialized`) ----

  action ClientAttach = all {
    m.mutex == "",
    m.clientOps < MAX_CLIENT_OPS,
    nondet x = Sessions.oneOf()
    m' = { ...m, mutex: "client", want: x, clientOps: m.clientOps + 1,
           reconnecting: if (ATTACH_CLEARS) "" else m.reconnecting,
           proc: { ...NoProc, who: "client", phase: "start", target: x } },
  }

  // Hub::detach: reconnecting and by_conn are removed, then the attachment is released.
  action ClientDetach = all {
    m.mutex == "",
    m.clientOps < MAX_CLIENT_OPS,
    m.conn != "" or m.reconnecting != "",
    m' = { ...m, mutex: "client", want: "", clientOps: m.clientOps + 1, reconnecting: "", conn: "", connAtt: 0,
           proc: { ...NoProc, who: "client", phase: if (m.conn != "") "release" else "done", target: "",
                   prev: m.conn, prevAtt: m.connAtt } },
  }

  // ---- the attach procedure (Hub::attach), run by the client or by reattach ----

  // Already attached there: nothing to do. Otherwise acquire the session's worker (launch one if none; wait for a
  // retiring one to be gone), then take by_conn off the previous session.
  action ProcStarts = all {
    m.proc.phase == "start",
    val x = m.proc.target
    val w = m.workers.get(x)
    all {
      not(m.restartPending.get(x)),
      if (m.conn == x) m' = { ...m, proc: { ...m.proc, phase: "done" } }
      else if (w.state == "retiring") m' = { ...m, proc: { ...m.proc, phase: "waitGone" } }
      else
        val s1 = if (w.state == "none") launch(m, x) else m
        m' = { ...s1, conn: "", connAtt: 0,
               proc: { ...m.proc, phase: if (m.conn != "") "release" else "attach", prev: m.conn, prevAtt: m.connAtt } },
    },
  }

  // attach_client got worker_retiring: wait_worker_gone, then acquire (a new worker).
  action ProcWorkerGone = all {
    m.proc.phase == "waitGone",
    m.workers.get(m.proc.target).state == "none",
    val s1 = launch(m, m.proc.target)
    m' = { ...s1, conn: "", connAtt: 0,
           proc: { ...m.proc, phase: if (m.conn != "") "release" else "attach", prev: m.conn, prevAtt: m.connAtt } },
  }

  // release_attachment: a detach for an attachment the live worker still has, then the attachment goes.
  action ProcReleases = all {
    m.proc.phase == "release",
    m.op == NoOp,
    val x = m.proc.prev
    if (m.workers.get(x).state != "none" and m.srvAtt.get(x).contains(m.proc.prevAtt))
      m' = { ...send(m, x, m.proc.prevAtt, false), proc: { ...m.proc, phase: "releaseWait" } }
    else
      m' = { ...removeAtt(m, x, m.proc.prevAtt),
             proc: { ...m.proc, phase: if (m.proc.target == "") "done" else "attach" } },
  }

  action ProcReleased = all {
    m.proc.phase == "releaseWait",
    outcome(m) != "",
    val s1 = removeAtt(settleDetach(m), m.proc.prev, m.proc.prevAtt)
    m' = { ...s1, proc: { ...m.proc, phase: if (m.proc.target == "") "done" else "attach" } },
  }

  // attach_client: a worker the server knows is retiring refuses at once; otherwise a new attachment and its demand.
  action ProcAttaches = all {
    m.proc.phase == "attach",
    m.op == NoOp,
    m.nextAtt <= MAX_ATT,
    val x = m.proc.target
    val w = m.workers.get(x)
    if (w.state != "up") m' = { ...m, proc: { ...m.proc, phase: "waitGone" } }
    else
      val att = m.nextAtt
      val s1 = { ...m, nextAtt: att + 1, srvAtt: m.srvAtt.set(x, m.srvAtt.get(x).union(Set(att))) }
      m' = { ...send(s1, x, att, true), proc: { ...m.proc, phase: "attachWait", att: att } },
  }

  // The attach's answer: applied (by_conn set, the client told), rejected as retiring (wait for it to go), the
  // worker gone (an error), or no answer in time (withdrawn by a detach, an error).
  action ProcAttached = all {
    m.proc.phase == "attachWait",
    val x = m.proc.target
    val o = outcome(m)
    all {
      o != "",
      if (o == "ok")
        m' = { ...m, op: NoOp, conn: x, connAtt: m.proc.att, proc: { ...m.proc, phase: "done" } }
      else if (o == "rejected")
        m' = { ...removeAtt({ ...m, op: NoOp }, x, m.proc.att), proc: { ...m.proc, phase: "waitGone" } }
      else if (o == "gone")
        m' = { ...removeAtt({ ...m, op: NoOp }, x, m.proc.att), proc: { ...m.proc, phase: "done" } }
      else
        m' = { ...send({ ...m, op: NoOp }, x, m.proc.att, false), proc: { ...m.proc, phase: "compensateWait" } },
    },
  }

  action ProcCompensated = all {
    m.proc.phase == "compensateWait",
    outcome(m) != "",
    m' = { ...removeAtt(settleDetach(m), m.proc.target, m.proc.att), proc: { ...m.proc, phase: "done" } },
  }

  // The procedure returns; `serialized` is released (reattach then removes reconnecting[conn]).
  action ProcEnds = all {
    m.proc.phase == "done",
    if (m.proc.who == "reattach")
      m' = { ...m, proc: NoProc, mutex: "", rpc: "idle", reconnecting: "" }
    else
      m' = { ...m, proc: NoProc, mutex: "", reconnecting: if (ATTACH_CLEARS and CLEAR_AT_END) "" else m.reconnecting },
  }

  // ---- the manager's wait ----

  // WORKER_DEMAND_TIMEOUT passes without an answer.
  action OpTimesOut = all {
    m.op.active,
    not(m.op.timedOut),
    outcome(m) == "",
    m' = { ...m, op: { ...m.op, timedOut: true } },
  }

  // LEDGER: an owed detach is sent again while the worker lives and none is on its way.
  action LedgerResends = all {
    LEDGER,
    nondet e = m.owed.oneOf()
    val w = m.workers.get(e._1)
    all {
      w.state != "none",
      w.chan.select(x => x.att == e._2).length() == 0,
      m' = { ...m, workers: m.workers.set(e._1, { ...w, chan: w.chan.append({ att: e._2, attached: false, req: 0 }) }) },
    },
  }

  // ---- the worker ----

  // Lifecycle::reconcile: an initialized worker with no demand retires (worker_retiring, then it exits).
  pure def reconciled(w: W): W = if (w.state == "up" and w.init and w.demands == Set()) { ...w, state: "retiring" } else w

  // handle_input -> demand -> Lifecycle::set_demand, answered on the same reader thread.
  action WorkerHandles = all {
    nondet x = Sessions.oneOf()
    val w = m.workers.get(x)
    all {
      w.state != "none",
      w.chan.length() > 0,
      val d = w.chan.head()
      val rest = { ...w, chan: w.chan.tail() }
      if (w.state == "retiring")
        m' = { ...m, workers: m.workers.set(x, rest), replies: m.replies.union(Set((d.req, false))) }
      else
        val demands = if (d.attached) w.demands.union(Set(d.att)) else w.demands.exclude(Set(d.att))
        m' = { ...m,
               workers: m.workers.set(x, reconciled({ ...rest, demands: demands, init: true, timer: false })),
               replies: m.replies.union(Set((d.req, true))),
               owed: if (d.attached) m.owed else m.owed.exclude(Set((x, d.att))) },
    },
  }

  // The initial grace timer: a worker no one demanded is initialized and reconciles.
  action WorkerTimer = all {
    nondet x = Sessions.oneOf()
    val w = m.workers.get(x)
    all {
      w.state == "up",
      w.timer,
      m' = { ...m, workers: m.workers.set(x, reconciled({ ...w, init: true, timer: false })) },
    },
  }

  // The retiring worker exits; the server forgets it and the router invalidates the session without a restart
  // (clients still on it get `attachment: null`).
  action WorkerRetires = all {
    nondet x = Sessions.oneOf()
    all {
      m.workers.get(x).state == "retiring",
      m' = { ...m,
             workers: m.workers.set(x, { ...Gone, gen: m.workers.get(x).gen }),
             srvAtt: m.srvAtt.set(x, Set()),
             owed: m.owed.filter(e => e._1 != x),
             conn: if (m.conn == x) "" else m.conn,
             connAtt: if (m.conn == x) 0 else m.connAtt },
    },
  }

  // The worker crashes with a restart pending: invalidate moves its clients to `reconnecting` and starts reattach.
  action WorkerCrashes = all {
    m.crashes < 1,
    nondet x = Sessions.oneOf()
    all {
      m.workers.get(x).state == "up",
      val moved = m.conn == x
      m' = { ...m,
             crashes: m.crashes + 1,
             workers: m.workers.set(x, { ...Gone, gen: m.workers.get(x).gen }),
             restartPending: m.restartPending.set(x, true),
             srvAtt: m.srvAtt.set(x, Set()),
             owed: m.owed.filter(e => e._1 != x),
             conn: if (moved) "" else m.conn,
             connAtt: if (moved) 0 else m.connAtt,
             reconnecting: if (moved) x else m.reconnecting,
             rpc: if (moved) "waiting" else m.rpc,
             reSession: if (moved) x else m.reSession },
    },
  }

  // schedule_restart's launch: the session gets a new worker.
  action WorkerRestarts = all {
    nondet x = Sessions.oneOf()
    all {
      m.restartPending.get(x),
      m.workers.get(x).state == "none",
      m' = { ...launch(m, x), restartPending: m.restartPending.set(x, false) },
    },
  }

  // LOSSY: a message on its way to the worker is dropped.
  action MessageLost = all {
    LOSSY,
    m.losses < 1,
    nondet x = Sessions.oneOf()
    val w = m.workers.get(x)
    all {
      w.chan.length() > 0,
      m' = { ...m, losses: m.losses + 1, workers: m.workers.set(x, { ...w, chan: w.chan.tail() }) },
    },
  }

  // ---- reattach (its own thread) ----

  // wait_restart returned (the restarted worker is up): `still` is read outside `serialized`; a connection no longer
  // reconnecting is dropped from the map (`reconnecting.remove`).
  action ReattachChecks = all {
    m.rpc == "waiting",
    not(m.restartPending.get(m.reSession)),
    if (m.reconnecting == m.reSession) m' = { ...m, rpc: "lock" }
    else m' = { ...m, rpc: "idle", reconnecting: "" },
  }

  // serialized(|| attach(conn, session)); RECHECK reads `reconnecting` again under the lock first.
  action ReattachLocks = all {
    m.rpc == "lock",
    m.mutex == "",
    if (RECHECK and m.reconnecting != m.reSession) m' = { ...m, rpc: "idle" }
    else m' = { ...m, mutex: "reattach", proc: { ...NoProc, who: "reattach", phase: "start", target: m.reSession } },
  }

  action step = any {
    ClientAttach, ClientDetach,
    ProcStarts, ProcWorkerGone, ProcReleases, ProcReleased, ProcAttaches, ProcAttached, ProcCompensated, ProcEnds,
    OpTimesOut, LedgerResends,
    WorkerHandles, WorkerTimer, WorkerRetires, WorkerCrashes, WorkerRestarts, MessageLost,
    ReattachChecks, ReattachLocks,
  }

  // Nothing left to happen: no procedure, no message, no timer, no exit, no restart, nothing owed.
  val Quiet = m.mutex == "" and m.rpc == "idle" and not(m.op.active) and m.owed == Set()
    and Sessions.forall(x => m.workers.get(x).chan == List() and not(m.workers.get(x).timer)
      and m.workers.get(x).state != "retiring" and not(m.restartPending.get(x)))

  // The client is attached only to the session it chose last (never moved to one it left or never asked for); while
  // its own request runs, by_conn still names the session it is leaving.
  val StaysWhereItChose = (m.conn != "" and m.mutex != "client") implies m.conn == m.want
  // At rest, a worker holds demand only for attachments the server still has.
  val NoDemandLeak = Quiet implies Sessions.forall(x => m.workers.get(x).demands.subseteq(m.srvAtt.get(x)))
  // At rest, every live worker has a client: one with no attachment has retired.
  // TLC's CONSTRAINT: at most two workers per session and a few demand messages (a client racing a retiring worker
  // can otherwise launch workers forever).
  val StateBound = m.nextReq <= 7 and Sessions.forall(x => m.workers.get(x).gen <= 2)
  val RetiresWhenIdle = Quiet implies Sessions.forall(x => m.workers.get(x).state == "up" implies m.srvAtt.get(x) != Set())
}
