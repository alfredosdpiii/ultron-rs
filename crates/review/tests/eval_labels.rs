//! Head-pinned, blinded scoring against adjudicated labels (review::eval_labels).

use review::eval_labels::{Labels, blind_ids, packet, parse_record, score, summary_lines};
use review::runner::ProcessRunner;
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_AUTHOR_NAME", "Review Test")
        .env("GIT_AUTHOR_EMAIL", "review@test")
        .env("GIT_COMMITTER_NAME", "Review Test")
        .env("GIT_COMMITTER_EMAIL", "review@test")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
}

/// One head with three findings: a real defect told right, the same defect told half right (posted), and a
/// novel one; only the second was posted.
fn record(head: &str) -> serde_json::Value {
    json!({"sha": head, "posted": [{"claimHash": review::state::claim_hash("total() can skip a row.")}],
        "result": {"complete": true, "files": 1, "added": 3, "removed": 1, "timing": {"totalMs": 1}, "usage": {"costUsd": 0.0, "frames": 1, "tokens": 1},
        "findings": [
            {"file": "app.py", "line": 3, "level": "medium", "claim": "total() skips the last row of rows.",
             "verification": "uncertain", "suggestedFix": "iterate over rows"},
            {"file": "app.py", "line": 3, "level": "high", "claim": "total() can skip a row.",
             "verification": "confirmed", "confidence": 0.9, "replacement": "    for row in rows[1:]:"},
            {"file": "app.py", "line": 1, "level": "low", "claim": "rows may be None.", "verification": "confirmed"}]}})
}

fn labels(head: &str, fix: &str, ids: &[String]) -> Labels {
    serde_json::from_value(json!({"cases": [{"head": head, "record": "r.json",
        "defects": [{"id": "last-row", "trigger": "any non-empty rows", "consequence": "the last row is not summed",
            "fixedBy": fix}],
        "findings": {
            ids[0].clone(): {"verdict": "defect", "defect": "last-row", "trigger": true, "consequence": true, "fix": "correct"},
            ids[1].clone(): {"verdict": "defect", "defect": "last-row", "trigger": true, "consequence": false, "fix": "wrong"},
            ids[2].clone(): {"verdict": "unsupported"}}}]}))
    .expect("labels")
}

#[test]
fn eval_labels_scores_discovery_and_posting_apart_and_judges_fixes_alone() {
    let head = "a".repeat(40);
    let recorded = parse_record(&record(&head)).expect("record");
    let ids = blind_ids(&head, &recorded.result.findings);
    let case = labels(&head, "f", &ids).cases.remove(0);
    let scored = score(&[(case, recorded)], None).expect("scored");
    // Discovery found the defect (the unposted finding told it right); posting only half-told it.
    assert_eq!(scored["discovery"]["defectsCaught"], 1);
    assert_eq!(scored["discovery"]["matched"], 1);
    assert_eq!(scored["discovery"]["partial"], 1);
    assert_eq!(scored["discovery"]["unsupported"], 1);
    assert_eq!(scored["posting"]["findings"], 1);
    assert_eq!(scored["posting"]["defectsCaught"], 0);
    assert_eq!(scored["posting"]["partial"], 1);
    // A wrong fix on a finding does not change its verdict; fixes are counted on their own.
    assert_eq!(scored["fixes"], json!({"correct": 1, "partial": 0, "wrong": 1}));
    let lines = summary_lines(&scored).join("\n");
    assert!(
        lines.contains("Discovery (every finding): 1 of 1 defects caught"),
        "{lines}"
    );
    assert!(
        lines.contains("Posting (posted findings): 0 of 1 defects caught"),
        "{lines}"
    );
}

#[test]
fn eval_labels_packet_hides_everything_but_place_claim_and_fix() {
    let head = "b".repeat(40);
    let recorded = parse_record(&record(&head)).expect("record");
    let shown = packet(&recorded).to_string();
    for hidden in [
        "level",
        "verification",
        "confirmed",
        "confidence",
        "high",
        "posted",
        "0.9",
    ] {
        assert!(!shown.contains(hidden), "the packet shows {hidden}: {shown}");
    }
    assert!(shown.contains("total() skips the last row of rows.") && shown.contains("rows[1:]"));
    // Ids depend on the head, so labels of one head never fit another.
    let other = blind_ids(&"c".repeat(40), &recorded.result.findings);
    assert!(
        blind_ids(&head, &recorded.result.findings)
            .iter()
            .all(|id| !other.contains(id))
    );
}

#[test]
fn eval_labels_refuses_a_wrong_head_a_fix_not_after_it_and_bad_labels() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("app.py"), "x = 1\n").expect("write");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "old"]);
    let old = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("app.py"), "x = 2\n").expect("write");
    git(repo, &["commit", "-qam", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("app.py"), "x = 3\n").expect("write");
    git(repo, &["commit", "-qam", "fix"]);
    let fix = git(repo, &["rev-parse", "HEAD"]);
    let runner = ProcessRunner;
    let repo = repo.display().to_string();
    let pinned = |labels_head: &str, record_head: &str, fixed_by: &str| {
        let recorded = parse_record(&record(record_head)).expect("record");
        let ids = blind_ids(record_head, &recorded.result.findings);
        let case = labels(labels_head, fixed_by, &ids).cases.remove(0);
        score(&[(case, recorded)], Some((&runner, repo.as_str())))
    };
    assert!(pinned(&head, &head, &fix).is_ok());
    let problems = pinned(&head, &head, &old).expect_err("a fix older than the head");
    assert!(
        problems.iter().any(|p| p.contains("does not come after head")),
        "{problems:?}"
    );
    assert!(pinned(&head, &head, &head).is_err(), "the head is no fix of itself");
    let problems = pinned(&head, &fix, &fix).expect_err("a record of another head");
    assert!(
        problems.iter().any(|p| p.contains("the record is of head")),
        "{problems:?}"
    );

    let recorded = parse_record(&record(&head)).expect("record");
    let ids = blind_ids(&head, &recorded.result.findings);
    let mut case = labels(&head, &fix, &ids).cases.remove(0);
    case.findings.get_mut(&ids[2]).expect("label").verdict = "defect".into();
    case.findings.get_mut(&ids[0]).expect("label").fix = Some("great".into());
    let problems = score(&[(case, recorded)], None).expect_err("bad labels");
    assert!(problems.iter().any(|p| p.contains("names a defect")), "{problems:?}");
    assert!(problems.iter().any(|p| p.contains("fix great")), "{problems:?}");
}
