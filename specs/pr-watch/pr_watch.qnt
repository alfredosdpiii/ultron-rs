// ScheduleModule::create, fire_with and cancel; Watch::observe.
// The write lock makes Apply one critical section. Network fetch is outside it, represented by BeginPoll.
// Crash/delivery/journal-write faults remain in schedule-fire (including its known at-least-once finding).
module pr_watch {
  const FILTER: bool
  const RECHECK: bool
  const DEDUP: bool
  const STOP: bool

  var active: bool
  var cancelled: bool
  var registrations: int
  var due: int
  var captured: int
  var observation: int // -1 initial, 0/1 open snapshots, 2 closed/merged/merge-ready, 3 failure
  var failures: int
  var runs: int
  var badQuiet: bool
  var badCancel: bool
  var badTerminal: bool

  action init = all {
    active' = false, cancelled' = false, registrations' = 0, due' = 0, captured' = -1,
    observation' = -1, failures' = 0, runs' = 0, badQuiet' = false, badCancel' = false, badTerminal' = false,
  }

  // Same active repository+PR returns its existing id, never another registration.
  action Start = all {
    not(cancelled), due == 0, registrations < 2,
    registrations' = if (active and DEDUP) registrations else registrations + 1,
    active' = true,
    cancelled' = cancelled, due' = due, captured' = captured, observation' = observation, failures' = failures,
    runs' = runs, badQuiet' = badQuiet, badCancel' = badCancel, badTerminal' = badTerminal,
  }

  // Fetch before acquiring writing, carrying a snapshot of the durable slot.
  action BeginPoll = all {
    active, captured == -1, due < 3,
    captured' = due,
    active' = active, cancelled' = cancelled, registrations' = registrations, due' = due,
    observation' = observation, failures' = failures, runs' = runs,
    badQuiet' = badQuiet, badCancel' = badCancel, badTerminal' = badTerminal,
  }

  // Cancel records its terminal state even while a network fetch is in flight.
  action Cancel = all {
    active,
    active' = false, cancelled' = true,
    registrations' = registrations, due' = due, captured' = captured,
    observation' = observation, failures' = failures, runs' = runs,
    badQuiet' = badQuiet, badCancel' = badCancel, badTerminal' = badTerminal,
  }

  action Apply(result) = all {
    captured >= 0,
    val accept = not(RECHECK) or (active and not(cancelled) and captured == due)
    val n = if (result == 3) failures + 1 else 0
    // watch baseline is silent, failed observation wakes once, third error is terminal.
    val meaningful = if (result == 3) n == 1 or n == 3
                     else observation != -1 and observation != result or result == 2
    val wake = meaningful or not(FILTER)
    val terminal = result == 2 or n == 3
    all {
      active' = if (accept and terminal and STOP) false else active,
      observation' = if (accept) result else observation,
      failures' = if (accept) n else failures,
      due' = if (accept) due + 1 else due,
      runs' = if (accept and wake) runs + 1 else runs,
      badQuiet' = (badQuiet or (accept and wake and not(meaningful))),
      badCancel' = (badCancel or (accept and wake and cancelled)),
      badTerminal' = (badTerminal or (accept and terminal and not(STOP))),
    },
    captured' = -1, cancelled' = cancelled, registrations' = registrations,
  }

  // Restart loses only an in-progress read; durable baseline, error count, due and cancellation survive.
  action Restart = all {
    captured' = -1,
    active' = active, cancelled' = cancelled, registrations' = registrations, due' = due,
    observation' = observation, failures' = failures, runs' = runs,
    badQuiet' = badQuiet, badCancel' = badCancel, badTerminal' = badTerminal,
  }

  action step = any { Start, BeginPoll, Cancel, nondet result = 0.to(3).oneOf() Apply(result), Restart }
  val NoUnchangedWake = not(badQuiet)
  val NoWakeAfterCancel = not(badCancel)
  val OneRegistration = registrations <= 1
  val StopsAtTerminal = not(badTerminal)
}
