// A scheduled prompt (`schedule.every`) fires each slot once: the server's timer keeps one delivery in flight per
// schedule, and the session's worker runs a fire only for the stored next slot.
//
// Code (names a reviewer can open):
//   crates/daemon/src/prompt_schedules.rs     Scheduler::run (skip a schedule in flight, deliver what is due),
//                                             Scheduler::delivered (Done / Retry / Gone answers), Scheduler::start
//                                             (a new server generation reads the journals), Scheduler::update
//   crates/kernel-apis/src/prompt_schedule.rs ScheduleModule::fire_with (under `writing`: refuse a fire whose due is
//                                             not the stored next slot, deliver, then put), Schedule::next_after
//   crates/session-core/src/core/schedule_ops.rs Core::schedule_fire, Core::schedule_start (start or queue the run)
//
// This model covers unconditional every/at/loop delivery. PR prefetch and quiet-slot advancement are
// covered by specs/pr-watch; the lock still covers the due recheck, deliver and put below.
// One step of the model is one critical section of the code. fire_with is two steps, its check-and-deliver and its
// put, because the worker can die between them or the put can fail. Time is whole intervals: slot `t` is due at t.
// Left out: `at` schedules, `until` and `max_runs`, cancellation, the retry delay (a retried slot is simply due
// again), the worker's own clock being behind the server's, and sessions that are gone.
module schedule_fire {
  // The code as written: CHECK_DUE, LOCKED and SKIP_IN_FLIGHT true. Each false is a broken variant.
  const CHECK_DUE: bool       // fire_with refuses a fire whose due is not the stored next slot
  const LOCKED: bool          // fire_with holds `writing` from its check to its put
  const SKIP_IN_FLIGHT: bool  // Scheduler::run skips a schedule whose delivery is in flight
  // The worker may die, or its put may fail, after the run started and before it was recorded.
  const FAULTS: bool
  const MAX_TIME: int

  // Two server generations: an old server that still fires while a new one reads the journals.
  pure val Gens = Set(1, 2)

  var now: int
  // The stored schedule value's next_due (`ultron.schedule`).
  var nextDue: int
  // Each generation's copy of next_due (0: it holds no schedule yet).
  var serverDue: int -> int
  // Deliveries in flight, as (generation, due, copy): a broken variant can deliver the same due twice.
  var inFlight: Set[(int, int, int)]
  // fire_with calls between their check and their put, by delivery.
  var firing: Set[(int, int, int)]
  // Runs started or queued, by slot.
  var started: int -> int
  // The session's worker is alive.
  var up: bool

  // Schedule::next_after with an interval of 1: the first slot after now.
  pure def nextAfter(due: int, t: int): int = if (t >= due) t + 1 else due + 1
  // Every delivery there can be: (generation, due, copy).
  pure val Deliveries = tuples(Gens, 1.to(MAX_TIME), 1.to(2))
  pure def gen(delivery: (int, int, int)): int = delivery._1
  pure def due(delivery: (int, int, int)): int = delivery._2

  action init = all {
    now' = 0,
    nextDue' = 1,
    serverDue' = Gens.mapBy(g => if (g == 1) 1 else 0),
    inFlight' = Set(),
    firing' = Set(),
    started' = 1.to(MAX_TIME + 1).mapBy(_ => 0),
    up' = true,
  }

  action Tick = all {
    now < MAX_TIME,
    now' = now + 1,
    nextDue' = nextDue, serverDue' = serverDue, inFlight' = inFlight, firing' = firing, started' = started, up' = up,
  }

  // Scheduler::start: generation 2 reads the stored value (an old generation may still be running).
  action ServerLoads = all {
    serverDue.get(2) == 0,
    serverDue' = serverDue.set(2, nextDue),
    now' = now, nextDue' = nextDue, inFlight' = inFlight, firing' = firing, started' = started, up' = up,
  }

  // Scheduler::run: a due schedule not in flight is delivered (the hub starts the worker when none runs).
  action Deliver(g) = all {
    val d = serverDue.get(g)
    val mine = inFlight.filter(x => gen(x) == g)
    all {
      d > 0,
      d <= now,
      not(SKIP_IN_FLIGHT) or mine == Set(),
      mine.size() < 2,
      inFlight' = inFlight.union(Set((g, d, mine.size() + 1))),
    },
    up' = true,
    now' = now, nextDue' = nextDue, serverDue' = serverDue, firing' = firing, started' = started,
  }

  // fire_with refuses the fire (not the stored slot): Delivered gets Done with the stored schedule.
  action FireRefused(x) = all {
    up,
    inFlight.contains(x),
    not(firing.contains(x)),
    not(LOCKED) or firing == Set(),
    CHECK_DUE and nextDue != due(x),
    inFlight' = inFlight.exclude(Set(x)),
    serverDue' = serverDue.set(gen(x), nextDue),
    now' = now, nextDue' = nextDue, firing' = firing, started' = started, up' = up,
  }

  // fire_with's check passes and deliver starts (or queues) the run.
  action FireStarts(x) = all {
    up,
    inFlight.contains(x),
    not(firing.contains(x)),
    not(LOCKED) or firing == Set(),
    not(CHECK_DUE) or nextDue == due(x),
    due(x) <= now,
    firing' = firing.union(Set(x)),
    started' = started.set(due(x), started.get(due(x)) + 1),
    now' = now, nextDue' = nextDue, serverDue' = serverDue, inFlight' = inFlight, up' = up,
  }

  // fire_with's put records the run and the next slot; Delivered gets Done with it.
  action FireRecorded(x) = all {
    firing.contains(x),
    val slot = nextAfter(due(x), now)
    all {
      nextDue' = slot,
      serverDue' = serverDue.set(gen(x), slot),
    },
    firing' = firing.exclude(Set(x)),
    inFlight' = inFlight.exclude(Set(x)),
    now' = now, started' = started, up' = up,
  }

  // fire_with's put fails after the run started: Delivered gets Retry, and the slot stays due.
  action PutFails(x) = all {
    FAULTS,
    firing.contains(x),
    firing' = firing.exclude(Set(x)),
    inFlight' = inFlight.exclude(Set(x)),
    now' = now, nextDue' = nextDue, serverDue' = serverDue, started' = started, up' = up,
  }

  // The worker dies: fires in progress end unrecorded, and their deliveries fail (Retry). A started run is in the
  // durable session and resumes.
  action WorkerDies = all {
    FAULTS,
    up,
    up' = false,
    firing' = Set(),
    inFlight' = Set(),
    now' = now, nextDue' = nextDue, serverDue' = serverDue, started' = started,
  }

  // Time is up and nothing is being fired.
  action Done = all {
    now == MAX_TIME,
    firing == Set(),
    now' = now, nextDue' = nextDue, serverDue' = serverDue, inFlight' = inFlight, firing' = firing,
    started' = started, up' = up,
  }

  action step = any {
    Tick,
    ServerLoads,
    nondet g = Gens.oneOf()
    Deliver(g),
    // One `nondet` per action over a constant set, so TLC names each step after its action.
    nondet x = Deliveries.oneOf() FireRefused(x),
    nondet x = Deliveries.oneOf() FireStarts(x),
    nondet x = Deliveries.oneOf() FireRecorded(x),
    nondet x = Deliveries.oneOf() PutFails(x),
    WorkerDies,
    Done,
  }

  // Each slot's prompt runs at most once.
  val OneRunPerSlot = started.keys().forall(d => started.get(d) <= 1)
  // A server generation has at most one delivery of the schedule in flight.
  val OneDeliveryInFlight = Gens.forall(g => inFlight.filter(x => gen(x) == g).size() <= 1)
  // TLC's CONSTRAINT: a model that broke OneRunPerSlot need not count further runs.
  val StateBound = started.keys().forall(d => started.get(d) <= 2)
}

module schedule_fire_code {
  import schedule_fire(CHECK_DUE = true, LOCKED = true, SKIP_IN_FLIGHT = true, FAULTS = false, MAX_TIME = 3).*
}
module schedule_fire_faults {
  import schedule_fire(CHECK_DUE = true, LOCKED = true, SKIP_IN_FLIGHT = true, FAULTS = true, MAX_TIME = 3).*
}
