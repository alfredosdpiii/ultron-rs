//! Sessions written before the reserved namespaces and durable kinds were renamed from `pi.*` to `ultron.*` open
//! unchanged on disk and read as `ultron.*`.

use serde_json::{Value, json};
use session_journal::codec::rename_legacy;
use session_journal::{SessionRepo, TestClock, Write};

#[test]
fn session_journal_reads_a_pi_named_session_as_ultron() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(dir.path(), TestClock::shared(1_790_000_000_000, 1));
    let mut session = repo.create("/work", None, None).unwrap();
    session
        .commit(&[Write::set("ultron.session.name", "", json!("old work"))])
        .unwrap();
    let metadata = session.metadata().clone();
    drop(session);
    let old = std::fs::read_to_string(&metadata.path)
        .unwrap()
        .replace("\"ultron.", "\"pi.");
    assert!(old.contains("\"pi.session.name\""));
    std::fs::write(&metadata.path, &old).unwrap();

    let reopened = repo.open(&metadata).unwrap();
    assert_eq!(reopened.value("ultron.session.name", ""), Some(&json!("old work")));
    assert_eq!(reopened.name().as_deref(), Some("old work"));
    assert_eq!(
        std::fs::read_to_string(&metadata.path).unwrap(),
        old,
        "the file was rewritten"
    );
}

#[test]
fn session_journal_renames_durable_entry_and_document_kinds() {
    let mut write: Value = json!({"kind": "list", "namespace": "ultron.module", "key": "durable", "value": {"writes": [
        {"type": "entry", "value": {"kind": "pi.assistant"}},
        {"type": "document.create", "record": {"kind": "pi.live"}},
        {"type": "entry", "value": {"kind": "test.other"}},
    ]}});
    rename_legacy(&mut write);
    let writes = &write["value"]["writes"];
    assert_eq!(writes[0]["value"]["kind"], "ultron.assistant");
    assert_eq!(writes[1]["record"]["kind"], "ultron.live");
    assert_eq!(writes[2]["value"]["kind"], "test.other");
}
