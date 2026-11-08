//! A server connection's route (daemon::routes `Routes`): attached to a session, or waiting to be re-attached to the
//! session whose worker restarts. A client that attaches elsewhere while it waits is not pulled back when the
//! restarted worker comes up.

use daemon::routes::{Route, Routes};

const CONN: u64 = 7;

#[test]
fn daemon_attach_elsewhere_ends_a_wait_for_a_restarting_session() {
    let mut routes: Routes<&str> = Routes::default();
    routes.attach(CONN, "A", "attachment-a");
    // A's worker crashed and is restarting: the connection waits for it.
    assert_eq!(routes.invalidate(CONN, "A", true), Some("attachment-a"));
    assert!(routes.reconnecting(CONN, "A"));

    // Meanwhile the client attaches to B.
    assert_eq!(routes.begin_attach(CONN, "B"), None);
    routes.attach(CONN, "B", "attachment-b");
    // A is back: the connection no longer waits for it, and finishing the re-attach leaves B alone.
    assert!(!routes.reconnecting(CONN, "A"));
    routes.end_reconnect(CONN, "A");
    assert_eq!(routes.attached(CONN, "B"), Some(&"attachment-b"));
}

#[test]
fn daemon_routes_keep_one_entry_per_connection() {
    let mut routes: Routes<&str> = Routes::default();
    routes.attach(CONN, "A", "a1");
    // A switch takes the old attachment out for release.
    assert_eq!(routes.begin_attach(CONN, "B"), Some("a1"));
    routes.attach(CONN, "B", "b1");
    // Another session's worker ending does not touch this connection.
    assert_eq!(routes.invalidate(CONN, "A", true), None);
    assert_eq!(
        routes.get(CONN),
        Some(&Route::Attached {
            session: "B".into(),
            attachment: "b1"
        })
    );
    // B restarts; re-attaching to B itself keeps the wait until the attach succeeds.
    assert_eq!(routes.invalidate(CONN, "B", true), Some("b1"));
    assert_eq!(routes.begin_attach(CONN, "B"), None);
    assert!(routes.reconnecting(CONN, "B"));
    routes.attach(CONN, "B", "b2");
    assert!(!routes.reconnecting(CONN, "B"));
    // Detaching ends any route; a wait has no attachment to release.
    assert_eq!(routes.remove(CONN), Some("b2"));
    assert_eq!(routes.invalidate(CONN, "B", true), None);
    routes.attach(CONN, "C", "c1");
    assert_eq!(routes.invalidate(CONN, "C", true), Some("c1"));
    assert_eq!(routes.remove(CONN), None);
    assert_eq!(routes.get(CONN), None);
}
