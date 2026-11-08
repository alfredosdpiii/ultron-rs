use config::lock::{Options, Retry, SYNC, Until, acquire};
use std::time::{Duration, Instant, SystemTime};

fn once(stale: Duration, update: Option<Duration>) -> Options {
    Options {
        stale,
        retry: Retry {
            until: Until::Tries(1),
            ..SYNC.retry
        },
        update,
    }
}

#[test]
fn config_lock_a_held_lock_is_refused_with_proper_lockfiles_error_and_freed_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.lock");
    let held = acquire(&path, SYNC).unwrap();
    let error = acquire(&path, once(Duration::from_secs(10), None)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(error.to_string(), "Lock file is already being held");
    drop(held);
    assert!(!path.exists());
    acquire(&path, SYNC).unwrap();
}

#[test]
fn config_lock_a_stale_lock_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.lock");
    std::fs::create_dir(&path).unwrap();
    let old = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::open(&path).unwrap().set_modified(old).unwrap();
    acquire(&path, once(Duration::from_secs(10), None)).unwrap();
}

#[test]
fn config_lock_a_holder_with_update_keeps_its_lock_fresh_until_released() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.lock");
    let held = acquire(&path, once(Duration::from_secs(10), Some(Duration::from_millis(10)))).unwrap();
    let old = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::open(&path).unwrap().set_modified(old).unwrap();
    let modified = || std::fs::metadata(&path).unwrap().modified().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while modified() <= old + Duration::from_secs(30) {
        assert!(Instant::now() < deadline, "the holder never refreshed its lock");
        std::thread::yield_now();
    }
    assert!(acquire(&path, once(Duration::from_secs(10), None)).is_err());
    drop(held);
    assert!(!path.exists());
}
