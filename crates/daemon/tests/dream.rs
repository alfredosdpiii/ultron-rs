//! Dream mode's schedule (crate::dream; a Rust-only feature, literal expectations): the first start creates the dream
//! session and its daily schedule at `learn.dreamAt` local time, a second start changes nothing, a changed time
//! replaces it, and `learn.dream: false` cancels it.

use daemon::dream::{PROMPT, SCHEDULE_NAME, SESSION_FILE, dream_at, ensure, next_slot, session_id};
use daemon::prompt_schedules::scan_sessions;
use kernel_apis::ManualTickClock;
use kernel_apis::localtime::Zone;
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
