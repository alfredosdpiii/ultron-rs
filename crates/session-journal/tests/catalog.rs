//! The sessions-root layout every reader shares (`session_journal::catalog`).

use session_journal::catalog;
use session_journal::{SessionRepo, TestClock};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn session_journal_session_directory_name_matches_jsonl_repo() {
    // sessionDirectoryName (packages/agent/src/harness/session/jsonl/repo.ts), values from node.
    for (cwd, want) in [
        ("/home/u/p", "--home-u-p--"),
        ("/", "----"),
        ("C:\\Users\\x", "--C--Users-x--"),
        ("/a:b/c", "--a-b-c--"),
        ("rel/x", "--rel-x--"),
        ("//double", "---double--"),
    ] {
        assert_eq!(catalog::session_directory_name(cwd), want, "{cwd}");
    }
}

#[test]
fn session_journal_catalog_lists_v4_and_legacy_v3_sessions_of_a_cwd_newest_first() {
    // One listing for `--continue`, `--session`, `--fork` and `--resume`: a legacy Pi v3 file in the store is a
    // session of its cwd as the repository reads it; files that are not sessions are skipped.
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(dir.path(), TestClock::shared(1_790_000_000_000, 1));
    let v4 = repo.create("/w", Some("v4"), None).unwrap().metadata().path.clone();
    let folder = dir.path().join(catalog::session_directory_name("/w"));
    let v3 = folder.join("legacy.jsonl");
    std::fs::write(
        &v3,
        "{\"type\":\"session\",\"version\":3,\"id\":\"v3\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/w\"}\n",
    )
    .unwrap();
    std::fs::write(folder.join("junk.jsonl"), "not json\n").unwrap();
    std::fs::write(folder.join("notes.txt"), "").unwrap();
    for (path, seconds) in [(&v4, 1_000), (&v3, 2_000)] {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
    }
    let ids = |listed: Vec<session_journal::Metadata>| listed.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(ids(catalog::list(dir.path(), Some("/w"))), ["v3", "v4"]);
    assert!(catalog::list(dir.path(), Some("/elsewhere")).is_empty());
    assert_eq!(
        catalog::find(dir.path(), "v4").unwrap().map(|m| m.path),
        Some(v4.clone())
    );
    assert_eq!(catalog::find(dir.path(), "missing"), Ok(None));
    // Two files with one id (a copied session) make the id ambiguous.
    std::fs::copy(&v4, folder.join("copy.jsonl")).unwrap();
    assert_eq!(
        catalog::find(dir.path(), "v4"),
        Err("Session v4 is ambiguous: more than one session file has this id".to_string())
    );
}

#[test]
fn session_journal_catalog_reads_only_the_header_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, "first\nsecond\n").unwrap();
    assert_eq!(catalog::first_line(&path).unwrap(), "first");
    std::fs::write(&path, "unterminated").unwrap();
    assert_eq!(catalog::first_line(&path).unwrap(), "unterminated");
}
