---- MODULE pr_watch ----
\* ScheduleModule::create, fire_with and cancel; Watch::observe.
\* The write lock makes Apply one critical section. Network fetch is outside it, represented by BeginPoll.
\* Crash/delivery/journal-write faults remain in schedule-fire (including its known at-least-once finding).
EXTENDS Integers

CONSTANTS FILTER, RECHECK, DEDUP, STOP

VARIABLES active, cancelled, registrations, due, captured,
          observation, \* -1 initial, 0/1 open snapshots, 2 closed/merged/merge-ready, 3 failure
          failures, runs, badQuiet, badCancel, badTerminal

init ==
  /\ active = FALSE /\ cancelled = FALSE /\ registrations = 0 /\ due = 0 /\ captured = -1
  /\ observation = -1 /\ failures = 0 /\ runs = 0 /\ badQuiet = FALSE /\ badCancel = FALSE /\ badTerminal = FALSE

\* Same active repository+PR returns its existing id, never another registration.
Start ==
  /\ ~cancelled /\ due = 0 /\ registrations < 2
  /\ registrations' = IF active /\ DEDUP THEN registrations ELSE registrations + 1
  /\ active' = TRUE
  /\ UNCHANGED <<cancelled, due, captured, observation, failures, runs, badQuiet, badCancel, badTerminal>>

\* Fetch before acquiring writing, carrying a snapshot of the durable slot.
BeginPoll ==
  /\ active /\ captured = -1 /\ due < 3
  /\ captured' = due
  /\ UNCHANGED <<active, cancelled, registrations, due, observation, failures, runs, badQuiet, badCancel, badTerminal>>

\* Cancel records its terminal state even while a network fetch is in flight.
Cancel ==
  /\ active
  /\ active' = FALSE /\ cancelled' = TRUE
  /\ UNCHANGED <<registrations, due, captured, observation, failures, runs, badQuiet, badCancel, badTerminal>>

Apply(result) ==
  /\ captured >= 0
  /\ LET accept == ~RECHECK \/ (active /\ ~cancelled /\ captured = due)
         n == IF result = 3 THEN failures + 1 ELSE 0
         \* watch baseline is silent, failed observation wakes once, third error is terminal.
         meaningful == IF result = 3 THEN n = 1 \/ n = 3
                       ELSE (observation # -1 /\ observation # result) \/ result = 2
         wake == meaningful \/ ~FILTER
         terminal == result = 2 \/ n = 3
     IN /\ active' = IF accept /\ terminal /\ STOP THEN FALSE ELSE active
        /\ observation' = IF accept THEN result ELSE observation
        /\ failures' = IF accept THEN n ELSE failures
        /\ due' = IF accept THEN due + 1 ELSE due
        /\ runs' = IF accept /\ wake THEN runs + 1 ELSE runs
        /\ badQuiet' = (badQuiet \/ (accept /\ wake /\ ~meaningful))
        /\ badCancel' = (badCancel \/ (accept /\ wake /\ cancelled))
        /\ badTerminal' = (badTerminal \/ (accept /\ terminal /\ ~STOP))
  /\ captured' = -1
  /\ UNCHANGED <<cancelled, registrations>>

\* Restart loses only an in-progress read; durable baseline, error count, due and cancellation survive.
Restart ==
  /\ captured' = -1
  /\ UNCHANGED <<active, cancelled, registrations, due, observation, failures, runs, badQuiet, badCancel, badTerminal>>

step == Start \/ BeginPoll \/ Cancel \/ (\E result \in 0..3 : Apply(result)) \/ Restart

NoUnchangedWake == ~badQuiet
NoWakeAfterCancel == ~badCancel
OneRegistration == registrations <= 1
StopsAtTerminal == ~badTerminal
====
