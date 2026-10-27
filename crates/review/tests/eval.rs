//! `ultron autoreview eval`'s cases and scores (review::eval): reverted commits, fix-inducing commits found as SZZ
//! finds them, controls, over a real git history.

use review::eval::{BugLine, Case, CaseKind, EvalOptions, build_cases, score, summary, summary_lines};
use review::plan::EngineResult;
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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
}

fn commit(repo: &Path, file: &str, text: &str, message: &str) -> String {
    std::fs::write(repo.join(file), text).expect("write");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", message]);
    git(repo, &["rev-parse", "HEAD"])
}

const APP: &str = "def total(rows):\n    result = 0\n    for row in rows:\n        result += row\n    return result\n";

/// init, add parse, speed up totals (the bug), fix: totals (the fix), add logging, risky change, its revert, docs.
struct History {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    shas: Vec<String>,
}

fn history() -> History {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().to_path_buf();
    git(&repo, &["init", "-q", "-b", "main"]);
    let mut shas = vec![commit(&repo, "app.py", APP, "init")];
    shas.push(commit(
        &repo,
        "parse.py",
        "def parse(text):\n    return text.split(',')\n",
        "add parse",
    ));
    let buggy = APP.replace("    for row in rows:\n", "    for row in rows[:-1]:\n");
    shas.push(commit(&repo, "app.py", &buggy, "speed up totals"));
    shas.push(commit(&repo, "app.py", APP, "fix: totals skip the last row"));
    shas.push(commit(
        &repo,
        "log.py",
        "import logging\nlog = logging.getLogger('app')\n",
        "add logging",
    ));
    shas.push(commit(
        &repo,
        "parse.py",
        "def parse(text):\n    return text.split(';')\n",
        "risky change",
    ));
    git(&repo, &["revert", "--no-edit", "HEAD"]);
    shas.push(git(&repo, &["rev-parse", "HEAD"]));
    shas.push(commit(&repo, "README.md", "# app\n\nSums rows.\n", "docs"));
    History { _dir: dir, repo, shas }
}

#[test]
fn review_eval_cases_are_reverted_fix_inducing_and_controls() {
    let h = history();
    let s = &h.shas;
    let cases = build_cases(&ProcessRunner, &h.repo.display().to_string(), &EvalOptions::default()).unwrap();
    let got: Vec<(CaseKind, &str, &str, Vec<&str>)> = cases
        .iter()
        .map(|case| {
            (
                case.kind,
                case.sha.as_str(),
                case.subject.as_str(),
                case.by.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (CaseKind::Reverted, s[5].as_str(), "risky change", vec![s[6].as_str()]),
            (
                CaseKind::FixInducing,
                s[2].as_str(),
                "speed up totals",
                vec![s[3].as_str()]
            ),
            (CaseKind::Control, s[7].as_str(), "docs", vec![]),
            (CaseKind::Control, s[4].as_str(), "add logging", vec![]),
            (CaseKind::Control, s[1].as_str(), "add parse", vec![]),
        ],
        "the fix and the revert are no cases, and the root commit has no parent to review against"
    );
    assert_eq!(cases[1].parent, s[1]);
    assert_eq!(
        cases[1].lines,
        [BugLine {
            path: "app.py".into(),
            line: 3
        }],
        "the line the fix changed, where the bug commit wrote it"
    );
    let few = EvalOptions {
        limit: 1,
        controls: 1,
        ..EvalOptions::default()
    };
    let cases = build_cases(&ProcessRunner, &h.repo.display().to_string(), &few).unwrap();
    assert_eq!(
        cases.iter().map(|case| case.kind).collect::<Vec<_>>(),
        [CaseKind::Reverted, CaseKind::Control]
    );
}

fn result(findings: serde_json::Value) -> EngineResult {
    serde_json::from_value(json!({
        "complete": true, "files": 1, "added": 1, "removed": 1, "findings": findings,
        "timing": {"totalMs": 1000}, "usage": {"costUsd": 0.25, "frames": 2, "tokens": 10},
    }))
    .unwrap()
}

#[test]
fn review_eval_scores_blocks_and_located_bugs() {
    let case = Case {
        kind: CaseKind::FixInducing,
        sha: "a".repeat(40),
        parent: "b".repeat(40),
        subject: "speed up totals".into(),
        by: vec!["c".repeat(40)],
        lines: vec![BugLine {
            path: "app.py".into(),
            line: 3,
        }],
    };
    let near = |line: i64, level: &str| json!({"file": "app.py", "line": line, "level": level, "claim": "skips the last row", "verification": "confirmed"});
    let caught = score(&case, &result(json!([near(5, "high")])), "medium");
    assert_eq!(
        (caught.blocked, caught.located, caught.confirmed),
        (true, Some(true), 1)
    );
    let noted = score(&case, &result(json!([near(9, "low")])), "medium");
    assert_eq!(
        (noted.blocked, noted.located),
        (false, Some(false)),
        "a note far from the bug"
    );
    let control = Case {
        kind: CaseKind::Control,
        lines: Vec::new(),
        ..case.clone()
    };
    let quiet = score(&control, &result(json!([])), "medium");
    assert_eq!((quiet.blocked, quiet.located), (false, None));
    let totals = summary(&[(case.clone(), caught), (case, noted), (control, quiet)]);
    assert_eq!(
        totals["fixInducing"],
        json!({"cases": 2, "blocked": 1, "blockedShare": 0.5, "located": 1, "locatedShare": 0.5})
    );
    assert_eq!(
        totals["controls"],
        json!({"cases": 1, "blocked": 0, "blockedShare": 0.0})
    );
    assert_eq!(totals["costUsd"], 0.75);
    assert_eq!(
        summary_lines(&totals, "medium"),
        [
            "0 reverted, 2 fix-inducing and 1 control commits, reviewed with blockAt medium.",
            "Fix-inducing: 1 of 2 blocked; a confirmed finding within 3 lines of the fixed lines in 1.",
            "Controls: 0 of 1 blocked (an upper bound on false alarms).",
            "Cost: $0.7500.",
        ]
    );
}
