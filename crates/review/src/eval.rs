//! `ultron autoreview eval`: the reviewer measured on a repository's own mistakes (Rust-only). The cases come from
//! its history, with no model call:
//!
//! - **Reverted**: a commit a later `This reverts commit <sha>` undid. The whole change was wrong (or unwanted), so
//!   a review that requests changes on it caught it.
//! - **Fix-inducing**: a commit a later fix repaired, found as SZZ finds it (Śliwerski, Zimmermann and Zeller, "When
//!   do changes induce fixes?", MSR 2005): the lines a fix commit (its subject says fix, bug, regression or hotfix)
//!   removed or changed are blamed at the fix's parent, and the commits that wrote them are the cases, with those
//!   lines as where the bug was. A review caught it when it requests changes, and located it when a confirmed
//!   finding sits within [`LOCATE_WINDOW`] lines of one.
//! - **Control**: other commits of the same history, sampled evenly, for how often the reviewer blocks a change no
//!   one undid or fixed. Not every block of a control is a false alarm (the history may not show the fix), so the
//!   number is an upper bound on them.
//!
//! Each case is reviewed as `review --repo-dir --base <sha>^ --head <sha>` would review it.

use std::collections::{BTreeMap, HashSet};

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};

use crate::plan::{EngineResult, Verdict, decide_verdict};
use crate::runner::{Env, RunOptions, Runner, argv};

/// Commits of history read for cases.
pub const DEFAULT_SCAN: usize = 2000;
/// Cases reviewed (reverted and fix-inducing, newest first).
pub const DEFAULT_LIMIT: usize = 20;
/// A commit changing more lines than this (added plus removed) is no case: a review of it says little.
pub const MAX_CASE_LINES: u64 = 1500;
/// Fix commits blamed at most.
const MAX_FIXES: usize = 200;
/// A confirmed finding this close to a blamed line located the bug.
pub const LOCATE_WINDOW: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaseKind {
    Reverted,
    FixInducing,
    Control,
}

/// A line a later fix changed, in the case commit's own version of the file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct BugLine {
    pub path: String,
    pub line: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Case {
    pub kind: CaseKind,
    pub sha: String,
    pub parent: String,
    pub subject: String,
    /// The commits that reverted or fixed it.
    pub by: Vec<String>,
    /// Fix-inducing: the lines the fixes changed (at most 50).
    pub lines: Vec<BugLine>,
}

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub scan: usize,
    pub limit: usize,
    pub controls: usize,
    pub max_lines: u64,
}

impl Default for EvalOptions {
    fn default() -> Self {
        EvalOptions {
            scan: DEFAULT_SCAN,
            limit: DEFAULT_LIMIT,
            controls: DEFAULT_LIMIT,
            max_lines: MAX_CASE_LINES,
        }
    }
}

struct Commit {
    sha: String,
    parents: Vec<String>,
    subject: String,
    body: String,
}

fn git(runner: &dyn Runner, repo: &str, args: &[&str]) -> Result<String, String> {
    let mut parts = vec!["git", "-C", repo];
    parts.extend_from_slice(args);
    let mut env = Env::new();
    env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    // History lookups never reach the network.
    env.insert("GIT_NO_LAZY_FETCH".into(), "1".into());
    let result = runner.run(
        &argv(&parts),
        &RunOptions {
            env,
            timeout_ms: Some(120_000),
            ..RunOptions::default()
        },
    );
    if result.code == 0 {
        Ok(result.stdout)
    } else {
        Err(format!("git {}: {}", args.join(" "), result.stderr.trim()))
    }
}

fn history(runner: &dyn Runner, repo: &str, scan: usize) -> Result<Vec<Commit>, String> {
    let count = format!("-n{scan}");
    let out = git(
        runner,
        repo,
        &["log", &count, "--format=%H%x00%P%x00%s%x00%b%x1e", "HEAD"],
    )?;
    Ok(out
        .split('\u{1e}')
        .filter_map(|record| {
            let mut parts = record.trim_start_matches('\n').splitn(4, '\0');
            let sha = parts.next()?.trim().to_string();
            let parents = parts.next()?.split_whitespace().map(str::to_string).collect();
            let subject = parts.next()?.to_string();
            let body = parts.next().unwrap_or_default().to_string();
            (!sha.is_empty()).then_some(Commit {
                sha,
                parents,
                subject,
                body,
            })
        })
        .collect())
}

/// Lines `sha` changes (added plus removed; binary files count nothing).
fn changed_lines(runner: &dyn Runner, repo: &str, sha: &str) -> Option<u64> {
    let out = git(runner, repo, &["show", "--numstat", "--format=", "--no-renames", sha]).ok()?;
    Some(
        out.lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let added = fields.next()?.parse::<u64>().ok()?;
                let removed = fields.next()?.parse::<u64>().ok()?;
                Some(added + removed)
            })
            .sum(),
    )
}

/// The old-side line ranges (path, start, end) a commit removes or changes, from `git diff -U0`.
fn removed_ranges(diff: &str) -> Vec<(String, i64, i64)> {
    let hunk = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+").expect("static pattern");
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(old) = line.strip_prefix("--- ") {
            // A quoted path (unusual characters) or a new file is skipped.
            path = old
                .strip_prefix("a/")
                .filter(|p| !p.starts_with('"'))
                .map(str::to_string);
        } else if let (Some(found), Some(path)) = (hunk.captures(line), &path) {
            let start: i64 = found[1].parse().unwrap_or(0);
            let count: i64 = found.get(2).map_or(1, |n| n.as_str().parse().unwrap_or(0));
            if count > 0 {
                out.push((path.clone(), start, start + count - 1));
            }
        }
    }
    out
}

/// `git blame --porcelain` of lines `start..=end` of `path` at `rev`: (commit, path in it, its line) of each line
/// worth blaming (not blank or nearly so); never a boundary commit.
fn blame(runner: &dyn Runner, repo: &str, rev: &str, path: &str, start: i64, end: i64) -> Vec<(String, BugLine)> {
    let range = format!("{start},{end}");
    let Ok(out) = git(
        runner,
        repo,
        &["blame", "--porcelain", "-w", "-L", &range, rev, "--", path],
    ) else {
        return Vec::new();
    };
    let header = Regex::new(r"^([0-9a-f]{40}) (\d+) \d+").expect("static pattern");
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut boundary: HashSet<String> = HashSet::new();
    let mut current: Option<(String, i64)> = None;
    let mut found = Vec::new();
    for line in out.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            if let Some((sha, line)) = current.take()
                && !boundary.contains(&sha)
                && content.trim().chars().count() >= 3
            {
                let path = names.get(&sha).cloned().unwrap_or_else(|| path.to_string());
                found.push((sha, BugLine { path, line }));
            }
        } else if let Some(head) = header.captures(line) {
            current = Some((head[1].to_string(), head[2].parse().unwrap_or(0)));
        } else if let Some((sha, _)) = &current {
            if let Some(name) = line.strip_prefix("filename ") {
                names.insert(sha.clone(), name.to_string());
            } else if line == "boundary" {
                boundary.insert(sha.clone());
            }
        }
    }
    found
}

fn fix_subject(subject: &str) -> bool {
    Regex::new(r"(?i)\b(fix(es|ed)?|bug|bugfix|regression|hotfix)\b")
        .expect("static pattern")
        .is_match(subject)
}

/// The cases of the repository at `repo` (newest first: reverted and fix-inducing up to `limit`, then up to
/// `controls` controls).
pub fn build_cases(runner: &dyn Runner, repo: &str, options: &EvalOptions) -> Result<Vec<Case>, String> {
    let commits = history(runner, repo, options.scan)?;
    let by_sha: BTreeMap<&str, &Commit> = commits.iter().map(|commit| (commit.sha.as_str(), commit)).collect();
    let reverts = Regex::new(r"This reverts commit ([0-9a-f]{40})").expect("static pattern");
    // culprit -> (kind, by, lines)
    let mut found: BTreeMap<String, (CaseKind, Vec<String>, Vec<BugLine>)> = BTreeMap::new();
    let mut excluded: HashSet<String> = HashSet::new();
    for commit in &commits {
        let message = format!("{}\n{}", commit.subject, commit.body);
        if let Some(reverted) = reverts.captures(&message) {
            excluded.insert(commit.sha.clone());
            let entry = found
                .entry(reverted[1].to_string())
                .or_insert_with(|| (CaseKind::Reverted, Vec::new(), Vec::new()));
            entry.0 = CaseKind::Reverted;
            entry.1.push(commit.sha.clone());
        }
    }
    let fixes: Vec<&Commit> = commits
        .iter()
        .filter(|commit| commit.parents.len() == 1 && !excluded.contains(&commit.sha) && fix_subject(&commit.subject))
        .take(MAX_FIXES)
        .collect();
    for fix in fixes {
        excluded.insert(fix.sha.clone());
        let parent = &fix.parents[0];
        let Ok(diff) = git(
            runner,
            repo,
            &[
                "diff",
                "-U0",
                "--no-color",
                "--no-renames",
                "--no-ext-diff",
                parent,
                &fix.sha,
            ],
        ) else {
            continue;
        };
        for (path, start, end) in removed_ranges(&diff) {
            for (sha, line) in blame(runner, repo, parent, &path, start, end) {
                let entry = found
                    .entry(sha)
                    .or_insert_with(|| (CaseKind::FixInducing, Vec::new(), Vec::new()));
                if !entry.1.contains(&fix.sha) {
                    entry.1.push(fix.sha.clone());
                }
                if !entry.2.contains(&line) {
                    entry.2.push(line);
                }
            }
        }
    }
    let mut cases = Vec::new();
    // Newest first: the history's order.
    for commit in &commits {
        if cases.len() == options.limit {
            break;
        }
        let Some((kind, by, lines)) = found.get(&commit.sha) else {
            continue;
        };
        if commit.parents.len() != 1
            || excluded.contains(&commit.sha)
            || changed_lines(runner, repo, &commit.sha).is_none_or(|n| n == 0 || n > options.max_lines)
        {
            continue;
        }
        let mut lines = lines.clone();
        lines.sort();
        lines.truncate(50);
        cases.push(Case {
            kind: *kind,
            sha: commit.sha.clone(),
            parent: commit.parents[0].clone(),
            subject: commit.subject.clone(),
            by: by.clone(),
            lines,
        });
    }
    // Controls: every k-th of the rest, so they span the same stretch of history.
    let rest: Vec<&Commit> = commits
        .iter()
        .filter(|commit| {
            commit.parents.len() == 1
                && !excluded.contains(&commit.sha)
                && !found.contains_key(&commit.sha)
                && by_sha.contains_key(commit.parents[0].as_str())
        })
        .collect();
    if options.controls > 0 && !rest.is_empty() {
        let step = (rest.len() / options.controls).max(1);
        let mut taken = 0;
        for commit in rest.iter().step_by(step) {
            if taken == options.controls {
                break;
            }
            if changed_lines(runner, repo, &commit.sha).is_none_or(|n| n == 0 || n > options.max_lines) {
                continue;
            }
            taken += 1;
            cases.push(Case {
                kind: CaseKind::Control,
                sha: commit.sha.clone(),
                parent: commit.parents[0].clone(),
                subject: commit.subject.clone(),
                by: Vec::new(),
                lines: Vec::new(),
            });
        }
    }
    Ok(cases)
}

/// How one review of a case went.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseScore {
    /// The review requests changes.
    pub blocked: bool,
    /// Fix-inducing: a confirmed finding within [`LOCATE_WINDOW`] lines of a line the fix changed.
    pub located: Option<bool>,
    pub confirmed: usize,
    /// Every model frame ran (an incomplete review is scored, but named).
    pub complete: bool,
    pub cost_usd: f64,
}

/// Scores the review `result` of `case` at `block_at`.
pub fn score(case: &Case, result: &EngineResult, block_at: &str) -> CaseScore {
    let blocked = decide_verdict(result, false, "open", Some(block_at)).verdict == Verdict::RequestChanges;
    let confirmed: Vec<_> = result
        .findings
        .iter()
        .filter(|finding| finding.verification == "confirmed")
        .collect();
    let located = (case.kind == CaseKind::FixInducing).then(|| {
        confirmed.iter().any(|finding| {
            case.lines.iter().any(|bug| {
                bug.path == finding.file && {
                    let end = finding.end_line.unwrap_or(finding.line).max(finding.line);
                    bug.line >= finding.line - LOCATE_WINDOW && bug.line <= end + LOCATE_WINDOW
                }
            })
        })
    });
    CaseScore {
        blocked,
        located,
        confirmed: confirmed.len(),
        complete: result.complete,
        cost_usd: result.usage.cost_usd,
    }
}

fn share(part: usize, whole: usize) -> Value {
    if whole == 0 {
        Value::Null
    } else {
        json!(((part as f64 / whole as f64) * 1000.0).round() / 1000.0)
    }
}

/// The totals over scored cases: per kind, how many were blocked (and, fix-inducing, located).
pub fn summary(scored: &[(Case, CaseScore)]) -> Value {
    let of = |kind: CaseKind| -> Vec<&CaseScore> {
        scored
            .iter()
            .filter(|(case, _)| case.kind == kind)
            .map(|(_, score)| score)
            .collect()
    };
    let (reverted, fixes, controls) = (of(CaseKind::Reverted), of(CaseKind::FixInducing), of(CaseKind::Control));
    let blocked = |items: &[&CaseScore]| items.iter().filter(|score| score.blocked).count();
    let located = fixes.iter().filter(|score| score.located == Some(true)).count();
    json!({
        "reverted": {"cases": reverted.len(), "blocked": blocked(&reverted), "blockedShare": share(blocked(&reverted), reverted.len())},
        "fixInducing": {"cases": fixes.len(), "blocked": blocked(&fixes), "blockedShare": share(blocked(&fixes), fixes.len()),
            "located": located, "locatedShare": share(located, fixes.len())},
        "controls": {"cases": controls.len(), "blocked": blocked(&controls), "blockedShare": share(blocked(&controls), controls.len())},
        "incomplete": scored.iter().filter(|(_, score)| !score.complete).count(),
        "costUsd": (scored.iter().map(|(_, score)| score.cost_usd).sum::<f64>() * 1e6).round() / 1e6,
    })
}

/// The summary as lines of text.
pub fn summary_lines(summary: &Value, block_at: &str) -> Vec<String> {
    let count = |path: &str, key: &str| summary[path][key].as_u64().unwrap_or(0);
    let mut lines = vec![format!(
        "{} reverted, {} fix-inducing and {} control commits, reviewed with blockAt {block_at}.",
        count("reverted", "cases"),
        count("fixInducing", "cases"),
        count("controls", "cases")
    )];
    if count("reverted", "cases") > 0 {
        lines.push(format!(
            "Reverted: {} of {} blocked.",
            count("reverted", "blocked"),
            count("reverted", "cases")
        ));
    }
    if count("fixInducing", "cases") > 0 {
        lines.push(format!(
            "Fix-inducing: {} of {} blocked; a confirmed finding within {LOCATE_WINDOW} lines of the fixed lines in {}.",
            count("fixInducing", "blocked"),
            count("fixInducing", "cases"),
            count("fixInducing", "located")
        ));
    }
    if count("controls", "cases") > 0 {
        lines.push(format!(
            "Controls: {} of {} blocked (an upper bound on false alarms).",
            count("controls", "blocked"),
            count("controls", "cases")
        ));
    }
    let incomplete = summary["incomplete"].as_u64().unwrap_or(0);
    if incomplete > 0 {
        lines.push(format!("{incomplete} review(s) were incomplete."));
    }
    lines.push(format!("Cost: ${:.4}.", summary["costUsd"].as_f64().unwrap_or(0.0)));
    lines
}
