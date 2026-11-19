---- MODULE worker_ownership ----
\* Session worker ownership: the server's attachments and the worker's demand ledger (session_demand attach/detach,
\* its timeout), retirement, and the router's re-attach of a client after its session's worker crashed and restarted.
\*
\* Code (names a reviewer can open):
\*   crates/daemon/src/workers.rs    Manager::attach_client, release_attachment, apply_demand (wait for the answer up to
\*                                   WORKER_DEMAND_TIMEOUT: a timed-out attach is withdrawn by a detach, a timed-out
\*                                   detach "counts as applied")
\*   crates/daemon/src/worker.rs     handle_input / demand (the worker's single reader thread answers demand_applied or
\*                                   demand_rejected), the on_retire callback (worker_retiring, then exit)
\*   crates/daemon/src/lifecycle.rs  Lifecycle::set_demand, reconcile (retire once initialized with no demand, hold or
\*                                   operation), the initial grace timer
\*   crates/daemon/src/routed.rs     Hub::attach (acquire, release the previous attachment, attach_client), detach,
\*                                   invalidate (a worker that ended with a restart pending: its clients go to
\*                                   `reconnecting`), reattach (wait for the restart, check `reconnecting`, then
\*                                   attach under `serialized`)
\*   crates/daemon/src/coordinator.rs route (per-connection order; a message is dropped only when its target is gone)
\*
\* One step of the model is one critical section of the code. One client connection, two sessions A and B, one
\* server generation. The server -> worker channel is FIFO; LOSSY lets one message be dropped while both ends stay up
\* (the coordinator's transport does not do that today: see the report). A demand answer that does not come within the
\* timeout may still be applied later (the worker was slow).
\* Left out: server generations and discovery/adoption (announce, orphan grace, release on shutdown), operations and
\* retirement holds, heartbeats and stuck workers, restart backoff and give-up (the restart always comes back),
\* subscriptions, more than one client.
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS
  \* The code: Hub::attach leaves `reconnecting[conn]` as it is. TRUE: attach removes it.
  ATTACH_CLEARS,
  \* The code: reattach checks `reconnecting` before it takes `serialized`. TRUE: it checks again under it.
  RECHECK,
  \* The code: a timed-out detach counts as applied. TRUE: the server keeps it in a ledger and sends it again until
  \* the worker answers or is gone.
  LEDGER,
  \* One server -> worker message may be lost while both ends stay connected.
  LOSSY,
  \* With ATTACH_CLEARS: the client's attach also removes `reconnecting[conn]` when it returns (under `serialized`),
  \* not only when it starts: a crash of the session it is leaving can mark it reconnecting in between.
  CLEAR_AT_END

Sessions == {"A", "B"}
MAX_ATT == 3
MAX_CLIENT_OPS == 2

\* Msg: [att, attached, req]
\* W: [state, gen, demands, init, timer, chan]
\* P, the router procedure that holds `serialized`: who runs it, where it is, the session it attaches.
\* Op, the manager's apply_demand in flight (one at a time: every caller holds `serialized`).
\*
\* m is a record:
\*   want          the session the client last chose ("" after it detached)
\*   conn          by_conn[conn]: the session it is attached to ("" none)
\*   connAtt
\*   reconnecting  reconnecting[conn]
\*   mutex         `serialized`: "", "client" or "reattach"
\*   proc
\*   rpc           the reattach thread: "idle", "waiting" (restart), "lock" (checked, wants the mutex)
\*   reSession     the session reattach waits for
\*   op
\*   workers       session -> W
\*   restartPending
\*   srvAtt        Worker.attachments on the server
\*   replies       set of <<req, applied>>
\*   owed          LEDGER: detaches not yet answered, set of <<session, att>>
\*   nextAtt, nextReq, clientOps, crashes, losses
VARIABLE m

NoProc == [who |-> "", phase |-> "", target |-> "", prev |-> "", prevAtt |-> 0, att |-> 0]
NoOp == [active |-> FALSE, session |-> "", att |-> 0, attached |-> FALSE, req |-> 0, gen |-> 0, timedOut |-> FALSE]
Gone == [state |-> "none", gen |-> 0, demands |-> {}, init |-> FALSE, timer |-> FALSE, chan |-> <<>>]

init == m = [
    want |-> "", conn |-> "", connAtt |-> 0, reconnecting |-> "", mutex |-> "", proc |-> NoProc, rpc |-> "idle",
    reSession |-> "",
    op |-> NoOp,
    workers |-> [x \in Sessions |-> Gone],
    restartPending |-> [x \in Sessions |-> FALSE],
    srvAtt |-> [x \in Sessions |-> {}],
    replies |-> {}, owed |-> {}, nextAtt |-> 1, nextReq |-> 1, clientOps |-> 0, crashes |-> 0, losses |-> 0]

\* Manager::open_session_with: a new worker for the session (initial grace timer armed, no demand yet).
launch(s, x) ==
  [s EXCEPT !.workers[x] = [Gone EXCEPT !.state = "up", !.gen = s.workers[x].gen + 1, !.timer = TRUE]]

\* apply_demand sends session_demand and waits.
send(s, x, att, attached) ==
  LET w == s.workers[x] IN
  [s EXCEPT
    !.workers[x] = [w EXCEPT !.chan = Append(w.chan, [att |-> att, attached |-> attached, req |-> s.nextReq])],
    !.op = [active |-> TRUE, session |-> x, att |-> att, attached |-> attached, req |-> s.nextReq, gen |-> w.gen,
            timedOut |-> FALSE],
    !.nextReq = s.nextReq + 1]

\* How the op in flight stands: "ok", "rejected", "gone" (the worker ended), "timeout", or "" (still waiting).
\* A worker that ended counts first: Hub::attach finds its session no longer hosted and releases what it got.
outcome(s) ==
  IF s.workers[s.op.session].gen /= s.op.gen \/ s.workers[s.op.session].state = "none" THEN "gone"
  ELSE IF <<s.op.req, TRUE>> \in s.replies THEN "ok"
  ELSE IF <<s.op.req, FALSE>> \in s.replies THEN "rejected"
  ELSE IF s.op.timedOut THEN "timeout"
  ELSE ""

\* A detach that did not get its answer: the code counts it as applied; LEDGER keeps it owed.
settleDetach(s) ==
  IF LEDGER /\ outcome(s) = "timeout" THEN [s EXCEPT !.op = NoOp, !.owed = s.owed \union {<<s.op.session, s.op.att>>}]
  ELSE [s EXCEPT !.op = NoOp]

removeAtt(s, x, att) == [s EXCEPT !.srvAtt[x] = s.srvAtt[x] \ {att}]

\* ---- the client's requests (each runs under `serialized`) ----

ClientAttach ==
  /\ m.mutex = ""
  /\ m.clientOps < MAX_CLIENT_OPS
  /\ \E x \in Sessions:
       m' = [m EXCEPT !.mutex = "client", !.want = x, !.clientOps = m.clientOps + 1,
                      !.reconnecting = IF ATTACH_CLEARS THEN "" ELSE m.reconnecting,
                      !.proc = [NoProc EXCEPT !.who = "client", !.phase = "start", !.target = x]]

\* Hub::detach: reconnecting and by_conn are removed, then the attachment is released.
ClientDetach ==
  /\ m.mutex = ""
  /\ m.clientOps < MAX_CLIENT_OPS
  /\ m.conn /= "" \/ m.reconnecting /= ""
  /\ m' = [m EXCEPT !.mutex = "client", !.want = "", !.clientOps = m.clientOps + 1, !.reconnecting = "", !.conn = "",
                    !.connAtt = 0,
                    !.proc = [NoProc EXCEPT !.who = "client", !.phase = IF m.conn /= "" THEN "release" ELSE "done",
                                            !.target = "", !.prev = m.conn, !.prevAtt = m.connAtt]]

\* ---- the attach procedure (Hub::attach), run by the client or by reattach ----

\* Already attached there: nothing to do. Otherwise acquire the session's worker (launch one if none; wait for a
\* retiring one to be gone), then take by_conn off the previous session.
ProcStarts ==
  /\ m.proc.phase = "start"
  /\ LET x == m.proc.target
         w == m.workers[x]
     IN
     /\ ~m.restartPending[x]
     /\ IF m.conn = x THEN m' = [m EXCEPT !.proc.phase = "done"]
        ELSE IF w.state = "retiring" THEN m' = [m EXCEPT !.proc.phase = "waitGone"]
        ELSE
          LET s1 == IF w.state = "none" THEN launch(m, x) ELSE m IN
          m' = [s1 EXCEPT !.conn = "", !.connAtt = 0,
                          !.proc = [m.proc EXCEPT !.phase = IF m.conn /= "" THEN "release" ELSE "attach",
                                                  !.prev = m.conn, !.prevAtt = m.connAtt]]

\* attach_client got worker_retiring: wait_worker_gone, then acquire (a new worker).
ProcWorkerGone ==
  /\ m.proc.phase = "waitGone"
  /\ m.workers[m.proc.target].state = "none"
  /\ LET s1 == launch(m, m.proc.target) IN
     m' = [s1 EXCEPT !.conn = "", !.connAtt = 0,
                     !.proc = [m.proc EXCEPT !.phase = IF m.conn /= "" THEN "release" ELSE "attach",
                                             !.prev = m.conn, !.prevAtt = m.connAtt]]

\* release_attachment: a detach for an attachment the live worker still has, then the attachment goes.
ProcReleases ==
  /\ m.proc.phase = "release"
  /\ m.op = NoOp
  /\ LET x == m.proc.prev IN
     IF m.workers[x].state /= "none" /\ m.proc.prevAtt \in m.srvAtt[x]
     THEN m' = [send(m, x, m.proc.prevAtt, FALSE) EXCEPT !.proc = [m.proc EXCEPT !.phase = "releaseWait"]]
     ELSE m' = [removeAtt(m, x, m.proc.prevAtt) EXCEPT
                  !.proc = [m.proc EXCEPT !.phase = IF m.proc.target = "" THEN "done" ELSE "attach"]]

ProcReleased ==
  /\ m.proc.phase = "releaseWait"
  /\ outcome(m) /= ""
  /\ LET s1 == removeAtt(settleDetach(m), m.proc.prev, m.proc.prevAtt) IN
     m' = [s1 EXCEPT !.proc = [m.proc EXCEPT !.phase = IF m.proc.target = "" THEN "done" ELSE "attach"]]

\* attach_client: a worker the server knows is retiring refuses at once; otherwise a new attachment and its demand.
ProcAttaches ==
  /\ m.proc.phase = "attach"
  /\ m.op = NoOp
  /\ m.nextAtt <= MAX_ATT
  /\ LET x == m.proc.target
         w == m.workers[x]
     IN
     IF w.state /= "up" THEN m' = [m EXCEPT !.proc.phase = "waitGone"]
     ELSE
       LET att == m.nextAtt
           s1 == [m EXCEPT !.nextAtt = att + 1, !.srvAtt[x] = m.srvAtt[x] \union {att}]
       IN m' = [send(s1, x, att, TRUE) EXCEPT !.proc = [m.proc EXCEPT !.phase = "attachWait", !.att = att]]

\* The attach's answer: applied (by_conn set, the client told), rejected as retiring (wait for it to go), the
\* worker gone (an error), or no answer in time (withdrawn by a detach, an error).
ProcAttached ==
  /\ m.proc.phase = "attachWait"
  /\ LET x == m.proc.target
         o == outcome(m)
     IN
     /\ o /= ""
     /\ IF o = "ok" THEN
          m' = [m EXCEPT !.op = NoOp, !.conn = x, !.connAtt = m.proc.att, !.proc = [m.proc EXCEPT !.phase = "done"]]
        ELSE IF o = "rejected" THEN
          m' = [removeAtt([m EXCEPT !.op = NoOp], x, m.proc.att) EXCEPT
                  !.proc = [m.proc EXCEPT !.phase = "waitGone"]]
        ELSE IF o = "gone" THEN
          m' = [removeAtt([m EXCEPT !.op = NoOp], x, m.proc.att) EXCEPT
                  !.proc = [m.proc EXCEPT !.phase = "done"]]
        ELSE
          m' = [send([m EXCEPT !.op = NoOp], x, m.proc.att, FALSE) EXCEPT
                  !.proc = [m.proc EXCEPT !.phase = "compensateWait"]]

ProcCompensated ==
  /\ m.proc.phase = "compensateWait"
  /\ outcome(m) /= ""
  /\ m' = [removeAtt(settleDetach(m), m.proc.target, m.proc.att) EXCEPT !.proc = [m.proc EXCEPT !.phase = "done"]]

\* The procedure returns; `serialized` is released (reattach then removes reconnecting[conn]).
ProcEnds ==
  /\ m.proc.phase = "done"
  /\ IF m.proc.who = "reattach"
     THEN m' = [m EXCEPT !.proc = NoProc, !.mutex = "", !.rpc = "idle", !.reconnecting = ""]
     ELSE m' = [m EXCEPT !.proc = NoProc, !.mutex = "",
                         !.reconnecting = IF ATTACH_CLEARS /\ CLEAR_AT_END THEN "" ELSE m.reconnecting]

\* ---- the manager's wait ----

\* WORKER_DEMAND_TIMEOUT passes without an answer.
OpTimesOut ==
  /\ m.op.active
  /\ ~m.op.timedOut
  /\ outcome(m) = ""
  /\ m' = [m EXCEPT !.op.timedOut = TRUE]

\* LEDGER: an owed detach is sent again while the worker lives and none is on its way.
LedgerResends ==
  /\ LEDGER
  /\ \E e \in m.owed:
       LET w == m.workers[e[1]] IN
       /\ w.state /= "none"
       /\ ~\E i \in 1..Len(w.chan): w.chan[i].att = e[2]
       /\ m' = [m EXCEPT !.workers[e[1]] =
                  [w EXCEPT !.chan = Append(w.chan, [att |-> e[2], attached |-> FALSE, req |-> 0])]]

\* ---- the worker ----

\* Lifecycle::reconcile: an initialized worker with no demand retires (worker_retiring, then it exits).
reconciled(w) == IF w.state = "up" /\ w.init /\ w.demands = {} THEN [w EXCEPT !.state = "retiring"] ELSE w

\* handle_input -> demand -> Lifecycle::set_demand, answered on the same reader thread.
WorkerHandles ==
  \E x \in Sessions:
    LET w == m.workers[x] IN
    /\ w.state /= "none"
    /\ Len(w.chan) > 0
    /\ LET d == Head(w.chan)
           rest == [w EXCEPT !.chan = Tail(w.chan)]
       IN
       IF w.state = "retiring"
       THEN m' = [m EXCEPT !.workers[x] = rest, !.replies = m.replies \union {<<d.req, FALSE>>}]
       ELSE
         LET demands == IF d.attached THEN w.demands \union {d.att} ELSE w.demands \ {d.att} IN
         m' = [m EXCEPT
                 !.workers[x] = reconciled([rest EXCEPT !.demands = demands, !.init = TRUE, !.timer = FALSE]),
                 !.replies = m.replies \union {<<d.req, TRUE>>},
                 !.owed = IF d.attached THEN m.owed ELSE m.owed \ {<<x, d.att>>}]

\* The initial grace timer: a worker no one demanded is initialized and reconciles.
WorkerTimer ==
  \E x \in Sessions:
    LET w == m.workers[x] IN
    /\ w.state = "up"
    /\ w.timer
    /\ m' = [m EXCEPT !.workers[x] = reconciled([w EXCEPT !.init = TRUE, !.timer = FALSE])]

\* The retiring worker exits; the server forgets it and the router invalidates the session without a restart
\* (clients still on it get `attachment: null`).
WorkerRetires ==
  \E x \in Sessions:
    /\ m.workers[x].state = "retiring"
    /\ m' = [m EXCEPT
              !.workers[x] = [Gone EXCEPT !.gen = m.workers[x].gen],
              !.srvAtt[x] = {},
              !.owed = {e \in m.owed: e[1] /= x},
              !.conn = IF m.conn = x THEN "" ELSE m.conn,
              !.connAtt = IF m.conn = x THEN 0 ELSE m.connAtt]

\* The worker crashes with a restart pending: invalidate moves its clients to `reconnecting` and starts reattach.
WorkerCrashes ==
  /\ m.crashes < 1
  /\ \E x \in Sessions:
       /\ m.workers[x].state = "up"
       /\ LET moved == m.conn = x IN
          m' = [m EXCEPT
                 !.crashes = m.crashes + 1,
                 !.workers[x] = [Gone EXCEPT !.gen = m.workers[x].gen],
                 !.restartPending[x] = TRUE,
                 !.srvAtt[x] = {},
                 !.owed = {e \in m.owed: e[1] /= x},
                 !.conn = IF moved THEN "" ELSE m.conn,
                 !.connAtt = IF moved THEN 0 ELSE m.connAtt,
                 !.reconnecting = IF moved THEN x ELSE m.reconnecting,
                 !.rpc = IF moved THEN "waiting" ELSE m.rpc,
                 !.reSession = IF moved THEN x ELSE m.reSession]

\* schedule_restart's launch: the session gets a new worker.
WorkerRestarts ==
  \E x \in Sessions:
    /\ m.restartPending[x]
    /\ m.workers[x].state = "none"
    /\ m' = [launch(m, x) EXCEPT !.restartPending[x] = FALSE]

\* LOSSY: a message on its way to the worker is dropped.
MessageLost ==
  /\ LOSSY
  /\ m.losses < 1
  /\ \E x \in Sessions:
       LET w == m.workers[x] IN
       /\ Len(w.chan) > 0
       /\ m' = [m EXCEPT !.losses = m.losses + 1, !.workers[x] = [w EXCEPT !.chan = Tail(w.chan)]]

\* ---- reattach (its own thread) ----

\* wait_restart returned (the restarted worker is up): `still` is read outside `serialized`; a connection no longer
\* reconnecting is dropped from the map (`reconnecting.remove`).
ReattachChecks ==
  /\ m.rpc = "waiting"
  /\ ~m.restartPending[m.reSession]
  /\ IF m.reconnecting = m.reSession THEN m' = [m EXCEPT !.rpc = "lock"]
     ELSE m' = [m EXCEPT !.rpc = "idle", !.reconnecting = ""]

\* serialized(|| attach(conn, session)); RECHECK reads `reconnecting` again under the lock first.
ReattachLocks ==
  /\ m.rpc = "lock"
  /\ m.mutex = ""
  /\ IF RECHECK /\ m.reconnecting /= m.reSession THEN m' = [m EXCEPT !.rpc = "idle"]
     ELSE m' = [m EXCEPT !.mutex = "reattach",
                         !.proc = [NoProc EXCEPT !.who = "reattach", !.phase = "start", !.target = m.reSession]]

step ==
  \/ ClientAttach \/ ClientDetach
  \/ ProcStarts \/ ProcWorkerGone \/ ProcReleases \/ ProcReleased \/ ProcAttaches \/ ProcAttached
  \/ ProcCompensated \/ ProcEnds
  \/ OpTimesOut \/ LedgerResends
  \/ WorkerHandles \/ WorkerTimer \/ WorkerRetires \/ WorkerCrashes \/ WorkerRestarts \/ MessageLost
  \/ ReattachChecks \/ ReattachLocks

\* Nothing left to happen: no procedure, no message, no timer, no exit, no restart, nothing owed.
Quiet ==
  /\ m.mutex = "" /\ m.rpc = "idle" /\ ~m.op.active /\ m.owed = {}
  /\ \A x \in Sessions: m.workers[x].chan = <<>> /\ ~m.workers[x].timer
                        /\ m.workers[x].state /= "retiring" /\ ~m.restartPending[x]

\* The client is attached only to the session it chose last (never moved to one it left or never asked for); while
\* its own request runs, by_conn still names the session it is leaving.
StaysWhereItChose == (m.conn /= "" /\ m.mutex /= "client") => m.conn = m.want
\* At rest, a worker holds demand only for attachments the server still has.
NoDemandLeak == Quiet => \A x \in Sessions: m.workers[x].demands \subseteq m.srvAtt[x]
\* At rest, every live worker has a client: one with no attachment has retired.
\* TLC's CONSTRAINT: at most two workers per session and a few demand messages (a client racing a retiring worker
\* can otherwise launch workers forever).
StateBound == m.nextReq <= 7 /\ \A x \in Sessions: m.workers[x].gen <= 2
RetiresWhenIdle == Quiet => \A x \in Sessions: (m.workers[x].state = "up" => m.srvAtt[x] /= {})
====
