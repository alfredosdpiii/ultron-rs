//! Execution of a prepared pull-request review: checkout, diff/promotion preparation, test environment setup,
//! engine invocation, GitHub posting (or dry-run output), state persistence, and thread resolution.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::knowledge::{backfill_notes, index_knowledge, learn, learned_guide, repo_key, repo_ledger};
use super::{
    Outcome, Prepared, PreparedEnvironment, Reviewed, ReviewerDeps, dry_run_markdown, github_of, pull_state_key,
    seconds, short,
};
use crate::accounts::account_key;
use crate::ban::banned_reason;
use crate::checkout::{Checkout, PullHeads, make_private_dir, random_hex};
use crate::github::{Comment, Error, GitHub, PullRef, PullRequest, ReviewCommentInput, ReviewInput, pull_key};
use crate::ledger::{FateInput, ThreadState, fates_of, lessons_for};
use crate::local::{expand_path, find_checkout, without_guide_mentions};
use crate::plan::{EngineResult, PlanOptions, PromotionNote, ReviewPlan, Verdict, plan_review, without_inline};
use crate::promotion::{
    PromotionDelta, PromotionSource, changed_new_lines, cherry_sources, promotion_delta, referenced_pulls, source_label,
};
use crate::settings::{engine_settings, tests_eligible};
use crate::state::{PostedFinding, PullState, ReviewRecord, claim_hash, to_json_1};
use jsrt::json::Num;

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
struct EarlierStatus {
    id: String,
    status: String,
    line: i64,
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
fn lift_own_blocks(deps: &ReviewerDeps, github: &GitHub, r: &PullRef, me: &str, head: &str, name: &str) {
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
fn write_review_record(dir: &str, name: &str, record: &Value) -> std::io::Result<()> {
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

fn write_private(path: &str, text: &str) -> std::io::Result<()> {
    config::paths::write_private(std::path::Path::new(path), text)
}

fn review_input(head: &str, plan: &ReviewPlan) -> ReviewInput {
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
fn next_finding_number(findings: &[PostedFinding], short_sha: &str) -> usize {
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
        &std::env::temp_dir().to_string_lossy(),
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
    let last_sha = &prepared.last_sha;
    let io = |e: std::io::Error| Error::Other(e.to_string());

    let mut since_sha = None;
    let full_diff;
    let mut review_diff;
    let mut earlier_diff = None;
    if let Some(checkout) = checkout {
        full_diff = checkout.diff(&checkout.merge_base, head)?;
        review_diff = full_diff.clone();
        if let Some(last) = last_sha.as_deref().filter(|last| *last != head)
            && checkout.is_ancestor(last, head)
        {
            since_sha = Some(last.to_string());
            let incremental = checkout.diff(last, head)?;
            let pr_paths = changes_paths(&full_diff);
            review_diff = filter_diff_by_paths(&incremental, &pr_paths);
            earlier_diff = Some(review_diff.clone());
        } else if last_sha.as_deref() == Some(head.as_str()) {
            earlier_diff = Some(String::new());
        }
    } else {
        full_diff = github.pull_diff(r)?;
        review_diff = full_diff.clone();
        if last_sha.as_deref() == Some(head.as_str()) {
            earlier_diff = Some(String::new());
        }
    }
    let fresh_lines = if prepared.forced {
        None
    } else if last_sha.as_deref() == Some(head.as_str()) {
        Some(indexmap::IndexMap::new())
    } else {
        since_sha.as_ref().and(earlier_diff.as_deref()).map(changed_new_lines)
    };
    if prepared.recheck_only {
        (deps.log)(&format!(
            "{name}: {} was reviewed in full: re-checking the earlier findings only",
            short(head)
        ));
        review_diff = String::new();
    }
    let mut promotion: Option<PromotionDelta> = None;
    let mut promotion_inputs: Vec<PromotionSource> = Vec::new();
    if let Some(checkout) = checkout.filter(|_| !prepared.recheck_only) {
        promotion_inputs = promotion_sources(github, r, pull, checkout);
        if !promotion_inputs.is_empty() {
            let delta = promotion_delta(&review_diff, &promotion_inputs, &|path| {
                head_lines(&checkout.work_dir, path)
            });
            if !delta.sources.is_empty() {
                (deps.log)(&format!(
                    "{name}: carries {}: reading {} of {} changed lines ({} files carried over unchanged, {} read in full \
                     for other code around them, {} with lines left out)",
                    delta.sources.join(", "),
                    changed_lines(&delta.diff),
                    changed_lines(&review_diff),
                    delta.carried_files,
                    delta.context_files,
                    delta.missing.len()
                ));
                review_diff = delta.diff.clone();
                promotion = Some(delta);
            }
        }
    }
    let diff_path = jsrt::path::join(&[scratch, "review.diff"]);
    let post_diff_path = jsrt::path::join(&[scratch, "pull.diff"]);
    let earlier_diff_path = jsrt::path::join(&[scratch, "earlier.diff"]);
    std::fs::write(&diff_path, &review_diff).map_err(io)?;
    std::fs::write(&post_diff_path, &full_diff).map_err(io)?;
    if let Some(earlier_diff) = &earlier_diff {
        std::fs::write(&earlier_diff_path, earlier_diff).map_err(io)?;
    }
    Ok(PreparedDiffs {
        diff_path,
        post_diff_path,
        earlier_diff_path,
        has_earlier_diff: earlier_diff.is_some(),
        since_sha,
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

fn prepare_test_env(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    checkout: Option<&Checkout>,
) -> Result<TestEnvSetup, Error> {
    let r = &prepared.candidate.pull;
    let run_tests = checkout.is_some()
        && !prepared.recheck_only
        && deps.config.mode != "fast"
        && deps.config.run_tests
        && tests_eligible(&deps.config, &r.owner, github.can_push(r)?);
    let test_env = deps
        .config
        .test_env
        .get(&format!("{}/{}", r.owner, r.repo).to_lowercase())
        .cloned();
    let prepared_env = match (checkout, &deps.environments) {
        (Some(checkout), Some(environments)) if run_tests && test_env.is_none() && deps.config.prepare_envs => {
            let log = |line: &str| (deps.log)(&format!("{}: {line}", pull_key(r)));
            match environments.prepare(&checkout.work_dir, &log) {
                Ok(env) => Some(env),
                Err(error) => {
                    (deps.log)(&format!("{}: prepare failed: {error}", pull_key(r)));
                    None
                }
            }
        }
        _ => None,
    };
    if let Some(env) = &prepared_env {
        (deps.log)(&format!(
            "{}: environment {}{}: {}{}",
            pull_key(r),
            env.hash,
            if env.cached { " (cached)" } else { "" },
            if env.prepared.is_empty() {
                "nothing prepared".to_string()
            } else {
                env.prepared.join("; ")
            },
            if env.failures.is_empty() {
                String::new()
            } else {
                format!("; failed: {}", env.failures.join("; "))
            }
        ));
    }
    let test_checkout = match prepared_env.as_ref().filter(|env| !env.prepared.is_empty()) {
        Some(env) => Some(env.dir.clone()),
        None if run_tests && test_env.is_none() && !deps.config.checkout_roots.is_empty() => {
            find_checkout(deps.runner.as_ref(), &deps.config.checkout_roots, r)
        }
        None => None,
    };
    Ok(TestEnvSetup {
        run_tests,
        test_env,
        prepared_env,
        test_checkout,
    })
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
    let others: Vec<Value> = review_comments
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
        .collect();
    let lessons = if deps.config.learn {
        lessons_for(
            &repo_ledger(&deps.store.read(), &repo_key(r)),
            &jsrt::date::iso_string((deps.now)()),
        )
    } else {
        Vec::new()
    };

    let mut spec = Map::new();
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
    spec.insert("runTests".into(), json!(env_setup.run_tests));
    if prepared.recheck_only {
        spec.insert("recheckOnly".into(), json!(true));
    }
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
    context.insert("comments".into(), Value::Array(others));
    spec.insert("context".into(), Value::Object(context));
    if !lessons.is_empty() {
        spec.insert("lessons".into(), serde_json::to_value(&lessons).unwrap_or(Value::Null));
    }
    if !earlier.is_empty() {
        let replies = |finding: &PostedFinding| -> Vec<Value> {
            let Some(root) = finding.comment_id else {
                return Vec::new();
            };
            review_comments
                .iter()
                .filter(|comment| comment.in_reply_to == Some(root) && comment.user.to_lowercase() != me)
                .map(|comment| {
                    json!({ "author": comment.user, "body": jsrt::utf16::head(&comment.body, MAX_REPLY_CHARS) })
                })
                .collect()
        };
        let items: Vec<Value> = earlier
            .iter()
            .map(|f| {
                let mut item =
                    json!({ "id": f.id, "file": f.file, "line": f.line, "severity": f.severity, "claim": f.claim });
                let replies = replies(f);
                if !replies.is_empty() {
                    item["replies"] = Value::Array(replies);
                }
                item
            })
            .collect();
        spec.insert("earlier".into(), Value::Array(items));
        if diffs.has_earlier_diff {
            spec.insert("earlierDiffPath".into(), json!(diffs.earlier_diff_path));
        }
    }
    Value::Object(spec)
}

fn track_finding(
    result: &EngineResult,
    raw_findings: &[Value],
    short_sha: &str,
    head: &str,
    numbered: usize,
    index: usize,
    posted: &mut Vec<PostedFinding>,
) -> usize {
    let finding = &result.findings[index];
    let kind = raw_findings
        .get(index)
        .and_then(|f| f.get("kind"))
        .and_then(Value::as_str);
    let mut extra = indexmap::IndexMap::new();
    if let Some(Value::Array(ids)) = raw_findings.get(index).and_then(|f| f.get("rules"))
        && ids.iter().all(Value::is_i64)
        && !ids.is_empty()
    {
        extra.insert("rules".to_string(), Value::Array(ids.clone()));
    }
    posted.push(PostedFinding {
        id: format!("{short_sha}-{}", numbered + index),
        file: finding.file.clone(),
        line: finding.line,
        severity: finding.level().into(),
        claim: finding.claim.clone(),
        claim_hash: claim_hash(&finding.claim),
        category: Some(finding.category.clone()),
        kind: kind.map(String::from),
        sha: head.to_string(),
        comment_id: None,
        thread_id: None,
        status: "open".into(),
        extra,
    });
    posted.len() - 1
}

struct PublishOutcome {
    posted: Vec<PostedFinding>,
    review_id: Option<i64>,
    path: Option<String>,
}

#[expect(clippy::too_many_arguments, reason = "internal review publisher")]
fn publish_review(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    saved: &PullState,
    result: &EngineResult,
    raw: &Value,
    plan: &mut ReviewPlan,
    earlier: &[PostedFinding],
    statuses: &[EarlierStatus],
    me: &str,
    name: &str,
) -> Result<PublishOutcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let head = &prepared.pull.head_sha;
    let short_sha = short(head);
    let io = |e: std::io::Error| Error::Other(e.to_string());
    let raw_findings = match raw.get("findings") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let numbered = next_finding_number(&saved.findings, &short_sha);
    let mut posted: Vec<PostedFinding> = Vec::new();

    if prepared.dry_run {
        make_private_dir(&deps.paths.dry_run).map_err(io)?;
        let base = jsrt::path::join(&[
            &deps.paths.dry_run,
            &format!("{}-{}-{}-{short_sha}-{}", r.owner, r.repo, r.number, account.login),
        ]);
        let mut saved_json = Map::new();
        saved_json.insert("account".into(), json!(account_key(account)));
        saved_json.insert("pull".into(), json!(pull_key(r)));
        saved_json.insert("sha".into(), json!(head));
        if let Some(ack) = &prepared.ack {
            saved_json.insert("ack".into(), json!(ack));
        }
        saved_json.insert(
            "review".into(),
            serde_json::to_value(review_input(head, plan)).unwrap_or(Value::Null),
        );
        saved_json.insert("result".into(), raw.clone());
        write_private(&format!("{base}.json"), &format!("{}\n", to_json_1(&saved_json))).map_err(io)?;
        write_private(
            &format!("{base}.md"),
            &dry_run_markdown(candidate, head, plan, prepared.ack.as_deref()),
        )
        .map_err(io)?;
        for comment in &plan.comments {
            track_finding(
                result,
                &raw_findings,
                &short_sha,
                head,
                numbered,
                comment.finding,
                &mut posted,
            );
        }
        for index in &plan.in_summary {
            track_finding(result, &raw_findings, &short_sha, head, numbered, *index, &mut posted);
        }
        return Ok(PublishOutcome {
            posted,
            review_id: None,
            path: Some(base),
        });
    }

    let id = match github.post_review(r, &review_input(head, plan)) {
        Ok(id) => id,
        Err(Error::GitHub { message, status: 422 }) if !plan.comments.is_empty() => {
            let created = github.reviews(r)?.into_iter().find(|review| {
                review.user.to_lowercase() == me && review.commit_id == *head && review.body == plan.body
            });
            match created {
                Some(review) => {
                    (deps.log)(&format!(
                        "{name}: GitHub answered {message} but created the review; not posting it again"
                    ));
                    review.id
                }
                None => {
                    (deps.log)(&format!(
                        "{name}: GitHub rejected the inline comments ({message}); posting them in the summary"
                    ));
                    *plan = without_inline(plan, result);
                    github.post_review(r, &review_input(head, plan))?
                }
            }
        }
        Err(error) => return Err(error),
    };
    let mut entries: Vec<(String, usize)> = Vec::new();
    for comment in &plan.comments {
        let entry = track_finding(
            result,
            &raw_findings,
            &short_sha,
            head,
            numbered,
            comment.finding,
            &mut posted,
        );
        entries.push((comment.marker.clone(), entry));
    }
    for index in &plan.in_summary {
        track_finding(result, &raw_findings, &short_sha, head, numbered, *index, &mut posted);
    }
    if !plan.comments.is_empty() {
        let read_back = (|| -> Result<(), Error> {
            let kept = github.comments_of_review(r, id)?;
            for (comment, (_, entry)) in plan.comments.iter().zip(&entries) {
                if let Some(found) = kept.iter().find(|item| item.body.contains(&comment.marker)) {
                    posted[*entry].comment_id = Some(found.id);
                    continue;
                }
                let input = ReviewCommentInput {
                    path: comment.path.clone(),
                    line: comment.line,
                    side: comment.side.into(),
                    start_line: comment.start_line.filter(|_| comment.start_side.is_some()),
                    start_side: comment
                        .start_side
                        .filter(|_| comment.start_line.is_some())
                        .map(String::from),
                    body: comment.body.clone(),
                };
                match github.post_review_comment(r, head, &input) {
                    Ok(comment_id) => {
                        posted[*entry].comment_id = Some(comment_id);
                        (deps.log)(&format!(
                            "{name}: re-posted a dropped inline comment on {}:{}",
                            comment.path, comment.line
                        ));
                    }
                    Err(error @ Error::RateLimit { .. }) => return Err(error),
                    Err(error) => (deps.log)(&format!(
                        "{name}: an inline comment on {}:{} could not be posted: {error}",
                        comment.path, comment.line
                    )),
                }
            }
            let threads = github.review_threads(r)?;
            for (_, entry) in &entries {
                if let Some(thread) = posted[*entry].comment_id.and_then(|id| threads.get(&id)) {
                    posted[*entry].thread_id = Some(thread.id.clone());
                }
            }
            Ok(())
        })();
        if let Err(error) = read_back {
            (deps.log)(&format!("{name}: the posted review could not be read back: {error}"));
        }
    }
    if deps.config.learn && !earlier.is_empty() {
        let learned = (|| -> Result<(), Error> {
            let threads = github.review_threads(r)?;
            let fates = fates_of(&FateInput {
                pull: pull_key(r),
                now: jsrt::date::iso_string((deps.now)()),
                findings: earlier.iter().map(PostedFinding::ledger).collect(),
                statuses: statuses.iter().map(|s| (s.id.clone(), s.status.clone())).collect(),
                threads: thread_states(threads),
                resolved_by_us: saved
                    .findings
                    .iter()
                    .filter(|f| f.status == "fixed")
                    .filter_map(|f| f.thread_id.clone().filter(|id| !id.is_empty()))
                    .collect::<HashSet<String>>(),
                merged: false,
            });
            learn(deps, r, &fates, earlier, name)
        })();
        if let Err(error) = learned {
            (deps.log)(&format!("{name}: the fates of earlier findings were not read: {error}"));
        }
    }
    Ok(PublishOutcome {
        posted,
        review_id: Some(id),
        path: None,
    })
}

fn review_in(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    saved: &PullState,
    scratch: &str,
    checkout_slot: &mut Option<Checkout>,
) -> Result<Outcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let (dry_run, pull) = (prepared.dry_run, &prepared.pull);
    let github = github_of(deps, account);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);
    let head = pull.head_sha.clone();
    let me = account.login.to_lowercase();
    let now = || (deps.now)();

    let heads = PullHeads {
        head_sha: head.clone(),
        base_ref: pull.base_ref.clone(),
        base_sha: pull.base_sha.clone(),
    };
    match deps
        .tokens
        .env(account)
        .and_then(|env| deps.checkouts.prepare(r, &heads, &env))
    {
        Ok(prepared_checkout) => *checkout_slot = Some(prepared_checkout),
        Err(Error::StaleHead(message)) => return Ok(Outcome::Requeue(message)),
        Err(error) => (deps.log)(&format!("{name}: no checkout, reviewing the diff only: {error}")),
    }
    let checkout = checkout_slot.as_ref();
    let review_comments = github.review_comments(r)?;
    let ci = github.check_summary(r, &head)?;
    let earlier: Vec<PostedFinding> = saved.findings.iter().filter(|f| f.status == "open").cloned().collect();
    let diffs = prepare_diffs(deps, &github, prepared, checkout, scratch, &name)?;
    let env_setup = prepare_test_env(deps, &github, prepared, checkout)?;
    let spec = build_engine_spec(
        deps,
        &github,
        prepared,
        checkout,
        &diffs,
        &env_setup,
        &review_comments,
        &earlier,
        ci.as_deref(),
        scratch,
        &me,
        &name,
    );
    if let Some(reason) = banned_now(deps, prepared) {
        (deps.log)(&format!("{name}: {reason}; the review was not started"));
        return Ok(Outcome::Skipped(reason));
    }
    let raw = deps.engine.review(&spec, None).map_err(Error::Other)?;
    let result = EngineResult::parse(&raw).map_err(Error::Other)?;
    let reviewed_at_ms = now();

    let current = github.pull(r)?;
    if current.head_sha != head {
        (deps.log)(&format!(
            "{name}: the head moved to {} during the review; discarded",
            short(&current.head_sha)
        ));
        return Ok(Outcome::Requeue("the head commit moved during the review".into()));
    }
    if let Some(reason) = banned_now(deps, prepared) {
        (deps.log)(&format!("{name}: {reason}; the finished review was discarded"));
        return Ok(Outcome::Skipped(reason));
    }

    let state = if current.state == "open" {
        "open"
    } else if current.merged {
        "merged"
    } else {
        "closed"
    };
    let mut plan = plan_review(
        &result,
        &PlanOptions {
            self_authored: pull.author.to_lowercase() == me,
            state: state.into(),
            head_sha: head.clone(),
            since_sha: diffs.since_sha,
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
            fresh_lines: diffs.fresh_lines,
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
    let reviewed_at = jsrt::date::iso_string(now());
    let short_sha = short(&head);
    let statuses = earlier_statuses(&raw);
    let published = publish_review(
        deps, &github, prepared, saved, &result, &raw, &mut plan, &earlier, &statuses, &me, &name,
    )?;

    if !dry_run {
        let mut record = Map::new();
        record.insert("account".into(), json!(account_key(account)));
        record.insert("pull".into(), json!(pull_key(r)));
        record.insert("sha".into(), json!(head));
        record.insert("reviewId".into(), json!(published.review_id));
        record.insert(
            "review".into(),
            serde_json::to_value(review_input(&head, &plan)).unwrap_or(Value::Null),
        );
        record.insert(
            "posted".into(),
            serde_json::to_value(&published.posted).unwrap_or(Value::Null),
        );
        record.insert("result".into(), raw.clone());
        if let Some(delta) = &diffs.promotion {
            record.insert(
                "promotion".into(),
                json!({ "sources": delta.sources, "carriedLines": delta.carried_lines,
                    "carriedFiles": delta.carried_files, "contextFiles": delta.context_files,
                    "missing": delta.missing }),
            );
        }
        let name_of = format!("{}-{}-{}-{short_sha}-{}", r.owner, r.repo, r.number, account.login);
        if let Err(error) = write_review_record(&deps.paths.dir, &name_of, &Value::Object(record)) {
            (deps.log)(&format!("{name}: the review record was not written: {error}"));
        }
    }

    let ack_to_post_ms = prepared.ack_at.map(|at| (now() - at).max(0));
    let verdict = plan.verdict.name().to_string();
    let stages = match raw.get("timing").and_then(|timing| timing.get("stages")) {
        Some(Value::Object(stages)) => stages.clone().into_iter().collect(),
        _ => indexmap::IndexMap::new(),
    };
    deps.store.update_pull(&key, |pull_state, all| {
        pull_state.last_reviewed_sha = Some(head.clone());
        pull_state.last_reviewed_at = Some(reviewed_at.clone());
        pull_state.last_verdict = Some(verdict.clone());
        pull_state.last_review_complete = Some(result.complete || prepared.recheck_only);
        pull_state.closed = None;
        pull_state.last_review_dry_run = dry_run.then_some(true);
        if let Some(id) = published.review_id {
            pull_state.last_review_id = Some(id);
        }
        pull_state.attempt_succeeded(&head);
        for status in &statuses {
            let Some(finding) = pull_state.findings.iter_mut().find(|item| item.id == status.id) else {
                continue;
            };
            if status.status == "fixed" || status.status == "not_applicable" {
                finding.status = status.status.clone();
            } else {
                finding.line = status.line;
            }
        }
        pull_state.findings.extend(published.posted.iter().cloned());
        if pull_state.findings.len() > 200 {
            let excess = pull_state.findings.len() - 200;
            pull_state.findings.drain(..excess);
        }
        let mut stages = stages.clone();
        stages.insert("postMs".into(), json!(Num(((now() - reviewed_at_ms).max(0)) as f64)));
        all.recent.push(ReviewRecord {
            account: account_key(account),
            pull: pull_key(r),
            sha: head.clone(),
            at: reviewed_at.clone(),
            outcome: if dry_run { "dry-run" } else { "posted" }.into(),
            verdict: Some(verdict.clone()),
            findings: Some(Num(result.findings.len() as f64)),
            total_ms: Some(Num(result.timing.total_ms)),
            stages: Some(stages),
            pickup_to_post_ms: Some(Num((now() - candidate.picked_at) as f64)),
            tag_to_ack_ms: prepared.tag_to_ack_ms.map(|ms| Num(ms as f64)),
            ack_to_post_ms: ack_to_post_ms.map(|ms| Num(ms as f64)),
            cost_usd: Some(Num(result.usage.cost_usd)),
            dry_run: dry_run.then_some(true),
            extra: Default::default(),
        });
    })?;
    let mut resolved: Vec<String> = Vec::new();
    if !dry_run {
        if plan.verdict == Verdict::Comment && state == "open" && pull.author.to_lowercase() != me {
            lift_own_blocks(deps, &github, r, &me, &head, &name);
        }
        for status in statuses.iter().filter(|status| status.status == "fixed") {
            let Some(finding) = saved.findings.iter().find(|item| item.id == status.id) else {
                continue;
            };
            let Some(thread_id) = &finding.thread_id else { continue };
            match github.resolve_thread(thread_id) {
                Ok(()) => resolved.push(finding.id.clone()),
                Err(error) => (deps.log)(&format!("{name}: thread of {} was not resolved: {error}", finding.id)),
            }
        }
    }
    (deps.log)(&format!(
        "{name}: {} {} for {short_sha}: {} findings, {} inline, {} threads resolved, {} s from pickup{}",
        if dry_run { "dry-run" } else { "posted" },
        plan.event,
        result.findings.len(),
        plan.comments.len(),
        resolved.len(),
        jsrt::number::to_string(jsrt::number::round((now() - candidate.picked_at) as f64 / 1000.0)),
        ack_to_post_ms.map_or(String::new(), |ms| format!(
            ", {} after the acknowledgement",
            seconds(ms)
        ))
    ));
    let reviewed = Box::new(Reviewed {
        verdict: plan.verdict.name(),
        sha: head,
        result: raw,
        plan,
        review_id: published.review_id,
        path: published.path,
    });
    Ok(if dry_run {
        Outcome::DryRun(reviewed)
    } else {
        Outcome::Posted(reviewed)
    })
}
