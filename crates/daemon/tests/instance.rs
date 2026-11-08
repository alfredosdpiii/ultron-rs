//! `daemon::instance`: which recorded server generations count as running.

/// A generation whose process exists but belongs to another user (`kill(pid, 0)` fails with EPERM, as for pid 1
/// under any user but root) is running; it used to be dropped as dead.
#[test]
fn daemon_instance_generation_of_another_users_process_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let record = |pid: u32| {
        serde_json::json!({"pid": pid, "serverId": format!("s{pid}"), "sessionDir": dir.path().join("sessions")})
            .to_string()
    };
    std::fs::write(dir.path().join("generation-a.json"), record(1)).unwrap();
    std::fs::write(dir.path().join("generation-b.json"), record(0)).unwrap();
    let running = daemon::instance::generations(dir.path());
    let pids: Vec<u32> = running.iter().map(|r| r.pid).collect();
    assert_eq!(pids, [1]);
}
