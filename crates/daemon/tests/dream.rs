//! Dream mode's schedule (crate::dream; a Rust-only feature, literal expectations): the first start creates the dream
//! session and its daily schedule at `learn.dreamAt` local time, a second start changes nothing, a changed time
//! replaces it, and `learn.dream: false` cancels it.

use daemon::dream::{PROMPT, SCHEDULE_NAME, SESSION_FILE, dream_at, ensure, next_slot, session_id};
use daemon::prompt_schedules::scan_sessions;
use jsrt::localtime::Zone;
use kernel_apis::ManualTickClock;
use serde_json::json;

/// 2027-01-15T12:00:00Z.
const NOON: i64 = 1_800_014_400_000;

#[test]
fn daemon_dream_next_slot_is_the_next_local_time() {
    let utc = Zone::utc();
    assert_eq!(next_slot(NOON, (3, 30), &utc), NOON + 15 * 3_600_000 + 30 * 60_000);
    assert_eq!(next_slot(NOON, (13, 0), &utc), NOON + 3_600_000);
    assert_eq!(
        next_slot(NOON, (12, 0), &utc),
        NOON + 24 * 3_600_000,
        "now itself is past"
    );
    // UTC+9 all year: 03:30 local is 18:30 UTC.
    let tokyo = Zone::from_posix("JST-9").unwrap();
    assert_eq!(next_slot(NOON, (3, 30), &tokyo), NOON + 6 * 3_600_000 + 30 * 60_000);
    assert_eq!(dream_at(&json!({})), (3, 30));
    assert_eq!(dream_at(&json!({"learn": {"dreamAt": "23:05"}})), (23, 5));
    assert_eq!(dream_at(&json!({"learn": {"dreamAt": "25:00"}})), (3, 30));
}

#[test]
fn daemon_dream_schedule_is_created_once_re_aimed_and_turned_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (instance, sessions, home) = (
        tmp.path().join("server"),
        tmp.path().join("sessions"),
        tmp.path().join("home"),
    );
    std::fs::create_dir_all(&home).unwrap();
    let clock = ManualTickClock::new(NOON);
    let utc = Zone::utc();
    let run = |settings| ensure(&instance, &sessions, &home, &settings, clock.clone(), &utc).unwrap();

    let first = run(json!({})).expect("on by default");
    assert_eq!(first.name, SCHEDULE_NAME);
    assert_eq!(first.prompt, PROMPT);
    assert_eq!(first.next_due, Some(NOON + 15 * 3_600_000 + 30 * 60_000));
    let id = session_id(&instance, &sessions).unwrap();
    assert_eq!(
        std::fs::read_to_string(instance.join(SESSION_FILE)).unwrap(),
        format!("{id}\n")
    );
    assert_eq!(first.session_id, id);

    let again = run(json!({})).unwrap();
    assert_eq!(again.id, first.id, "a second start keeps it");

    let moved = run(json!({"learn": {"dreamAt": "05:00"}})).unwrap();
    assert_ne!(moved.id, first.id);
    assert_eq!(moved.next_due, Some(NOON + 17 * 3_600_000));
    assert_eq!(session_id(&instance, &sessions).unwrap(), id, "the same dream session");

    assert!(run(json!({"learn": {"dream": false}})).is_none());
    let found = scan_sessions(&sessions);
    assert_eq!(found.len(), 1);
    let active: Vec<_> = found[0].1.iter().filter(|s| s.active()).collect();
    assert!(active.is_empty(), "{active:?}");
    assert_eq!(found[0].1.len(), 2, "both earlier schedules are kept as cancelled");
}

#[test]
fn daemon_dream_off_without_a_session_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (instance, sessions) = (tmp.path().join("server"), tmp.path().join("sessions"));
    let off = json!({"learn": {"dream": false}});
    let clock = ManualTickClock::new(NOON);
    assert!(
        ensure(&instance, &sessions, tmp.path(), &off, clock, &Zone::utc())
            .unwrap()
            .is_none()
    );
    assert!(!instance.join(SESSION_FILE).exists());
}

/// Records what the scheduler would have delivered.
#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<String>>);

impl daemon::prompt_schedules::Deliver for Recorded {
    fn fire(&self, _: &daemon::catalog::Meta, id: &str, _: i64) -> daemon::prompt_schedules::Delivery {
        self.0.lock().unwrap().push(id.to_string());
        daemon::prompt_schedules::Delivery::Done(serde_json::Value::Array(Vec::new()))
    }
}

#[test]
fn daemon_dream_quiet_night_passes_without_a_run_until_a_session_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (instance, sessions, home) = (
        tmp.path().join("server"),
        tmp.path().join("sessions"),
        tmp.path().join("home"),
    );
    std::fs::create_dir_all(&home).unwrap();
    let clock = ManualTickClock::new(NOON);
    let schedule = ensure(&instance, &sessions, &home, &json!({}), clock.clone(), &Zone::utc())
        .unwrap()
        .unwrap();
    let due = schedule.next_due.unwrap();
    clock.set(due + 1);
    let inner = std::sync::Arc::new(Recorded::default());
    let quiet = daemon::dream::QuietDreams {
        inner: inner.clone(),
        instance_dir: instance.clone(),
        session_dir: sessions.clone(),
        clock: clock.clone(),
    };
    let meta = daemon::catalog::list(&sessions).pop().unwrap();
    use daemon::prompt_schedules::{Deliver, Delivery};

    // Never dreamed: it runs.
    quiet.fire(&meta, &schedule.id, due);
    assert_eq!(inner.0.lock().unwrap().len(), 1);

    // Dreamed after every session last changed: the slot passes, the next night is due, no run is counted.
    let mut last = schedule.clone();
    last.last_fired = Some(jsrt::date::now_ms() + 3_600_000);
    daemon::prompt_schedules::in_journal(&meta.path, clock.clone(), |m| m.put(&last)).unwrap();
    let Delivery::Done(active) = quiet.fire(&meta, &schedule.id, due) else {
        panic!("a quiet night is done");
    };
    assert_eq!(inner.0.lock().unwrap().len(), 1, "nothing delivered");
    assert_eq!(active[0]["nextDue"], json!(due + 24 * 3_600_000));
    let after = scan_sessions(&sessions).pop().unwrap().1;
    let kept = after.iter().find(|s| s.id == schedule.id).unwrap();
    assert_eq!((kept.runs, kept.last_fired), (0, last.last_fired));

    // Another session changed since: the next night runs.
    let mut changed = kept.clone();
    changed.last_fired = Some(0);
    daemon::prompt_schedules::in_journal(&meta.path, clock.clone(), |m| m.put(&changed)).unwrap();
    session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared())
        .create(&home.display().to_string(), None, None)
        .unwrap();
    clock.set(due + 24 * 3_600_000 + 1);
    quiet.fire(&meta, &schedule.id, due + 24 * 3_600_000);
    assert_eq!(inner.0.lock().unwrap().len(), 2);
    assert!(daemon::dream::changed_since(&sessions, &meta.id, Some(0)));
    assert!(!daemon::dream::changed_since(&sessions, &meta.id, Some(i64::MAX / 2)));
}
