//! Execution of a prepared pull-request review: checkout, diff/promotion preparation, test environment setup,
//! engine invocation, GitHub posting (or dry-run output), state persistence, and thread resolution.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::knowledge::{backfill_notes, index_knowledge, learned_guide, repo_key, repo_ledger};
use super::publish::{finalize_posted_threads, persist_review_state, publish_review, write_posted_review_record};
use super::{
    Outcome, Prepared, PreparedEnvironment, Reviewed, ReviewerDeps, github_of, pull_state_key, seconds, short,
};
use crate::ban::banned_reason;
use crate::checkout::{Checkout, PullHeads, make_private_dir, random_hex};
use crate::github::{Comment, Error, GitHub, PullRef, PullRequest, ReviewCommentInput, ReviewInput, pull_key};
use crate::ledger::{ThreadState, lessons_for};
use crate::local::{expand_path, find_checkout, without_guide_mentions};
use crate::plan::{EngineResult, PlanOptions, PromotionNote, ReviewPlan, plan_review};
use crate::promotion::{
    PromotionDelta, PromotionSource, changed_new_lines, cherry_sources, promotion_delta, referenced_pulls, source_label,
};
use crate::settings::{engine_settings, tests_eligible};
use crate::state::{PostedFinding, PullState, to_json_1};

/// A reply in a finding's thread is cut to this many characters for the re-check.
pub const MAX_REPLY_CHARS: usize = 2000;

/// Review records kept: the newest this many.
pub const MAX_REVIEW_RECORDS: usize = 200;

pub(super) fn thread_states(
    threads: indexmap::IndexMap<i64, crate::github::ReviewThread>,
) -> HashMap<i64, ThreadState> {
    threads
        .into_iter()
        .map(|(id, thread)| {
            (
                id,
                ThreadState {
                    id: thread.id,
                    resolved: thread.resolved,
                    thumbs_up: thread.thumbs_up,
                    thumbs_down: thread.thumbs_down,
                },
            )
        })
        .collect()
}

/// An earlier finding's status as the re-review settled it.
pub(super) struct EarlierStatus {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) line: i64,
}

fn earlier_statuses(result: &Value) -> Vec<EarlierStatus> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = result.get("earlier") {
        for item in items {
            out.push(EarlierStatus {
                id: crate::github::text(item.get("id")),
                status: crate::github::text(item.get("status")),
                line: item.get("line").and_then(Value::as_i64).unwrap_or(0),
            });
        }
    }
    out
}

/// Dismiss this account's requests for changes on the pull request, after a review at `head` that blocks nothing.
/// Never fails a review: an error (an account that may not dismiss reviews there) is logged.
pub(super) fn lift_own_blocks(deps: &ReviewerDeps, github: &GitHub, r: &PullRef, me: &str, head: &str, name: &str) {
    let reviews = match github.reviews(r) {
        Ok(reviews) => reviews,
        Err(error) => {
            (deps.log)(&format!(
                "{name}: the earlier requests for changes were not read: {error}"
            ));
            return;
        }
    };
    let message = format!(
        "Lifted by the review of `{}`: none of its findings blocks the merge.",
        short(head)
    );
    for review in reviews
        .iter()
        .filter(|review| review.user.to_lowercase() == me && review.state == "CHANGES_REQUESTED")
    {
        match github.dismiss_review(r, review.id, &message) {
            Ok(()) => (deps.log)(&format!(
                "{name}: dismissed its request for changes at {}",
                short(&review.commit_id)
            )),
            Err(error) => (deps.log)(&format!(
                "{name}: its request for changes at {} was not dismissed: {error}",
                short(&review.commit_id)
            )),
        }
    }
}

/// Writes `<dir>/reviews/<name>.json` and removes the oldest records beyond [`MAX_REVIEW_RECORDS`].
pub(super) fn write_review_record(dir: &str, name: &str, record: &Value) -> std::io::Result<()> {
    let reviews = jsrt::path::join(&[dir, "reviews"]);
    make_private_dir(&reviews)?;
    write_private(
        &jsrt::path::join(&[&reviews, &format!("{name}.json")]),
        &format!("{}\n", to_json_1(record)),
    )?;
    let mut records: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(&reviews)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    if records.len() > MAX_REVIEW_RECORDS {
        records.sort();
        for (_, path) in &records[..records.len() - MAX_REVIEW_RECORDS] {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

pub(super) fn write_private(path: &str, text: &str) -> std::io::Result<()> {
    config::paths::write_private(std::path::Path::new(path), text)
}

pub(super) fn review_input(head: &str, plan: &ReviewPlan) -> ReviewInput {
    ReviewInput {
        commit_id: head.into(),
        event: plan.event.into(),
        body: plan.body.clone(),
        comments: plan
            .comments
            .iter()
            .map(|comment| ReviewCommentInput {
                path: comment.path.clone(),
                line: comment.line,
                side: comment.side.into(),
                start_line: comment.start_line,
                start_side: comment.start_line.and(comment.start_side.map(String::from)),
                body: comment.body.clone(),
            })
            .collect(),
    }
}

/// The number after the highest `<short_sha>-<n>` id among `findings` (1 when there is none).
pub(super) fn next_finding_number(findings: &[PostedFinding], short_sha: &str) -> usize {
    findings
        .iter()
        .filter_map(|finding| {
            finding
                .id
                .strip_prefix(short_sha)?
                .strip_prefix('-')?
                .parse::<usize>()
                .ok()
        })
        .max()
        .map_or(1, |highest| highest + 1)
}

/// Review a prepared pull request and post the review (or write it to the dry-run directory).
pub fn run_review(deps: &ReviewerDeps, prepared: &Prepared) -> Result<Outcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);
    let head = prepared.pull.head_sha.clone();
    if let Some(reason) = banned_now(deps, prepared) {
        return Ok(Outcome::Skipped(reason));
    }
    let saved: PullState = deps.store.read().pulls.get(&key).cloned().unwrap_or_default();
    (deps.log)(&format!("{name}: reviewing {} ({})", short(&head), prepared.reason));
    let failed = FailedAttempt {
        deps,
        key: &key,
        head: &head,
    };
    let scratch = jsrt::path::join(&[
        &jsrt::fs::temp_dir().to_string_lossy(),
        &format!("ultron-autoreview-diff-{}", random_hex(6)),
    ]);
    make_private_dir(&scratch).map_err(|e| Error::Other(format!("{scratch}: {e}")))?;
    let mut checkout: Option<Checkout> = None;
    let outcome = review_in(deps, prepared, &saved, &scratch, &mut checkout);
    match &outcome {
        Err(Error::RateLimit { .. }) | Ok(_) => {}
        Err(_) => failed.record(),
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if let Some(checkout) = &checkout {
        checkout.cleanup();
    }
    outcome
}

/// Counts a failed attempt at a head commit: by [`FailedAttempt::record`], or when the review panics.
struct FailedAttempt<'a> {
    deps: &'a ReviewerDeps,
    key: &'a str,
    head: &'a str,
}

impl FailedAttempt<'_> {
    fn record(&self) {
        if let Err(error) = self
            .deps
            .store
            .update_pull(self.key, |state, _| state.attempt_failed(self.head))
        {
            (self.deps.log)(&format!("{}: the failed attempt was not recorded: {error}", self.key));
        }
    }
}

impl Drop for FailedAttempt<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.record();
        }
    }
}

/// The skip reason when the prepared pull request's author is banned for its account now (the list re-read).
fn banned_now(deps: &ReviewerDeps, prepared: &Prepared) -> Option<String> {
    let author = &prepared.pull.author;
    deps.bans
        .refresh()
        .banned(&prepared.candidate.account, author)
        .map(|_| banned_reason(author))
}

/// The carried sources (`labels`) whose pull request was not approved: none of its reviewers approved it, or one's
/// latest verdict asks for changes, or it is not found. An entirely carried cherry-pick is approved only when this
/// is empty: it was read where those were, and is as approved as they are.
fn unapproved_sources(github: &GitHub, r: &PullRef, sources: &[PromotionSource], labels: &[String]) -> Vec<String> {
    let approved = |source: &PromotionSource| {
        let number = source.pull.or_else(|| {
            github
                .commit_pulls(&r.owner, &r.repo, &source.sha)
                .ok()
                .and_then(|pulls| pulls.into_iter().find(|number| *number != r.number))
        });
        let Some(reviews) = number.and_then(|n| github.reviews(&PullRef::new(&r.host, &r.owner, &r.repo, n)).ok())
        else {
            return false;
        };
        let mut latest: HashMap<String, &str> = HashMap::new();
        for review in &reviews {
            if matches!(review.state.as_str(), "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
                latest.insert(review.user.to_lowercase(), review.state.as_str());
            }
        }
        latest.values().any(|state| *state == "APPROVED") && !latest.values().any(|state| *state == "CHANGES_REQUESTED")
    };
    let mut out: Vec<String> = sources
        .iter()
        .filter(|source| labels.contains(&source.label) && !approved(source))
        .map(|source| source.label.clone())
        .collect();
    out.dedup();
    out
}

fn changed_lines(diff: &str) -> usize {
    diff.lines()
        .filter(|line| {
            (line.starts_with('+') && !line.starts_with("+++")) || (line.starts_with('-') && !line.starts_with("---"))
        })
        .count()
}

/// The merged pull requests this one carries to its base branch, as diffs (`promotion.rs`): the commits its
/// `git cherry-pick -x` trailers name, and, into a branch other than the default one, the merged pull requests of
/// another base branch that its title or description names. Empty for any other pull request.
fn promotion_sources(github: &GitHub, r: &PullRef, pull: &PullRequest, checkout: &Checkout) -> Vec<PromotionSource> {
    let commits = checkout
        .commits(&checkout.merge_base, &pull.head_sha)
        .unwrap_or_default();
    let mut wanted: Vec<(String, String, Option<u64>)> = cherry_sources(&commits)
        .into_iter()
        .map(|sha| {
            let subject = commits
                .iter()
                .find(|commit| commit.message.contains(&sha))
                .map_or("", |commit| commit.subject.as_str());
            (source_label(subject, &sha), sha, None)
        })
        .collect();
    if !pull.default_branch.is_empty() && pull.base_ref != pull.default_branch {
        for number in referenced_pulls(&format!("{}\n{}", pull.title, pull.body), r.number) {
            let Ok(other) = github.pull(&PullRef::new(&r.host, &r.owner, &r.repo, number)) else {
                continue;
            };
            if let Some(sha) = other
                .merge_commit_sha
                .filter(|_| other.merged && other.base_ref != pull.base_ref)
                && !wanted.iter().any(|(_, known, _)| *known == sha)
            {
                wanted.push((format!("#{number}"), sha, Some(number)));
            }
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    checkout.fetch_commits(&wanted.iter().map(|(_, sha, _)| sha.clone()).collect::<Vec<_>>());
    wanted
        .into_iter()
        .filter_map(|(label, sha, number)| {
            let parent = format!("{sha}^1");
            let diff = checkout.diff(&parent, &sha).ok()?;
            let context_differs = match checkout.changed_paths(&checkout.merge_base, &parent) {
                Ok(paths) => paths.into_iter().collect(),
                Err(_) => changes_paths(&diff),
            };
            (!diff.is_empty()).then_some(PromotionSource {
                label,
                sha,
                pull: number,
                diff,
                context_differs,
            })
        })
        .collect()
}

/// The paths a diff touches (its `+++ b/` and `--- a/` lines).
fn changes_paths(diff: &str) -> HashSet<String> {
    diff.lines()
        .filter_map(|line| line.strip_prefix("+++ b/").or_else(|| line.strip_prefix("--- a/")))
        .map(str::to_string)
        .collect()
}

/// Keep only unified-diff file sections whose path appears in `allowed`, so an incremental
/// `last..head` diff after merging the base branch never drags unrelated base-branch files into a PR review.
fn filter_diff_by_paths(diff: &str, allowed: &HashSet<String>) -> String {
    if diff.is_empty() || allowed.is_empty() {
        return String::new();
    }
    let mut kept = String::new();
    let mut section = String::new();
    let mut section_matches = false;
    let flush = |out: &mut String, buf: &mut String, matched: bool| {
        if matched && !buf.is_empty() {
            out.push_str(buf);
        }
        buf.clear();
    };
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            flush(&mut kept, &mut section, section_matches);
            section_matches = false;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(path) = trimmed
            .strip_prefix("+++ b/")
            .or_else(|| trimmed.strip_prefix("--- a/"))
            && allowed.contains(path)
        {
            section_matches = true;
        }
        section.push_str(line);
    }
    flush(&mut kept, &mut section, section_matches);
    kept
}

/// A file of the head as lines, for what a cherry-pick left out; `None` outside the worktree or when absent.
fn head_lines(work_dir: &str, path: &str) -> Option<Vec<String>> {
    let relative = std::path::Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let text = std::fs::read_to_string(std::path::Path::new(work_dir).join(relative)).ok()?;
    Some(text.split('\n').map(str::to_string).collect())
}

struct PreparedDiffs {
    diff_path: String,
    post_diff_path: String,
    earlier_diff_path: String,
    has_earlier_diff: bool,
    since_sha: Option<String>,
    fresh_lines: Option<indexmap::IndexMap<String, Vec<(i64, i64)>>>,
    promotion: Option<PromotionDelta>,
    promotion_inputs: Vec<PromotionSource>,
}

struct RawDiffs {
    full_diff: String,
    review_diff: String,
    earlier_diff: Option<String>,
    since_sha: Option<String>,
}

fn compute_raw_diffs(
    github: &GitHub,
    r: &PullRef,
    checkout: Option<&Checkout>,
    head: &str,
    last_sha: Option<&str>,
) -> Result<RawDiffs, Error> {
    let Some(checkout) = checkout else {
        let full_diff = github.pull_diff(r)?;
        let earlier_diff = (last_sha == Some(head)).then(String::new);
        return Ok(RawDiffs {
            review_diff: full_diff.clone(),
            full_diff,
            earlier_diff,
            since_sha: None,
        });
    };
    let full_diff = checkout.diff(&checkout.merge_base, head)?;
    if let Some(last) = last_sha.filter(|last| *last != head)
        && checkout.is_ancestor(last, head)
    {
        let incremental = checkout.diff(last, head)?;
        let pr_paths = changes_paths(&full_diff);
        let review_diff = filter_diff_by_paths(&incremental, &pr_paths);
        return Ok(RawDiffs {
            full_diff,
            earlier_diff: Some(review_diff.clone()),
            review_diff,
            since_sha: Some(last.to_string()),
        });
    }
    let earlier_diff = (last_sha == Some(head)).then(String::new);
    Ok(RawDiffs {
        review_diff: full_diff.clone(),
        full_diff,
        earlier_diff,
        since_sha: None,
    })
}

fn compute_promotion(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &PullRef,
    pull: &PullRequest,
    checkout: &Checkout,
    review_diff: &mut String,
    name: &str,
) -> (Option<PromotionDelta>, Vec<PromotionSource>) {
    let inputs = promotion_sources(github, r, pull, checkout);
    if inputs.is_empty() {
        return (None, inputs);
    }
    let delta = promotion_delta(review_diff, &inputs, &|path| head_lines(&checkout.work_dir, path));
    if delta.sources.is_empty() {
        return (None, inputs);
    }
    (deps.log)(&format!(
        "{name}: carries {}: reading {} of {} changed lines ({} files carried over unchanged, {} read in full \
         for other code around them, {} with lines left out)",
        delta.sources.join(", "),
        changed_lines(&delta.diff),
        changed_lines(review_diff),
        delta.carried_files,
        delta.context_files,
        delta.missing.len()
    ));
    *review_diff = delta.diff.clone();
    (Some(delta), inputs)
}

fn prepare_diffs(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    checkout: Option<&Checkout>,
    scratch: &str,
    name: &str,
) -> Result<PreparedDiffs, Error> {
    let r = &prepared.candidate.pull;
    let pull = &prepared.pull;
    let head = &pull.head_sha;
    let io = |e: std::io::Error| Error::Other(e.to_string());
    let mut raw = compute_raw_diffs(github, r, checkout, head, prepared.last_sha.as_deref())?;
    let fresh_lines = if prepared.forced {
        None
    } else if prepared.last_sha.as_deref() == Some(head.as_str()) {
        Some(indexmap::IndexMap::new())
    } else {
        raw.since_sha
            .as_ref()
            .and(raw.earlier_diff.as_deref())
            .map(changed_new_lines)
    };
    if prepared.recheck_only {
        (deps.log)(&format!(
            "{name}: {} was reviewed in full: re-checking the earlier findings only",
            short(head)
        ));
        raw.review_diff = String::new();
    }
    let (promotion, promotion_inputs) = match checkout.filter(|_| !prepared.recheck_only) {
        Some(checkout) => compute_promotion(deps, github, r, pull, checkout, &mut raw.review_diff, name),
        None => (None, Vec::new()),
    };
    let diff_path = jsrt::path::join(&[scratch, "review.diff"]);
    let post_diff_path = jsrt::path::join(&[scratch, "pull.diff"]);
    let earlier_diff_path = jsrt::path::join(&[scratch, "earlier.diff"]);
    std::fs::write(&diff_path, &raw.review_diff).map_err(io)?;
    std::fs::write(&post_diff_path, &raw.full_diff).map_err(io)?;
    if let Some(earlier_diff) = &raw.earlier_diff {
        std::fs::write(&earlier_diff_path, earlier_diff).map_err(io)?;
    }
    Ok(PreparedDiffs {
        diff_path,
        post_diff_path,
        earlier_diff_path,
        has_earlier_diff: raw.earlier_diff.is_some(),
        since_sha: raw.since_sha,
        fresh_lines,
        promotion,
        promotion_inputs,
    })
}

struct TestEnvSetup {
    run_tests: bool,
    test_env: Option<String>,
    prepared_env: Option<PreparedEnvironment>,
    test_checkout: Option<String>,
}

fn log_prepared_env(deps: &ReviewerDeps, r: &PullRef, env: &PreparedEnvironment) {
    let summary = if env.prepared.is_empty() {
        "nothing prepared".to_string()
    } else {
        env.prepared.join("; ")
    };
    let failures = if env.failures.is_empty() {
        String::new()
    } else {
        format!("; failed: {}", env.failures.join("; "))
    };
    (deps.log)(&format!(
        "{}: environment {}{}: {summary}{failures}",
        pull_key(r),
        env.hash,
        if env.cached { " (cached)" } else { "" },
    ));
}

fn try_prepare_cached_env(
    deps: &ReviewerDeps,
    r: &PullRef,
    checkout: Option<&Checkout>,
    enabled: bool,
) -> Option<PreparedEnvironment> {
    if !enabled {
        return None;
    }
    let (checkout, environments) = (checkout?, deps.environments.as_ref()?);
    let log = |line: &str| (deps.log)(&format!("{}: {line}", pull_key(r)));
    match environments.prepare(&checkout.work_dir, &log) {
        Ok(env) => {
            log_prepared_env(deps, r, &env);
            Some(env)
        }
        Err(error) => {
            (deps.log)(&format!("{}: prepare failed: {error}", pull_key(r)));
            None
        }
    }
}

fn should_run_tests(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &PullRef,
    prepared: &Prepared,
    checkout: Option<&Checkout>,
) -> Result<bool, Error> {
    if checkout.is_none() || prepared.recheck_only || deps.config.mode == "fast" || !deps.config.run_tests {
        return Ok(false);
    }
    Ok(tests_eligible(&deps.config, &r.owner, github.can_push(r)?))
}

fn resolve_test_checkout(
    deps: &ReviewerDeps,
    r: &PullRef,
    prepared_env: Option<&PreparedEnvironment>,
    run_tests: bool,
    has_test_env: bool,
) -> Option<String> {
    if let Some(env) = prepared_env.filter(|env| !env.prepared.is_empty()) {
        return Some(env.dir.clone());
    }
    if run_tests && !has_test_env && !deps.config.checkout_roots.is_empty() {
        return find_checkout(deps.runner.as_ref(), &deps.config.checkout_roots, r);
    }
    None
}

fn prepare_test_env(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    checkout: Option<&Checkout>,
) -> Result<TestEnvSetup, Error> {
    let r = &prepared.candidate.pull;
    let run_tests = should_run_tests(deps, github, r, prepared, checkout)?;
    let test_env = deps
        .config
        .test_env
        .get(&format!("{}/{}", r.owner, r.repo).to_lowercase())
        .cloned();
    let prepared_env = try_prepare_cached_env(
        deps,
        r,
        checkout,
        run_tests && test_env.is_none() && deps.config.prepare_envs,
    );
    let test_checkout = resolve_test_checkout(deps, r, prepared_env.as_ref(), run_tests, test_env.is_some());
    Ok(TestEnvSetup {
        run_tests,
        test_env,
        prepared_env,
        test_checkout,
    })
}

fn other_review_comments(review_comments: &[Comment], me: &str) -> Vec<Value> {
    review_comments
        .iter()
        .filter(|comment| comment.user.to_lowercase() != me && comment.path.is_some())
        .map(|comment| {
            let mut item = Map::new();
            item.insert("author".into(), json!(comment.user));
            item.insert("body".into(), json!(comment.body));
            if let Some(path) = &comment.path {
                item.insert("path".into(), json!(path));
            }
            if let Some(line) = comment.line {
                item.insert("line".into(), json!(line));
            }
            Value::Object(item)
        })
        .collect()
}

fn insert_checkout_and_promotion(
    spec: &mut Map<String, Value>,
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &PullRef,
    checkout: Option<&Checkout>,
    diffs: &PreparedDiffs,
    name: &str,
) {
    if let Some(checkout) = checkout {
        spec.insert("workDir".into(), json!(checkout.work_dir));
        spec.insert("baseSha".into(), json!(checkout.merge_base));
        if let Some(knowledge) = index_knowledge(deps, &repo_key(r), &checkout.work_dir, "HEAD") {
            spec.insert("knowledge".into(), knowledge);
            if deps.config.learn {
                backfill_notes(deps, r, github, name);
            }
        }
    }
    if let Some(delta) = diffs.promotion.as_ref().filter(|delta| changed_lines(&delta.diff) == 0) {
        let unapproved = unapproved_sources(github, r, &diffs.promotion_inputs, &delta.sources);
        if unapproved.is_empty() {
            spec.insert("carried".into(), json!(true));
        } else {
            (deps.log)(&format!(
                "{name}: carries only reviewed lines, but {} not approved: no approval",
                unapproved.join(", ")
            ));
            spec.insert("unapprovedSources".into(), json!(unapproved));
        }
    }
}

fn insert_test_env_spec(spec: &mut Map<String, Value>, env_setup: &TestEnvSetup) {
    spec.insert("runTests".into(), json!(env_setup.run_tests));
    if let Some(test_env) = env_setup.test_env.as_ref().filter(|_| env_setup.run_tests) {
        spec.insert("testEnv".into(), json!(test_env));
        spec.insert("testEnvKind".into(), json!("testEnv"));
    }
    let use_prepared = env_setup.prepared_env.as_ref().filter(|env| !env.prepared.is_empty());
    if let Some(test_checkout) = &env_setup.test_checkout {
        spec.insert("testCheckout".into(), json!(test_checkout));
        spec.insert(
            "testEnvKind".into(),
            json!(if use_prepared.is_some() { "prepared" } else { "checkout" }),
        );
    }
    if let Some(env) = use_prepared {
        spec.insert("testToolchain".into(), json!(env.toolchain));
        if !env.failures.is_empty() {
            spec.insert("testEnvFailures".into(), json!(env.failures));
        }
    }
}

fn earlier_spec_items(earlier: &[PostedFinding], review_comments: &[Comment], me: &str) -> Vec<Value> {
    earlier
        .iter()
        .map(|f| {
            let mut item =
                json!({ "id": f.id, "file": f.file, "line": f.line, "severity": f.severity, "claim": f.claim });
            let replies: Vec<Value> = f.comment_id.map_or_default(|root| {
                review_comments
                    .iter()
                    .filter(|comment| comment.in_reply_to == Some(root) && comment.user.to_lowercase() != me)
                    .map(|comment| {
                        json!({
                            "author": comment.user,
                            "body": jsrt::utf16::head(&comment.body, MAX_REPLY_CHARS)
                        })
                    })
                    .collect()
            });
            if !replies.is_empty() {
                item["replies"] = Value::Array(replies);
            }
            item
        })
        .collect()
}

#[expect(clippy::too_many_arguments, reason = "internal review spec builder")]
fn build_engine_spec(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    checkout: Option<&Checkout>,
    diffs: &PreparedDiffs,
    env_setup: &TestEnvSetup,
    review_comments: &[Comment],
    earlier: &[PostedFinding],
    ci: Option<&str>,
    scratch: &str,
    me: &str,
    name: &str,
) -> Value {
    let r = &prepared.candidate.pull;
    let pull = &prepared.pull;
    let lessons = if deps.config.learn {
        lessons_for(
            &repo_ledger(&deps.store.read(), &repo_key(r)),
            &jsrt::date::iso_string((deps.now)()),
        )
    } else {
        Vec::new()
    };
    let mut spec = Map::new();
    insert_checkout_and_promotion(&mut spec, deps, github, r, checkout, diffs, name);
    insert_test_env_spec(&mut spec, env_setup);
    if prepared.recheck_only {
        spec.insert("recheckOnly".into(), json!(true));
    }
    let mut guides: Vec<String> = deps
        .config
        .guides
        .iter()
        .map(|path| expand_path(path, &deps.home))
        .collect();
    guides.extend(learned_guide(deps, &repo_key(r), scratch));
    if !guides.is_empty() {
        spec.insert("guides".into(), json!(guides));
    }
    spec.insert("repo".into(), json!(format!("{}/{}", r.owner, r.repo)));
    spec.insert("diffPath".into(), json!(diffs.diff_path));
    spec.insert("postDiffPath".into(), json!(diffs.post_diff_path));
    spec.insert("label".into(), json!(format!("{}/{}#{}", r.owner, r.repo, r.number)));
    if let Ok(Value::Object(settings)) = serde_json::to_value(engine_settings(&deps.config)) {
        spec.extend(settings);
    }
    let mut context = Map::new();
    context.insert("title".into(), json!(pull.title));
    context.insert("description".into(), json!(pull.body));
    if let Some(ci) = ci {
        context.insert("ci".into(), json!(ci));
    }
    context.insert(
        "comments".into(),
        Value::Array(other_review_comments(review_comments, me)),
    );
    spec.insert("context".into(), Value::Object(context));
    if !lessons.is_empty() {
        spec.insert("lessons".into(), serde_json::to_value(&lessons).unwrap_or(Value::Null));
    }
    if !earlier.is_empty() {
        spec.insert(
            "earlier".into(),
            Value::Array(earlier_spec_items(earlier, review_comments, me)),
        );
        if diffs.has_earlier_diff {
            spec.insert("earlierDiffPath".into(), json!(diffs.earlier_diff_path));
        }
    }
    Value::Object(spec)
}

#[expect(clippy::too_many_arguments, reason = "internal review plan builder")]
fn build_review_plan(
    deps: &ReviewerDeps,
    result: &EngineResult,
    raw: &Value,
    pull: &PullRequest,
    state: &str,
    diffs: &mut PreparedDiffs,
    me: &str,
    name: &str,
) -> ReviewPlan {
    let mut plan = plan_review(
        result,
        &PlanOptions {
            self_authored: pull.author.to_lowercase() == me,
            state: state.into(),
            head_sha: pull.head_sha.clone(),
            since_sha: diffs.since_sha.take(),
            signature: deps.config.signature,
            block_at: Some(deps.config.block_at.clone()),
            never_request_changes: deps.config.never_request_changes,
            max_comments: Some(deps.config.max_comments.0 as i64),
            promotion: diffs.promotion.as_ref().map(|delta| PromotionNote {
                sources: delta.sources.clone(),
                carried_lines: delta.carried_lines,
                carried_files: delta.carried_files,
                context_files: delta.context_files,
                missing: delta.missing.clone(),
                entire: changed_lines(&delta.diff) == 0,
            }),
            fresh_lines: diffs.fresh_lines.take(),
        },
    );
    let guide_names: Vec<String> = match raw.get("guideNames") {
        Some(Value::Array(names)) => names.iter().filter_map(|n| n.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    let (checked, withheld) = without_guide_mentions(&plan, &guide_names);
    if !withheld.is_empty() {
        plan = checked;
        (deps.log)(&format!(
            "{name}: withheld {}: it named a private review guide",
            withheld.join("; ")
        ));
    }
    plan
}

fn pull_lifecycle_state(current: &PullRequest) -> &'static str {
    if current.state == "open" {
        "open"
    } else if current.merged {
        "merged"
    } else {
        "closed"
    }
}

fn prepare_checkout_slot(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    checkout_slot: &mut Option<Checkout>,
    name: &str,
) -> Option<Outcome> {
    let (account, r) = (&prepared.candidate.account, &prepared.candidate.pull);
    let heads = PullHeads {
        head_sha: prepared.pull.head_sha.clone(),
        base_ref: prepared.pull.base_ref.clone(),
        base_sha: prepared.pull.base_sha.clone(),
    };
    match deps
        .tokens
        .env(account)
        .and_then(|env| deps.checkouts.prepare(r, &heads, &env))
    {
        Ok(prepared_checkout) => {
            *checkout_slot = Some(prepared_checkout);
            None
        }
        Err(Error::StaleHead(message)) => Some(Outcome::Requeue(message)),
        Err(error) => {
            (deps.log)(&format!("{name}: no checkout, reviewing the diff only: {error}"));
            None
        }
    }
}

fn check_after_review(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    name: &str,
) -> Result<Result<PullRequest, Outcome>, Error> {
    let current = github.pull(&prepared.candidate.pull)?;
    if current.head_sha != prepared.pull.head_sha {
        (deps.log)(&format!(
            "{name}: the head moved to {} during the review; discarded",
            short(&current.head_sha)
        ));
        return Ok(Err(Outcome::Requeue("the head commit moved during the review".into())));
    }
    if let Some(reason) = banned_now(deps, prepared) {
        (deps.log)(&format!("{name}: {reason}; the finished review was discarded"));
        return Ok(Err(Outcome::Skipped(reason)));
    }
    Ok(Ok(current))
}

#[expect(clippy::too_many_arguments, reason = "internal review completion logger")]
fn log_review_done(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    name: &str,
    short_sha: &str,
    plan: &ReviewPlan,
    finding_count: usize,
    resolved_count: usize,
    ack_to_post_ms: Option<i64>,
) {
    let elapsed_s = jsrt::number::to_string(jsrt::number::round(
        ((deps.now)() - prepared.candidate.picked_at) as f64 / 1000.0,
    ));
    let ack_tail = ack_to_post_ms.map_or(String::new(), |ms| {
        format!(", {} after the acknowledgement", seconds(ms))
    });
    (deps.log)(&format!(
        "{name}: {} {} for {short_sha}: {finding_count} findings, {} inline, {resolved_count} threads resolved, {elapsed_s} s from pickup{ack_tail}",
        if prepared.dry_run { "dry-run" } else { "posted" },
        plan.event,
        plan.comments.len(),
    ));
}

struct ReviewContext {
    diffs: PreparedDiffs,
    earlier: Vec<PostedFinding>,
    spec: Value,
}

#[expect(clippy::too_many_arguments, reason = "internal review context builder")]
fn gather_review_context(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    saved: &PullState,
    checkout: Option<&Checkout>,
    scratch: &str,
    me: &str,
    name: &str,
) -> Result<ReviewContext, Error> {
    let r = &prepared.candidate.pull;
    let review_comments = github.review_comments(r)?;
    let ci = github.check_summary(r, &prepared.pull.head_sha)?;
    let earlier: Vec<PostedFinding> = saved.findings.iter().filter(|f| f.status == "open").cloned().collect();
    let diffs = prepare_diffs(deps, github, prepared, checkout, scratch, name)?;
    let env_setup = prepare_test_env(deps, github, prepared, checkout)?;
    let spec = build_engine_spec(
        deps,
        github,
        prepared,
        checkout,
        &diffs,
        &env_setup,
        &review_comments,
        &earlier,
        ci.as_deref(),
        scratch,
        me,
        name,
    );
    Ok(ReviewContext { diffs, earlier, spec })
}

#[expect(clippy::too_many_arguments, reason = "internal review publisher and recorder")]
fn publish_and_record(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    saved: &PullState,
    key: &str,
    current: &PullRequest,
    raw: Value,
    result: &EngineResult,
    reviewed_at_ms: i64,
    mut ctx: ReviewContext,
    me: &str,
    name: &str,
) -> Result<Outcome, Error> {
    let head = prepared.pull.head_sha.clone();
    let state = pull_lifecycle_state(current);
    let mut plan = build_review_plan(deps, result, &raw, &prepared.pull, state, &mut ctx.diffs, me, name);
    let reviewed_at = jsrt::date::iso_string((deps.now)());
    let short_sha = short(&head);
    let statuses = earlier_statuses(&raw);
    let published = publish_review(
        deps,
        github,
        prepared,
        saved,
        result,
        &raw,
        &mut plan,
        &ctx.earlier,
        &statuses,
        me,
        name,
    )?;
    if !prepared.dry_run {
        write_posted_review_record(
            deps,
            prepared,
            &plan,
            &published,
            &raw,
            ctx.diffs.promotion.as_ref(),
            name,
        );
    }
    let ack_to_post_ms = prepared.ack_at.map(|at| ((deps.now)() - at).max(0));
    persist_review_state(
        deps,
        prepared,
        key,
        result,
        &raw,
        &plan,
        &published,
        &statuses,
        &reviewed_at,
        reviewed_at_ms,
        ack_to_post_ms,
    )?;
    let resolved = finalize_posted_threads(deps, github, prepared, saved, &plan, &statuses, state, me, name);
    log_review_done(
        deps,
        prepared,
        name,
        &short_sha,
        &plan,
        result.findings.len(),
        resolved.len(),
        ack_to_post_ms,
    );
    let reviewed = Box::new(Reviewed {
        verdict: plan.verdict.name(),
        sha: head,
        result: raw,
        plan,
        review_id: published.review_id,
        path: published.path,
    });
    Ok(if prepared.dry_run {
        Outcome::DryRun(reviewed)
    } else {
        Outcome::Posted(reviewed)
    })
}

fn review_in(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    saved: &PullState,
    scratch: &str,
    checkout_slot: &mut Option<Checkout>,
) -> Result<Outcome, Error> {
    let (account, r) = (&prepared.candidate.account, &prepared.candidate.pull);
    let github = github_of(deps, account);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);
    let me = account.login.to_lowercase();

    if let Some(early) = prepare_checkout_slot(deps, prepared, checkout_slot, &name) {
        return Ok(early);
    }
    let ctx = gather_review_context(
        deps,
        &github,
        prepared,
        saved,
        checkout_slot.as_ref(),
        scratch,
        &me,
        &name,
    )?;
    if let Some(reason) = banned_now(deps, prepared) {
        (deps.log)(&format!("{name}: {reason}; the review was not started"));
        return Ok(Outcome::Skipped(reason));
    }
    let raw = deps.engine.review(&ctx.spec, None).map_err(Error::Other)?;
    let result = EngineResult::parse(&raw).map_err(Error::Other)?;
    let reviewed_at_ms = (deps.now)();
    let current = match check_after_review(deps, &github, prepared, &name)? {
        Ok(current) => current,
        Err(early) => return Ok(early),
    };
    publish_and_record(
        deps,
        &github,
        prepared,
        saved,
        &key,
        &current,
        raw,
        &result,
        reviewed_at_ms,
        ctx,
        &me,
        &name,
    )
}
