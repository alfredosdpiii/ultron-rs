//! The ULTRON_LOKI_LOG line (loki.ts `lokiLog`): literal expectations from the TS semantics.

use host::file_hooks::GuardRecord;
use session_core::guard_log::loki_log;

#[test]
fn session_core_loki_log_lines_are_json_with_at_first() {
    let record = GuardRecord {
        guard: "loki".into(),
        phase: "before_write",
        outcome: "blocked",
        ms: 4,
        paths: vec!["a.txt".into()],
        lane: "main".into(),
        detail: Some("[loki] no".into()),
    };
    let quiet = GuardRecord {
        detail: None,
        outcome: "allowed",
        ..record.clone()
    };

    assert!(loki_log(None).is_none());
    assert!(loki_log(Some("  ")).is_none());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs/nested/loki.jsonl");
    let log = loki_log(Some(&format!("  {}  ", path.display()))).unwrap();
    log(&record);
    log(&quiet);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'), "{text:?}");
    // `JSON.stringify({ at, ...record })`: the time first, then the record's fields in order.
    let lines: Vec<(u64, &str)> = text
        .lines()
        .map(|line| {
            let (at, rest) = line.strip_prefix("{\"at\":").unwrap().split_once(',').unwrap();
            (at.parse().unwrap(), rest)
        })
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].0 > 1_600_000_000_000);
    assert_eq!(
        lines[0].1,
        "\"guard\":\"loki\",\"phase\":\"before_write\",\"outcome\":\"blocked\",\"ms\":4,\"paths\":[\"a.txt\"],\"lane\":\"main\",\"detail\":\"[loki] no\"}"
    );
    assert_eq!(
        lines[1].1,
        "\"guard\":\"loki\",\"phase\":\"before_write\",\"outcome\":\"allowed\",\"ms\":4,\"paths\":[\"a.txt\"],\"lane\":\"main\"}"
    );
}
