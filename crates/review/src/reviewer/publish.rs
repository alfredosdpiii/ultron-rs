use std::collections::HashSet;

use jsrt::json::Num;
use serde_json::{Map, Value, json};

use super::knowledge::learn;
use super::run::{
    EarlierStatus, lift_own_blocks, next_finding_number, review_input, thread_states, write_private,
    write_review_record,
};
use super::{Prepared, ReviewerDeps, dry_run_markdown, short};
use crate::accounts::account_key;
use crate::checkout::make_private_dir;
use crate::github::{Error, GitHub, ReviewCommentInput, pull_key};
use crate::ledger::{FateInput, fates_of};
use crate::plan::{EngineResult, ReviewPlan, Verdict, without_inline};
use crate::promotion::PromotionDelta;
use crate::state::{PostedFinding, PullState, ReviewRecord, claim_hash, to_json_1};

pub(super) struct PublishOutcome {
    pub(super) posted: Vec<PostedFinding>,
    pub(super) review_id: Option<i64>,
    pub(super) path: Option<String>,
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

fn track_plan_findings(
    result: &EngineResult,
    raw_findings: &[Value],
    short_sha: &str,
    head: &str,
    numbered: usize,
    plan: &ReviewPlan,
    posted: &mut Vec<PostedFinding>,
) -> Vec<(String, usize)> {
    let mut entries: Vec<(String, usize)> = Vec::new();
    for comment in &plan.comments {
        let entry = track_finding(result, raw_findings, short_sha, head, numbered, comment.finding, posted);
        entries.push((comment.marker.clone(), entry));
    }
    for index in &plan.in_summary {
        track_finding(result, raw_findings, short_sha, head, numbered, *index, posted);
    }
    entries
}

#[expect(clippy::too_many_arguments, reason = "internal dry-run publisher")]
fn publish_dry_run(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    result: &EngineResult,
    raw: &Value,
    raw_findings: &[Value],
    plan: &ReviewPlan,
    short_sha: &str,
    numbered: usize,
) -> Result<PublishOutcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let head = &prepared.pull.head_sha;
    let io = |e: std::io::Error| Error::Other(e.to_string());
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
    let mut posted = Vec::new();
    track_plan_findings(result, raw_findings, short_sha, head, numbered, plan, &mut posted);
    Ok(PublishOutcome {
        posted,
        review_id: None,
        path: Some(base),
    })
}

#[expect(clippy::too_many_arguments, reason = "internal review poster")]
fn post_review_with_fallback(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &crate::github::PullRef,
    head: &str,
    result: &EngineResult,
    plan: &mut ReviewPlan,
    me: &str,
    name: &str,
) -> Result<i64, Error> {
    match github.post_review(r, &review_input(head, plan)) {
        Ok(id) => Ok(id),
        Err(Error::GitHub { message, status: 422 }) if !plan.comments.is_empty() => {
            let created = github.reviews(r)?.into_iter().find(|review| {
                review.user.to_lowercase() == me && review.commit_id == *head && review.body == plan.body
            });
            match created {
                Some(review) => {
                    (deps.log)(&format!(
                        "{name}: GitHub answered {message} but created the review; not posting it again"
                    ));
                    Ok(review.id)
                }
                None => {
                    (deps.log)(&format!(
                        "{name}: GitHub rejected the inline comments ({message}); posting them in the summary"
                    ));
                    *plan = without_inline(plan, result);
                    github.post_review(r, &review_input(head, plan))
                }
            }
        }
        Err(error) => Err(error),
    }
}

fn repost_dropped_comment(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &crate::github::PullRef,
    head: &str,
    comment: &crate::plan::PlannedComment,
    name: &str,
) -> Result<Option<i64>, Error> {
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
            (deps.log)(&format!(
                "{name}: re-posted a dropped inline comment on {}:{}",
                comment.path, comment.line
            ));
            Ok(Some(comment_id))
        }
        Err(error @ Error::RateLimit { .. }) => Err(error),
        Err(error) => {
            (deps.log)(&format!(
                "{name}: an inline comment on {}:{} could not be posted: {error}",
                comment.path, comment.line
            ));
            Ok(None)
        }
    }
}

fn attach_thread_ids(
    github: &GitHub,
    r: &crate::github::PullRef,
    entries: &[(String, usize)],
    posted: &mut [PostedFinding],
) -> Result<(), Error> {
    let threads = github.review_threads(r)?;
    for (_, entry) in entries {
        if let Some(thread) = posted[*entry].comment_id.and_then(|id| threads.get(&id)) {
            posted[*entry].thread_id = Some(thread.id.clone());
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "internal comment reconciler")]
fn reconcile_inline_comments(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &crate::github::PullRef,
    head: &str,
    review_id: i64,
    plan: &ReviewPlan,
    entries: &[(String, usize)],
    posted: &mut [PostedFinding],
    name: &str,
) {
    let read_back = (|| -> Result<(), Error> {
        let kept = github.comments_of_review(r, review_id)?;
        for (comment, (_, entry)) in plan.comments.iter().zip(entries) {
            if let Some(found) = kept.iter().find(|item| item.body.contains(&comment.marker)) {
                posted[*entry].comment_id = Some(found.id);
            } else if let Some(comment_id) = repost_dropped_comment(deps, github, r, head, comment, name)? {
                posted[*entry].comment_id = Some(comment_id);
            }
        }
        attach_thread_ids(github, r, entries, posted)
    })();
    if let Err(error) = read_back {
        (deps.log)(&format!("{name}: the posted review could not be read back: {error}"));
    }
}

fn learn_earlier_fates(
    deps: &ReviewerDeps,
    github: &GitHub,
    r: &crate::github::PullRef,
    saved: &PullState,
    earlier: &[PostedFinding],
    statuses: &[EarlierStatus],
    name: &str,
) {
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

#[expect(clippy::too_many_arguments, reason = "internal review publisher")]
pub(super) fn publish_review(
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
    let r = &prepared.candidate.pull;
    let head = &prepared.pull.head_sha;
    let short_sha = short(head);
    let raw_findings = match raw.get("findings") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let numbered = next_finding_number(&saved.findings, &short_sha);
    if prepared.dry_run {
        return publish_dry_run(deps, prepared, result, raw, &raw_findings, plan, &short_sha, numbered);
    }
    let id = post_review_with_fallback(deps, github, r, head, result, plan, me, name)?;
    let mut posted = Vec::new();
    let entries = track_plan_findings(result, &raw_findings, &short_sha, head, numbered, plan, &mut posted);
    if !plan.comments.is_empty() {
        reconcile_inline_comments(deps, github, r, head, id, plan, &entries, &mut posted, name);
    }
    if deps.config.learn && !earlier.is_empty() {
        learn_earlier_fates(deps, github, r, saved, earlier, statuses, name);
    }
    Ok(PublishOutcome {
        posted,
        review_id: Some(id),
        path: None,
    })
}

pub(super) fn write_posted_review_record(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    plan: &ReviewPlan,
    published: &PublishOutcome,
    raw: &Value,
    promotion: Option<&PromotionDelta>,
    name: &str,
) {
    let (account, r) = (&prepared.candidate.account, &prepared.candidate.pull);
    let head = &prepared.pull.head_sha;
    let short_sha = short(head);
    let mut record = Map::new();
    record.insert("account".into(), json!(account_key(account)));
    record.insert("pull".into(), json!(pull_key(r)));
    record.insert("sha".into(), json!(head));
    record.insert("reviewId".into(), json!(published.review_id));
    record.insert(
        "review".into(),
        serde_json::to_value(review_input(head, plan)).unwrap_or(Value::Null),
    );
    record.insert(
        "posted".into(),
        serde_json::to_value(&published.posted).unwrap_or(Value::Null),
    );
    record.insert("result".into(), raw.clone());
    if let Some(delta) = promotion {
        record.insert(
            "promotion".into(),
            json!({
                "sources": delta.sources,
                "carriedLines": delta.carried_lines,
                "carriedFiles": delta.carried_files,
                "contextFiles": delta.context_files,
                "missing": delta.missing
            }),
        );
    }
    let name_of = format!("{}-{}-{}-{short_sha}-{}", r.owner, r.repo, r.number, account.login);
    if let Err(error) = write_review_record(&deps.paths.dir, &name_of, &Value::Object(record)) {
        (deps.log)(&format!("{name}: the review record was not written: {error}"));
    }
}

fn apply_earlier_statuses(findings: &mut [PostedFinding], statuses: &[EarlierStatus]) {
    for status in statuses {
        let Some(finding) = findings.iter_mut().find(|item| item.id == status.id) else {
            continue;
        };
        if status.status == "fixed" || status.status == "not_applicable" {
            finding.status = status.status.clone();
        } else {
            finding.line = status.line;
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "internal pull state updater")]
pub(super) fn persist_review_state(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    key: &str,
    result: &EngineResult,
    raw: &Value,
    plan: &ReviewPlan,
    published: &PublishOutcome,
    statuses: &[EarlierStatus],
    reviewed_at: &str,
    reviewed_at_ms: i64,
    ack_to_post_ms: Option<i64>,
) -> Result<(), Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let head = &prepared.pull.head_sha;
    let dry_run = prepared.dry_run;
    let verdict = plan.verdict.name().to_string();
    let stages: indexmap::IndexMap<String, Value> = match raw.get("timing").and_then(|timing| timing.get("stages")) {
        Some(Value::Object(stages)) => stages.clone().into_iter().collect(),
        _ => indexmap::IndexMap::new(),
    };
    deps.store.update_pull(key, |pull_state, all| {
        pull_state.last_reviewed_sha = Some(head.clone());
        pull_state.last_reviewed_at = Some(reviewed_at.to_string());
        pull_state.last_verdict = Some(verdict.clone());
        pull_state.last_review_complete = Some(result.complete || prepared.recheck_only);
        pull_state.closed = None;
        pull_state.last_review_dry_run = dry_run.then_some(true);
        if let Some(id) = published.review_id {
            pull_state.last_review_id = Some(id);
        }
        pull_state.attempt_succeeded(head);
        apply_earlier_statuses(&mut pull_state.findings, statuses);
        pull_state.findings.extend(published.posted.iter().cloned());
        if pull_state.findings.len() > 200 {
            let excess = pull_state.findings.len() - 200;
            pull_state.findings.drain(..excess);
        }
        let now_ms = (deps.now)();
        let mut stages = stages.clone();
        stages.insert("postMs".into(), json!(Num(((now_ms - reviewed_at_ms).max(0)) as f64)));
        all.recent.push(ReviewRecord {
            account: account_key(account),
            pull: pull_key(r),
            sha: head.clone(),
            at: reviewed_at.to_string(),
            outcome: if dry_run { "dry-run" } else { "posted" }.into(),
            verdict: Some(verdict.clone()),
            findings: Some(Num(result.findings.len() as f64)),
            total_ms: Some(Num(result.timing.total_ms)),
            stages: Some(stages),
            pickup_to_post_ms: Some(Num((now_ms - candidate.picked_at) as f64)),
            tag_to_ack_ms: prepared.tag_to_ack_ms.map(|ms| Num(ms as f64)),
            ack_to_post_ms: ack_to_post_ms.map(|ms| Num(ms as f64)),
            cost_usd: Some(Num(result.usage.cost_usd)),
            dry_run: dry_run.then_some(true),
            extra: Default::default(),
        });
    })
}

#[expect(clippy::too_many_arguments, reason = "internal post-review thread resolver")]
pub(super) fn finalize_posted_threads(
    deps: &ReviewerDeps,
    github: &GitHub,
    prepared: &Prepared,
    saved: &PullState,
    plan: &ReviewPlan,
    statuses: &[EarlierStatus],
    state: &str,
    me: &str,
    name: &str,
) -> Vec<String> {
    if prepared.dry_run {
        return Vec::new();
    }
    let r = &prepared.candidate.pull;
    let head = &prepared.pull.head_sha;
    if plan.verdict == Verdict::Comment && state == "open" && prepared.pull.author.to_lowercase() != me {
        lift_own_blocks(deps, github, r, me, head, name);
    }
    let mut resolved = Vec::new();
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
    resolved
}
