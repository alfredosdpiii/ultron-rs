//! Knowledge graph indexing, review-note recording/backfill, and lesson learning for [`super::ReviewerDeps`].

use std::collections::HashSet;

use serde_json::{Value, json};

use super::ReviewerDeps;
use crate::github::{Error, GitHub, PullRef, pull_key};
use crate::ledger::{Fate, FateKind, Lesson, lessons_for, merge_fates, without_unposted_merges};
use crate::state::{AutoreviewState, PostedFinding, PullState};

/// Where fates and rules are also retained (Hindsight).
pub trait LessonMemory: Send + Sync {
    fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), String>;
    /// What the memory has learned about reviewing `repo` (which findings its team accepts or rejects, and the rules
    /// that follow), as a review guide; `None` while it has learned nothing.
    fn guide(&self, _repo: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// The learned guide of `repo` written to `scratch` for the engine, when learning is on and the memory has one.
/// Never fails a review: an error is logged and the review runs without it.
pub(super) fn learned_guide(deps: &ReviewerDeps, repo: &str, scratch: &str) -> Option<String> {
    let memory = deps.lessons.as_ref().filter(|_| deps.config.learn)?;
    let text = match memory.guide(repo) {
        Ok(text) => text.filter(|text| !text.trim().is_empty())?,
        Err(error) => {
            (deps.log)(&format!("{repo}: the learned review guide was not read: {error}"));
            return None;
        }
    };
    let path = jsrt::path::join(&[scratch, "learned-review-lessons.md"]);
    let body = format!(
        "# What reviews of {repo} have taught\n\nWritten by memory from the fate of earlier findings in this \
         repository (accepted or rejected by its team). Weigh it when deciding what to report and at what level.\n\n{}\n",
        text.trim()
    );
    match std::fs::write(&path, body) {
        Ok(()) => Some(path),
        Err(error) => {
            (deps.log)(&format!("{repo}: the learned review guide was not written: {error}"));
            None
        }
    }
}

/// `host/owner/repo`: the key of a repository's ledger of fates.
pub fn repo_key(r: &PullRef) -> String {
    format!("{}/{}/{}", r.host, r.owner, r.repo).to_lowercase()
}

/// Indexes the checkout at `rev` into the knowledge graph and returns the spec's `knowledge` (store, repository,
/// commit) for the kernel to read. Never fails a review: an error is logged and the review runs without it. A
/// directory that is not a git checkout (a test's) is not indexed.
pub fn index_knowledge(deps: &ReviewerDeps, repo: &str, work_dir: &str, rev: &str) -> Option<Value> {
    if !std::path::Path::new(work_dir).join(".git").exists() {
        return None;
    }
    let db = std::path::Path::new(&deps.paths.knowledge);
    match knowledge::index_tree(db, repo, std::path::Path::new(work_dir), rev) {
        Ok(stats) => Some(json!({ "db": deps.paths.knowledge, "repo": repo, "rev": stats.rev })),
        Err(error) => {
            (deps.log)(&format!("{repo}: the knowledge graph was not updated: {error}"));
            None
        }
    }
}

/// A person's addressed comment within this many lines of no finding of ours is a miss.
pub const MISS_WINDOW: i64 = 10;

/// The reviewing accounts' logins this state knows (pull keys are `<login>@<pull>`).
fn our_logins(state: &AutoreviewState) -> Vec<String> {
    let mut logins: Vec<String> = state
        .pulls
        .keys()
        .filter_map(|key| key.split_once('@').map(|(login, _)| login.to_lowercase()))
        .chain(
            state
                .accounts
                .keys()
                .filter_map(|key| key.rsplit('/').next().map(str::to_lowercase)),
        )
        .collect();
    logins.sort();
    logins.dedup();
    logins
}

/// Sets what makes a note a rule (`knowledge::Note`): written by a person (not a bot, not one of our accounts) and,
/// where this reviewer reviewed the pull request, whether it raised nothing at that place (a miss).
pub fn classify_notes(notes: &mut [knowledge::Note], state: &AutoreviewState, r: &PullRef) {
    let logins = our_logins(state);
    let pull = pull_key(r);
    let ours: Vec<&PullState> = state
        .pulls
        .iter()
        .filter(|(key, _)| key.split_once('@').is_some_and(|(_, of)| of == pull))
        .map(|(_, saved)| saved)
        .collect();
    let reviewed = ours.iter().any(|saved| saved.last_verdict.is_some());
    for note in notes.iter_mut() {
        note.human = note.human && !logins.contains(&note.author.to_lowercase());
        note.missed = reviewed
            && note.human
            && note.state == "addressed"
            && !ours.iter().flat_map(|saved| &saved.findings).any(|finding| {
                finding.file == note.path && note.line.is_none_or(|line| (finding.line - line).abs() <= MISS_WINDOW)
            });
    }
}

/// Records the review threads of a closed pull request, by every reviewer, as notes in the knowledge graph: the
/// investigators' `review_notes` and `rules_for` lookups read them. Never fails a review: an error is logged.
pub fn record_notes(deps: &ReviewerDeps, r: &PullRef, github: &GitHub, name: &str) {
    let db = std::path::Path::new(&deps.paths.knowledge);
    let indexed = db.exists()
        && knowledge::Store::open(db)
            .and_then(|store| store.tree_count(&repo_key(r)))
            .is_ok_and(|trees| trees > 0);
    if !indexed {
        return;
    }
    let state = deps.store.read();
    let recorded = github
        .review_notes(r)
        .map_err(|error| error.to_string())
        .and_then(|mut notes| {
            classify_notes(&mut notes, &state, r);
            let mut store = knowledge::Store::open(std::path::Path::new(&deps.paths.knowledge))
                .map_err(|error| error.to_string())?;
            let added = store
                .record_notes(&repo_key(r), &notes)
                .map_err(|error| error.to_string())?;
            let _ = store.mark_not_human(&repo_key(r), &our_logins(&state));
            Ok((notes.len(), added, notes.iter().filter(|note| note.missed).count()))
        });
    match recorded {
        Ok((0, _, _)) => {}
        Ok((notes, added, missed)) => (deps.log)(&format!(
            "{name}: recorded {notes} review notes ({added} new, {missed} where this review raised nothing)"
        )),
        Err(error) => (deps.log)(&format!("{name}: the review notes were not recorded: {error}")),
    }
}

/// Closed pull requests whose review threads are read when a repository has no notes yet.
pub const BACKFILL_PULLS: usize = 50;

/// A repository indexed for the first time has no notes: those of its most recently closed pull requests are read
/// once (again after a schema migration), so the lookups have history from the first review on. Never fails a review: an error is logged.
pub(super) fn backfill_notes(deps: &ReviewerDeps, r: &PullRef, github: &GitHub, name: &str) {
    let db = std::path::Path::new(&deps.paths.knowledge);
    let repo = repo_key(r);
    let Ok(mut store) = knowledge::Store::open(db) else {
        return;
    };
    let marker = format!("notes_backfilled:{repo}");
    if store.meta(&marker).map_or(true, |done| done.is_some()) {
        return;
    }
    let pulls = match github.closed_pulls(&r.owner, &r.repo, 1) {
        Ok(pulls) => pulls,
        Err(error) => {
            (deps.log)(&format!("{name}: the review notes were not backfilled: {error}"));
            return;
        }
    };
    let (mut read, mut added) = (0, 0);
    let state = deps.store.read();
    for number in pulls.into_iter().take(BACKFILL_PULLS) {
        let pull = PullRef::new(&r.host, &r.owner, &r.repo, number);
        match github
            .review_notes(&pull)
            .map_err(|error| error.to_string())
            .and_then(|mut notes| {
                classify_notes(&mut notes, &state, &pull);
                store.record_notes(&repo, &notes).map_err(|error| error.to_string())
            }) {
            Ok(new) => {
                read += 1;
                added += new;
            }
            Err(error) => {
                (deps.log)(&format!(
                    "{name}: the review notes of #{number} were not backfilled: {error}"
                ));
                if error.to_lowercase().contains("rate limit") {
                    break;
                }
            }
        }
    }
    if read > 0 {
        let _ = store.set_meta(&marker, &jsrt::date::iso_string((deps.now)()));
        let _ = store.mark_not_human(&repo, &our_logins(&state));
    }
    (deps.log)(&format!(
        "{name}: backfilled {added} review notes from {read} closed pull requests"
    ));
}

/// `(pull, id)` of the findings that no account posted as a review comment: listed only in a review body.
fn unposted_findings(state: &AutoreviewState) -> HashSet<(String, String)> {
    let (mut posted, mut unposted) = (HashSet::new(), HashSet::new());
    for (key, pull) in &state.pulls {
        let Some((_, pull_ref)) = key.split_once('@') else {
            continue;
        };
        for finding in &pull.findings {
            let id = (pull_ref.to_string(), finding.id.clone());
            if finding.comment_id.is_some() {
                posted.insert(id);
            } else {
                unposted.insert(id);
            }
        }
    }
    unposted.retain(|id| !posted.contains(id));
    unposted
}

/// A repository's ledger as the lessons read it: without merge rejections of findings nobody saw.
pub(super) fn repo_ledger(state: &AutoreviewState, repo: &str) -> Vec<Fate> {
    let ledger = state
        .lessons
        .as_ref()
        .and_then(|all| all.get(repo))
        .map_or(&[][..], Vec::as_slice);
    without_unposted_merges(ledger, &unposted_findings(state))
}

/// The rules (knowledge notes, by id) a posted finding cited (`rules`, from the engine).
pub fn finding_rules(finding: &PostedFinding) -> Vec<i64> {
    match finding.extra.get("rules") {
        Some(Value::Array(ids)) => ids.iter().filter_map(Value::as_i64).collect(),
        _ => Vec::new(),
    }
}

/// Credit to the rules the findings of `fates` cited: each fate once (those already in the ledger were credited
/// when they came in). Returns how many rules were credited.
fn credit_rules(deps: &ReviewerDeps, repo: &str, fates: &[&Fate], findings: &[PostedFinding]) -> usize {
    let credits: Vec<(i64, bool)> = fates
        .iter()
        .filter_map(|fate| {
            findings
                .iter()
                .find(|finding| finding.id == fate.id)
                .map(|finding| (fate, finding))
        })
        .flat_map(|(fate, finding)| {
            finding_rules(finding)
                .into_iter()
                .map(move |id| (id, fate.fate == FateKind::Accepted))
        })
        .collect();
    let db = std::path::Path::new(&deps.paths.knowledge);
    if credits.is_empty() || !db.exists() {
        return 0;
    }
    knowledge::Store::open(db)
        .and_then(|mut store| store.credit_rules(repo, &credits))
        .unwrap_or(0)
}

/// Record what became of posted findings: into the state's ledger and, when configured, into the lesson memory,
/// and as credit to the rules they cited. Never fails a review: an error is logged.
pub(super) fn learn(
    deps: &ReviewerDeps,
    r: &PullRef,
    fates: &[Fate],
    findings: &[PostedFinding],
    name: &str,
) -> Result<(), Error> {
    if fates.is_empty() || !deps.config.learn {
        return Ok(());
    }
    let repo = repo_key(r);
    let now = jsrt::date::iso_string((deps.now)());
    let (lessons, new) = deps.store.update(|state| {
        let before = repo_ledger(state, &repo);
        let known: HashSet<String> = before.iter().map(|fate| format!("{}:{}", fate.pull, fate.id)).collect();
        let new: Vec<Fate> = fates
            .iter()
            .filter(|fate| !known.contains(&format!("{}:{}", fate.pull, fate.id)))
            .cloned()
            .collect();
        let merged = merge_fates(&before, fates);
        let lessons = lessons_for(&merged, &now);
        state
            .lessons
            .get_or_insert_with(Default::default)
            .insert(repo.clone(), merged);
        (lessons, new)
    })?;
    let credited = credit_rules(deps, &repo, &new.iter().collect::<Vec<_>>(), findings);
    if credited > 0 {
        (deps.log)(&format!(
            "{name}: credited {credited} rules with the fates of the findings that cited them"
        ));
    }
    let accepted = fates.iter().filter(|fate| fate.fate == FateKind::Accepted).count();
    let lowered: Vec<String> = lessons
        .iter()
        .filter(|lesson| lesson.lowered)
        .map(|lesson| format!("{}/{}", lesson.category, lesson.kind))
        .collect();
    (deps.log)(&format!(
        "{name}: learned the fate of {} finding{} ({accepted} accepted, {} rejected){}",
        fates.len(),
        if fates.len() == 1 { "" } else { "s" },
        fates.len() - accepted,
        if lowered.is_empty() {
            String::new()
        } else {
            format!("; lowered: {}", lowered.join(", "))
        }
    ));
    if let Some(memory) = &deps.lessons
        && let Err(error) = memory.retain(&repo, fates, &lessons)
    {
        (deps.log)(&format!("{name}: the fates were not retained in memory: {error}"));
    }
    Ok(())
}
