//! Ported from packages/coding-agent/test/experimental-session-worker-lifecycle.test.ts and
//! experimental-server-lifecycle.test.ts. TypeScript advances fake timers to the millisecond; these run on the real
//! clock. A "not yet" check counts only when it ran before the grace could have elapsed (`before_grace`), so a
//! descheduled test thread skips it instead of failing; a "now" check waits for the retirement (`retires`), so a
//! late timer thread slows the test down instead of failing it.

use daemon::lifecycle::Lifecycle;
use daemon::server::Lifetime;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const GENERATION: &str = "generation-1";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// How often the retirement ran, and a signal each time it does.
struct Retired {
    count: Arc<AtomicUsize>,
    signal: Receiver<()>,
}

impl Retired {
    fn new() -> (Retired, Box<dyn Fn() + Send + Sync>) {
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, signal) = channel();
        let counted = count.clone();
        let retire = Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(());
        });
        (Retired { count, signal }, retire)
    }
}

fn create(initial: u64, orphan: u64) -> (Arc<Lifecycle>, Retired) {
    let (retired, retire) = Retired::new();
    let lifecycle = Lifecycle::new(Some(GENERATION.into()), ms(initial), ms(orphan), retire);
    (lifecycle, retired)
}

fn retired(retired: &Retired) -> usize {
    retired.count.load(Ordering::SeqCst)
}

/// Asserts nothing retired yet, when the read provably happened before `grace` had passed since `armed` (taken
/// before the timer was armed, so the timer cannot fire earlier). Under load the read may come too late; then the
/// check proves nothing and is skipped.
fn before_grace(count: &Retired, armed: Instant, grace: Duration) {
    let seen = retired(count);
    if armed.elapsed() < grace {
        assert_eq!(seen, 0, "retired before the grace elapsed");
    }
}

/// Waits (hang guard 120 s) until the worker retired exactly once.
fn retires(count: &Retired) {
    count
        .signal
        .recv_timeout(Duration::from_secs(120))
        .expect("never retired");
    assert_eq!(retired(count), 1);
}

#[test]
fn daemon_lifecycle_retires_only_after_demand_and_activity_are_gone() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.operation_started("run", "main", "operation-1");
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(retired(&count), 0);
    lifecycle.operation_stopped("run", "main", "operation-1");
    assert_eq!(retired(&count), 1);
    lifecycle.close();
}

#[test]
fn daemon_lifecycle_released_server_demand_is_dropped_while_activity_keeps_the_worker() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.operation_started("run", "ultron.background-job.task-1", "operation-1");
    // Detached work (a shell job, a detached tool call) as the worker reports it (worker.rs `activity`).
    lifecycle.operation_started("activity", "main", "detached");
    assert!(lifecycle.busy());
    lifecycle.release_server(GENERATION);
    lifecycle.operation_stopped("run", "ultron.background-job.task-1", "operation-1");
    assert!(lifecycle.busy());
    assert_eq!(retired(&count), 0);
    lifecycle.operation_stopped("activity", "main", "detached");
    assert!(!lifecycle.busy());
    assert_eq!(retired(&count), 1);
    assert!(lifecycle.retiring());
    lifecycle.close();
}

#[test]
fn daemon_lifecycle_retains_the_worker_until_every_attachment_is_released() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.set_demand(GENERATION, "attachment-2", true).unwrap();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(retired(&count), 0);
    lifecycle.set_demand(GENERATION, "attachment-2", false).unwrap();
    assert_eq!(retired(&count), 1);
}

#[test]
fn daemon_lifecycle_tracks_nested_compaction_independently_from_its_run() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.operation_started("run", "main", "operation-1");
    lifecycle.operation_started("compaction", "main", "operation-1");
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    lifecycle.operation_stopped("compaction", "main", "operation-1");
    assert_eq!(retired(&count), 0);
    lifecycle.operation_stopped("run", "main", "operation-1");
    assert_eq!(retired(&count), 1);
}

#[test]
fn daemon_lifecycle_clears_suspended_compaction_and_navigation() {
    for kind in ["compaction", "navigation"] {
        let (lifecycle, count) = create(100, 200);
        lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
        lifecycle.operation_started(kind, "main", "operation-1");
        lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
        lifecycle.operation_stopped(kind, "main", "operation-1");
        assert_eq!(retired(&count), 1, "{kind}");
    }
}

#[test]
fn daemon_lifecycle_demand_acknowledgement_holds_reconciliation() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let hold = lifecycle.hold_retirement();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(retired(&count), 0);
    drop(hold);
    assert_eq!(retired(&count), 1);
}

#[test]
fn daemon_lifecycle_requests_hold_retirement_only_from_the_active_attachment() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let request = lifecycle.begin_request(GENERATION, "attachment-1").unwrap();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(retired(&count), 0);
    drop(request);
    assert_eq!(retired(&count), 1);
    let error = lifecycle.begin_request(GENERATION, "attachment-1").err().unwrap();
    assert!(error.contains("retiring"), "{error}");
}

#[test]
fn daemon_lifecycle_rejects_requests_from_stale_generations_and_attachments() {
    let (lifecycle, _) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let stale = lifecycle.begin_request("stale", "attachment-1").err().unwrap();
    assert!(stale.contains("stale server generation"), "{stale}");
    let wrong = lifecycle.begin_request(GENERATION, "wrong-attachment").err().unwrap();
    assert!(wrong.contains("active attachment"), "{wrong}");
    lifecycle.close();
}

#[test]
fn daemon_lifecycle_disconnected_generation_demand_lasts_the_orphan_grace() {
    let (lifecycle, count) = create(100, 500);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let armed = Instant::now();
    lifecycle.server_disconnected(GENERATION);
    std::thread::sleep(ms(100));
    before_grace(&count, armed, ms(500));
    retires(&count);
}

#[test]
fn daemon_lifecycle_server_lost_before_demanding_lapses_after_the_orphan_grace() {
    let (lifecycle, count) = create(10_000, 500);
    let armed = Instant::now();
    lifecycle.server_disconnected(GENERATION);
    std::thread::sleep(ms(100));
    before_grace(&count, armed, ms(500));
    retires(&count);
}

#[test]
fn daemon_lifecycle_replacement_generation_retains_the_worker() {
    let (lifecycle, count) = create(100, 200);
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.server_disconnected(GENERATION);
    lifecycle.server_connected("generation-2");
    lifecycle.set_demand("generation-2", "attachment-2", true).unwrap();
    std::thread::sleep(ms(350));
    assert_eq!(retired(&count), 0);
    lifecycle.set_demand("generation-2", "attachment-2", false).unwrap();
    assert_eq!(retired(&count), 1);
}

#[test]
fn daemon_lifecycle_owned_by_launching_server_until_released_or_disconnected() {
    let (lifecycle, _) = create(100, 200);
    assert_eq!(lifecycle.owner().as_deref(), Some(GENERATION));
    lifecycle.operation_started("run", "main", "operation-1");
    lifecycle.release_server(GENERATION);
    assert_eq!(lifecycle.owner(), None);
    lifecycle.server_connected("generation-2");
    assert_eq!(lifecycle.owner().as_deref(), Some("generation-2"));
    lifecycle.server_disconnected("generation-2");
    assert_eq!(lifecycle.owner(), None);
    lifecycle.close();
}

#[test]
fn daemon_lifecycle_retires_a_worker_that_never_receives_demand() {
    let armed = Instant::now();
    let (lifecycle, count) = create(500, 200);
    std::thread::sleep(ms(50));
    before_grace(&count, armed, ms(500));
    retires(&count);
    let error = lifecycle.set_demand(GENERATION, "attachment-1", true).err().unwrap();
    assert!(error.contains("retiring"), "{error}");
}

#[test]
fn daemon_lifecycle_rejects_demand_from_a_stale_generation() {
    let (lifecycle, _) = create(100, 200);
    let error = lifecycle.set_demand("stale", "attachment-1", true).err().unwrap();
    assert!(error.contains("stale server generation"), "{error}");
    lifecycle.close();
}

fn lifetime(keep_alive: bool) -> (Arc<Lifetime>, Retired) {
    let (retired, retire) = Retired::new();
    let lifetime = Lifetime::with_graces(keep_alive, ms(300), ms(100));
    lifetime.start(retire);
    (lifetime, retired)
}

#[test]
fn daemon_lifetime_holds_a_foreground_server_until_shutdown() {
    let (lifetime, count) = lifetime(true);
    std::thread::sleep(ms(600));
    assert_eq!(retired(&count), 0);
    lifetime.stop();
}

#[test]
fn daemon_lifetime_retires_an_automatic_server_without_a_first_client() {
    let armed = Instant::now();
    let (lifetime, count) = lifetime(false);
    std::thread::sleep(ms(250));
    // The startup grace runs first, then the idle grace.
    before_grace(&count, armed, ms(300 + 100));
    retires(&count);
    lifetime.stop();
}

#[test]
fn daemon_lifetime_requires_both_client_and_worker_demand_to_disappear() {
    let (lifetime, count) = lifetime(false);
    lifetime.set_connections(1);
    lifetime.set_workers(1);
    lifetime.set_connections(0);
    std::thread::sleep(ms(600));
    assert_eq!(retired(&count), 0);
    let armed = Instant::now();
    lifetime.set_workers(0);
    std::thread::sleep(ms(50));
    before_grace(&count, armed, ms(100));
    retires(&count);
    lifetime.stop();
}

/// A timer called off and replaced by a newer one leaves the newer one armed when it wakes: `retiring` stays true
/// for the newer grace, and a later idle check does not arm a third timer that would retire the server twice.
#[test]
fn daemon_lifetime_stale_idle_timer_leaves_the_newer_one_armed() {
    let (count, retire) = Retired::new();
    let lifetime = Lifetime::with_graces(false, Duration::from_secs(600), ms(400));
    lifetime.start(retire);
    lifetime.set_connections(1);
    let first = Instant::now();
    lifetime.set_connections(0);
    std::thread::sleep(ms(200));
    // Busy and idle again: the first timer is called off, a second one armed (it fires at about 600 ms).
    lifetime.set_connections(1);
    lifetime.set_connections(0);
    std::thread::sleep(ms(300));
    // The first timer woke (at 400 ms); the second still runs. Checked only when the read is provably in between.
    let retiring = lifetime.retiring();
    let elapsed = first.elapsed();
    if elapsed >= ms(400) && elapsed < ms(600) && retired(&count) == 0 {
        assert!(retiring, "the newer idle timer is still armed");
    }
    // An idle check meanwhile (a setter that changes nothing) must not arm another timer.
    lifetime.set_workers(0);
    retires(&count);
    assert!(
        count.signal.recv_timeout(ms(800)).is_err(),
        "the server retired a second time"
    );
    lifetime.stop();
}

#[test]
fn daemon_lifetime_never_retires_while_a_schedule_is_pending() {
    // A long idle grace: an armed retirement stays visible (`retiring`) instead of firing.
    let (tx, retired) = std::sync::mpsc::channel();
    let lifetime = Lifetime::with_graces(false, Duration::from_secs(600), Duration::from_secs(600));
    lifetime.set_schedules(true);
    lifetime.start(Box::new(move || {
        let _ = tx.send(());
    }));
    // The first client ends the startup grace; with it gone, the server is idle but for the schedule.
    lifetime.set_connections(1);
    lifetime.set_connections(0);
    lifetime.set_workers(1);
    lifetime.set_workers(0);
    assert!(!lifetime.retiring());
    // The schedule is done: the idle grace runs.
    lifetime.set_schedules(false);
    assert!(lifetime.retiring());
    // Pending again before the grace ends: the retirement is called off.
    lifetime.set_schedules(true);
    assert!(!lifetime.retiring());
    lifetime.stop();
    assert!(retired.try_recv().is_err());
    // With no grace, the release retires the server.
    let (tx, retired) = std::sync::mpsc::channel();
    let quick = Lifetime::with_graces(false, Duration::from_secs(600), Duration::ZERO);
    quick.set_schedules(true);
    quick.start(Box::new(move || {
        let _ = tx.send(());
    }));
    quick.set_connections(1);
    quick.set_connections(0);
    assert!(!quick.retiring());
    quick.set_schedules(false);
    retired.recv_timeout(Duration::from_secs(120)).unwrap();
    quick.stop();
}
