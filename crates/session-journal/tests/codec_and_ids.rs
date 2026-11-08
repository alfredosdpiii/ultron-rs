//! The JavaScript semantics the journal shares with session-file through `jsrt`: one process-wide `uuidv7()`
//! sequence, `String(value)` in codec errors, and Node's `mtimeMs`.

use serde_json::json;
use session_file::Env;
use session_journal::codec::parse_transaction;
use session_journal::{Clock, SystemClock};

#[test]
fn session_journal_ids_share_the_session_file_uuidv7_sequence() {
    // Upstream has one module-global `uuidv7()` generator, so a session id and the entry ids written after it sort in
    // creation order even within one millisecond; two generators with their own random sequences did not.
    let mut ids = Vec::new();
    for _ in 0..64 {
        ids.push(session_file::SystemEnv.session_id());
        ids.push(SystemClock.next_id());
    }
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
}

#[test]
fn session_journal_write_kind_errors_print_like_js_string() {
    // D-S1: `String([1, 2])` is "1,2" and `String([1, null])` is "1," (node); the codec printed JSON text ("[1,2]").
    let error = |kind: serde_json::Value| parse_transaction(&json!({"seq": 1, "kind": kind}).to_string()).unwrap_err();
    assert_eq!(error(json!([1, 2])), "Invalid JSONL write kind: 1,2");
    assert_eq!(error(json!([1, null])), "Invalid JSONL write kind: 1,");
    assert_eq!(error(json!(1.5)), "Invalid JSONL write kind: 1.5");
}

#[test]
fn session_journal_mtime_ms_is_negative_before_the_epoch() {
    // D7: Node's `mtimeMs` of a file dated 1.5 s before the epoch is -1500; the old reading had no value there.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.jsonl");
    std::fs::write(&path, "").unwrap();
    let at = std::time::UNIX_EPOCH - std::time::Duration::from_millis(1500);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(at)
        .unwrap();
    assert_eq!(session_journal::io::mtime_ms(&path), Some(-1500.0));
}
