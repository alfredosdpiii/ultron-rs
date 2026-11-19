---- MODULE conversation_observer ----
EXTENDS Integers, FiniteSets

CONSTANTS CHECK_IDENTITY, KEEP_SLOT

VARIABLE s

Requests == {1, 2, 3}

init == s = [
    enabled |-> FALSE, root |-> 0, nextRoot |-> 1, flight |-> 0, aborted |-> FALSE,
    op |-> [r \in Requests |-> 0], phase |-> [r \in Requests |-> "unused"],
    launched |-> 0, settled |-> 0, shown |-> 0, seen |-> {},
    badPublish |-> FALSE, badAccept |-> FALSE, duplicate |-> FALSE ]

Enable == ~s.enabled /\ s' = [s EXCEPT !.enabled = TRUE]

Disable ==
    /\ s.enabled
    /\ s' = [s EXCEPT !.enabled = FALSE, !.aborted = s.flight # 0 \/ s.aborted,
                      !.flight = IF KEEP_SLOT THEN s.flight ELSE 0]

StartRoot ==
    /\ s.root = 0 /\ s.nextRoot <= 2
    /\ s' = [s EXCEPT !.root = s.nextRoot, !.nextRoot = s.nextRoot + 1]

EndRoot ==
    /\ s.root # 0
    /\ s' = [s EXCEPT !.root = 0, !.aborted = s.flight # 0 \/ s.aborted,
                      !.flight = IF KEEP_SLOT THEN s.flight ELSE 0]

Launch ==
    /\ s.enabled /\ s.root # 0 /\ s.flight = 0 /\ s.shown = 0 /\ s.launched < 3
    /\ LET r == s.launched + 1 IN
       s' = [s EXCEPT !.flight = r, !.aborted = FALSE, !.launched = r,
                      !.op = [s.op EXCEPT ![r] = s.root],
                      !.phase = [s.phase EXCEPT ![r] = "working"]]

\* Usage settlement is attempted before the completion is sent; failures are outside the model.
Settle == \E r \in Requests :
    /\ s.phase[r] = "working"
    /\ s' = [s EXCEPT !.phase = [s.phase EXCEPT ![r] = "settled"], !.settled = s.settled + 1]

Send == \E r \in Requests :
    /\ s.phase[r] = "settled"
    /\ s' = [s EXCEPT !.phase = [s.phase EXCEPT ![r] = "queued"]]

Complete == \E r \in Requests : \E topic \in {0, 1, 2} :
    LET matches == s.flight = r
        accept == IF CHECK_IDENTITY THEN matches ELSE s.flight # 0
        valid == matches /\ ~s.aborted /\ s.enabled /\ s.root = s.op[r]
        publish == accept /\ s.enabled /\ ~s.aborted /\ s.root = s.op[r]
                   /\ s.shown = 0 /\ topic # 0 /\ topic \notin s.seen
    IN /\ s.phase[r] = "queued"
       /\ s' = [s EXCEPT !.phase = [s.phase EXCEPT ![r] = "done"],
                         !.flight = IF accept THEN 0 ELSE s.flight,
                         !.shown = IF publish THEN topic ELSE s.shown,
                         !.seen = IF publish THEN s.seen \cup {topic} ELSE s.seen,
                         !.badAccept = s.badAccept \/ (accept /\ ~matches),
                         !.badPublish = s.badPublish \/ (publish /\ ~valid),
                         !.duplicate = s.duplicate \/ (publish /\ topic \in s.seen)]

Dismiss == s.shown # 0 /\ s' = [s EXCEPT !.shown = 0]

Stutter == s' = s

step == Enable \/ Disable \/ StartRoot \/ EndRoot \/ Launch \/ Settle \/ Send \/ Complete \/ Dismiss \/ Stutter

NoRepeatedTopic == ~s.duplicate
IdentitySafe == ~s.badAccept
PublicationSafe == ~s.badPublish
AtMostOneWorker == Cardinality({r \in Requests : s.phase[r] \in {"working", "settled", "queued"}}) <= 1
SettlementBeforeSend == s.settled = Cardinality({r \in Requests : s.phase[r] \in {"settled", "queued", "done"}})
====
