//! A journal whose last line was torn by a killed writer opens with that line cut off in place: the complete lines
//! are kept byte for byte and the file is not republished (a new file renamed over it).

use serde_json::{Value, json};
use session_journal::{SessionRepo, TestClock, Write};
use std::os::unix::fs::MetadataExt;

#[test]
fn session_journal_open_cuts_a_torn_tail_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(dir.path(), TestClock::shared(1_790_000_000_000, 1));
    let mut session = repo.create("/work", None, None).unwrap();
    session
        .commit(&[Write::set("pi.branch.tip", "main", Value::Null)])
        .unwrap();
    session
        .append_message("main", json!({"role": "user", "content": "hi", "timestamp": 1}))
        .unwrap();
    let metadata = session.metadata().clone();
    drop(session);
    let complete = std::fs::read(&metadata.path).unwrap();
    let inode = std::fs::metadata(&metadata.path).unwrap().ino();

    let mut torn = complete.clone();
    torn.extend_from_slice(br#"[{"seq":3,"kind":"entry","#);
    std::fs::write(&metadata.path, &torn).unwrap();
    repo.open(&metadata).unwrap();

    assert_eq!(std::fs::read(&metadata.path).unwrap(), complete);
    assert_eq!(
        std::fs::metadata(&metadata.path).unwrap().ino(),
        inode,
        "the journal was republished"
    );
}
