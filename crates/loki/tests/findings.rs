//! How the host follows findings from check to check: by file and by text without the line number.

use std::path::Path;

/// Issue #5: `loki: protected file: X` was keyed by the file `loki`, which no check clears, so it stayed
/// "still open in loki" all session. It is followed by the file it names; other `loki:` lines by nothing.
#[test]
fn loki_engine_lines_are_keyed_by_the_file_they_name() {
    let protected = "loki: protected file: .loki/loki.json (set LOKI_ALLOW_PROTECTED=1 to override)";
    assert_eq!(
        loki::finding_key(Path::new("."), protected),
        (".loki/loki.json".to_string(), protected.to_string())
    );
    let other = "loki: unsupported changed file type: a";
    assert_eq!(
        loki::finding_key(Path::new("."), other),
        (String::new(), other.to_string())
    );
    assert_eq!(
        loki::finding_key(Path::new("."), "src/a.py:3: loki/x: y"),
        ("src/a.py".to_string(), "src/a.py: loki/x: y".to_string())
    );
}

/// `git: snapshot content exceeds 64 MiB` was keyed by a file `git` and nagged "still open in git" all session.
#[test]
fn loki_tool_prefixes_are_no_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Makefile"), "").unwrap();
    let git = "git: snapshot content exceeds 64 MiB";
    assert_eq!(loki::finding_key(root.path(), git), (String::new(), git.to_string()));
    assert_eq!(
        loki::finding_key(root.path(), "Makefile:3: tab"),
        ("Makefile".to_string(), "Makefile: tab".to_string())
    );
}

#[test]
fn loki_policy_changes_are_changed_loki_files_only() {
    let report = |blocking: &[&str]| loki::run::Report::new(blocking.iter().map(|s| s.to_string()).collect(), vec![]);
    assert!(report(&[".loki/loki.json: protected content changed"]).blocked_by_policy_changes());
    assert!(!report(&[".loki/loki.json: protected content changed", "a.py:1: r: x"]).blocked_by_policy_changes());
    assert!(!report(&["src/x: protected content changed"]).blocked_by_policy_changes());
    assert!(!report(&[]).blocked_by_policy_changes());
}
