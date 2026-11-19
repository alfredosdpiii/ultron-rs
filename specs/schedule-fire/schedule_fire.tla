\* A scheduled prompt (`schedule.every`) fires each slot once: the server's timer keeps one delivery in flight per
\* schedule, and the session's worker runs a fire only for the stored next slot.
\*
\* Code (names a reviewer can open):
\*   crates/daemon/src/prompt_schedules.rs     Scheduler::run (skip a schedule in flight, deliver what is due),
\*                                             Scheduler::delivered (Done / Retry / Gone answers), Scheduler::start
\*                                             (a new server generation reads the journals), Scheduler::update
\*   crates/kernel-apis/src/prompt_schedule.rs ScheduleModule::fire_with (under `writing`: refuse a fire whose due is
\*                                             not the stored next slot, deliver, then put), Schedule::next_after
\*   crates/session-core/src/core/schedule_ops.rs Core::schedule_fire, Core::schedule_start (start or queue the run)
\*
\* This model covers unconditional every/at/loop delivery. PR prefetch and quiet-slot advancement are
\* covered by specs/pr-watch; the lock still covers the due recheck, deliver and put below.
\* One step of the model is one critical section of the code. fire_with is two steps, its check-and-deliver and its
\* put, because the worker can die between them or the put can fail. Time is whole intervals: slot `t` is due at t.
\* Left out: `at` schedules, `until` and `max_runs`, cancellation, the retry delay (a retried slot is simply due
\* again), the worker's own clock being behind the server's, and sessions that are gone.
---- MODULE schedule_fire ----
EXTENDS Integers, FiniteSets

\* The code as written: CHECK_DUE, LOCKED and SKIP_IN_FLIGHT true. Each false is a broken variant.
CONSTANTS
  CHECK_DUE,       \* fire_with refuses a fire whose due is not the stored next slot
  LOCKED,          \* fire_with holds `writing` from its check to its put
  SKIP_IN_FLIGHT,  \* Scheduler::run skips a schedule whose delivery is in flight
  FAULTS,          \* the worker may die, or its put may fail, after the run started and before it was recorded
  MAX_TIME

\* Two server generations: an old server that still fires while a new one reads the journals.
Gens == {1, 2}

VARIABLES
  now,
  nextDue,    \* the stored schedule value's next_due (`ultron.schedule`)
  serverDue,  \* each generation's copy of next_due (0: it holds no schedule yet)
  inFlight,   \* deliveries in flight, as <<generation, due, copy>>: a broken variant can deliver the same due twice
  firing,     \* fire_with calls between their check and their put, by delivery
  started,    \* runs started or queued, by slot
  up          \* the session's worker is alive

vars == <<now, nextDue, serverDue, inFlight, firing, started, up>>

\* Schedule::next_after with an interval of 1: the first slot after now.
NextAfter(due, t) == IF t >= due THEN t + 1 ELSE due + 1
\* Every delivery there can be: <<generation, due, copy>>.
Deliveries == Gens \X (1..MAX_TIME) \X (1..2)
Gen(delivery) == delivery[1]
Due(delivery) == delivery[2]
Mine(g) == {x \in inFlight : Gen(x) = g}

init ==
  /\ now = 0
  /\ nextDue = 1
  /\ serverDue = [g \in Gens |-> IF g = 1 THEN 1 ELSE 0]
  /\ inFlight = {}
  /\ firing = {}
  /\ started = [d \in 1..(MAX_TIME + 1) |-> 0]
  /\ up = TRUE

Tick ==
  /\ now < MAX_TIME
  /\ now' = now + 1
  /\ UNCHANGED <<nextDue, serverDue, inFlight, firing, started, up>>

\* Scheduler::start: generation 2 reads the stored value (an old generation may still be running).
ServerLoads ==
  /\ serverDue[2] = 0
  /\ serverDue' = [serverDue EXCEPT ![2] = nextDue]
  /\ UNCHANGED <<now, nextDue, inFlight, firing, started, up>>

\* Scheduler::run: a due schedule not in flight is delivered (the hub starts the worker when none runs).
Deliver(g) ==
  /\ serverDue[g] > 0
  /\ serverDue[g] <= now
  /\ ~SKIP_IN_FLIGHT \/ Mine(g) = {}
  /\ Cardinality(Mine(g)) < 2
  /\ inFlight' = inFlight \union {<<g, serverDue[g], Cardinality(Mine(g)) + 1>>}
  /\ up' = TRUE
  /\ UNCHANGED <<now, nextDue, serverDue, firing, started>>

\* fire_with refuses the fire (not the stored slot): Delivered gets Done with the stored schedule.
FireRefused(x) ==
  /\ up
  /\ x \in inFlight
  /\ x \notin firing
  /\ ~LOCKED \/ firing = {}
  /\ CHECK_DUE /\ nextDue /= Due(x)
  /\ inFlight' = inFlight \ {x}
  /\ serverDue' = [serverDue EXCEPT ![Gen(x)] = nextDue]
  /\ UNCHANGED <<now, nextDue, firing, started, up>>

\* fire_with's check passes and deliver starts (or queues) the run.
FireStarts(x) ==
  /\ up
  /\ x \in inFlight
  /\ x \notin firing
  /\ ~LOCKED \/ firing = {}
  /\ ~CHECK_DUE \/ nextDue = Due(x)
  /\ Due(x) <= now
  /\ firing' = firing \union {x}
  /\ started' = [started EXCEPT ![Due(x)] = @ + 1]
  /\ UNCHANGED <<now, nextDue, serverDue, inFlight, up>>

\* fire_with's put records the run and the next slot; Delivered gets Done with it.
FireRecorded(x) ==
  /\ x \in firing
  /\ nextDue' = NextAfter(Due(x), now)
  /\ serverDue' = [serverDue EXCEPT ![Gen(x)] = NextAfter(Due(x), now)]
  /\ firing' = firing \ {x}
  /\ inFlight' = inFlight \ {x}
  /\ UNCHANGED <<now, started, up>>

\* fire_with's put fails after the run started: Delivered gets Retry, and the slot stays due.
PutFails(x) ==
  /\ FAULTS
  /\ x \in firing
  /\ firing' = firing \ {x}
  /\ inFlight' = inFlight \ {x}
  /\ UNCHANGED <<now, nextDue, serverDue, started, up>>

\* The worker dies: fires in progress end unrecorded, and their deliveries fail (Retry). A started run is in the
\* durable session and resumes.
WorkerDies ==
  /\ FAULTS
  /\ up
  /\ up' = FALSE
  /\ firing' = {}
  /\ inFlight' = {}
  /\ UNCHANGED <<now, nextDue, serverDue, started>>

\* Time is up and nothing is being fired.
Done ==
  /\ now = MAX_TIME
  /\ firing = {}
  /\ UNCHANGED vars

step ==
  \/ Tick
  \/ ServerLoads
  \/ \E g \in Gens : Deliver(g)
  \/ \E x \in Deliveries : FireRefused(x)
  \/ \E x \in Deliveries : FireStarts(x)
  \/ \E x \in Deliveries : FireRecorded(x)
  \/ \E x \in Deliveries : PutFails(x)
  \/ WorkerDies
  \/ Done

\* Each slot's prompt runs at most once.
OneRunPerSlot == \A d \in DOMAIN started : started[d] <= 1
\* A server generation has at most one delivery of the schedule in flight.
OneDeliveryInFlight == \A g \in Gens : Cardinality(Mine(g)) <= 1
\* The CONSTRAINT: a model that broke OneRunPerSlot need not count further runs.
StateBound == \A d \in DOMAIN started : started[d] <= 2
====
