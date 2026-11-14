module conversation_observer {
  const CHECK_IDENTITY: bool
  const KEEP_SLOT: bool
  type State = {
    enabled: bool, root: int, nextRoot: int, flight: int, aborted: bool,
    op: int -> int, phase: int -> str, launched: int, settled: int,
    shown: int, seen: Set[int], badPublish: bool, badAccept: bool, duplicate: bool, 
  }
  var s: State
  pure val Requests = Set(1, 2, 3)
  action init = s' = {
    enabled: false, root: 0, nextRoot: 1, flight: 0, aborted: false,
    op: Requests.mapBy(_ => 0), phase: Requests.mapBy(_ => "unused"),
    launched: 0, settled: 0, shown: 0, seen: Set(), badPublish: false, badAccept: false, duplicate: false, 
  }
  action Enable = all { not(s.enabled), s' = { ...s, enabled: true } }
  action Disable = all {
    s.enabled,
    s' = { ...s, enabled: false, aborted: s.flight != 0 or s.aborted,
      flight: if (KEEP_SLOT) s.flight else 0 },
  }
  action StartRoot = all {
    s.root == 0, s.nextRoot <= 2,
    s' = { ...s, root: s.nextRoot, nextRoot: s.nextRoot + 1 },
  }
  action EndRoot = all {
    s.root != 0,
    s' = { ...s, root: 0, aborted: s.flight != 0 or s.aborted,
      flight: if (KEEP_SLOT) s.flight else 0 },
  }
  action Launch = all {
    s.enabled, s.root != 0, s.flight == 0, s.shown == 0, s.launched < 3,
    val r = s.launched + 1
    s' = { ...s, flight: r, aborted: false, launched: r,
      op: s.op.set(r, s.root), phase: s.phase.set(r, "working") },
  }
  // Usage settlement is attempted before the completion is sent; failures are outside the model.
  action Settle = {
    nondet r = Requests.oneOf()
    all { s.phase.get(r) == "working",
    s' = { ...s, phase: s.phase.set(r, "settled"), settled: s.settled + 1 } }
  }
  action Send = {
    nondet r = Requests.oneOf()
    all { s.phase.get(r) == "settled",
    s' = { ...s, phase: s.phase.set(r, "queued") } }
  }
  action Complete = {
    nondet r = Requests.oneOf()
    nondet topic = Set(0, 1, 2).oneOf()
    val matches = s.flight == r
    val accept = if (CHECK_IDENTITY) matches else s.flight != 0
    val valid = matches and not(s.aborted) and s.enabled and s.root == s.op.get(r)
    val publish = accept and s.enabled and not(s.aborted) and s.root == s.op.get(r)
      and s.shown == 0 and topic != 0 and not(s.seen.contains(topic))
    all { s.phase.get(r) == "queued",
    s' = { ...s, phase: s.phase.set(r, "done"),
      flight: if (accept) 0 else s.flight,
      shown: if (publish) topic else s.shown,
      seen: if (publish) s.seen.union(Set(topic)) else s.seen,
      badAccept: s.badAccept or (accept and not(matches)),
      badPublish: s.badPublish or (publish and not(valid)),
      duplicate: s.duplicate or (publish and s.seen.contains(topic)),
    } }
  }
  action Dismiss = all { s.shown != 0, s' = { ...s, shown: 0 } }
  action Stutter = s' = s
  action step = any { Enable, Disable, StartRoot, EndRoot, Launch, Settle, Send, Complete, Dismiss, Stutter }
  val NoRepeatedTopic = not(s.duplicate)
  val IdentitySafe = not(s.badAccept)
  val PublicationSafe = not(s.badPublish)
  val AtMostOneWorker = Requests.filter(r => s.phase.get(r) == "working" or s.phase.get(r) == "settled" or s.phase.get(r) == "queued").size() <= 1
  val SettlementBeforeSend = s.settled == Requests.filter(r => s.phase.get(r) == "settled" or s.phase.get(r) == "queued" or s.phase.get(r) == "done").size()
}
