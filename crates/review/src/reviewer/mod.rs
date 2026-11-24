//! One pull request, one account (`autoreview/reviewer.ts`): decide whether a review is due, acknowledge, check the
//! source out, run the engine, and post the review (or write it to the dry-run directory). Everything read from the
//! pull request (description, comments, the mention itself, repository files) is data for the review, never
//! instructions.
//!
//! The engine is a [`ReviewEngine`] (the coordinator serves it with the kernel-backed engine); the lesson memory a
//! [`LessonMemory`] ([`crate::lessons::HindsightLessons`]); building a test environment an optional
//! [`RunnerPreparer`] (`prepareEnvironment`, which becomes a host job).

mod knowledge;
mod run;

pub use knowledge::{
    BACKFILL_PULLS, LessonMemory, MISS_WINDOW, classify_notes, finding_rules, index_knowledge, record_notes, repo_key,
};
pub use run::{MAX_REPLY_CHARS, MAX_REVIEW_RECORDS, run_review};

use std::sync::Arc;

use regex::Regex;
use serde_json::Value;

use crate::accounts::Account;
use crate::accounts::TokenStore;
use crate::ban::{Bans, banned_reason, refusal};
use crate::checkout::CheckoutManager;
use crate::engine::{Log, ReviewEngine};
use crate::github::{Clock, Comment, Error, GitHub, PullRef, PullRequest, Review, pull_key};
use crate::ledger::{FateInput, fates_of};
use crate::plan::ReviewPlan;
use crate::prepare_env::{EnvLease, RunnerPreparer};
use crate::runner::Runner;
use crate::settings::{AutoreviewConfig, AutoreviewPaths, MAX_ATTEMPTS};
use crate::state::{HeadAttempts, PostedFinding, PullState, StateStore};
use knowledge::learn;
use run::thread_states;

/// A prepared test environment (`prepareEnvironment`'s result).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedEnvironment {
    pub hash: String,
    pub cached: bool,
    pub dir: String,
    pub prepared: Vec<String>,
    pub failures: Vec<String>,
    /// The toolchain entries' paths.
    pub toolchain: Vec<String>,
    /// Held while the review uses the environment: it is not rebuilt under the review's tests.
    pub lease: EnvLease,
}

#[derive(Clone)]
pub struct ReviewerDeps {
    pub runner: Arc<dyn Runner>,
    pub tokens: Arc<TokenStore>,
    pub engine: Arc<dyn ReviewEngine>,
    pub store: Arc<StateStore>,
    pub checkouts: Arc<CheckoutManager>,
    pub config: AutoreviewConfig,
    pub paths: AutoreviewPaths,
    pub log: Log,
    pub now: Clock,
    /// A number in [0, 1): picks the acknowledgement line.
    pub random: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub lessons: Option<Arc<dyn LessonMemory>>,
    /// Builds a test environment for a worktree (with the network, before the review).
    pub environments: Option<Arc<RunnerPreparer>>,
    /// The home directory `~` in guide paths and checkout roots expands to.
    pub home: String,
    /// `autoreview.ban`, re-read before each decision and model turn.
    pub bans: Arc<Bans>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub account: Account,
    pub pull: PullRef,
    /// Why discovery picked it: notification reasons and search names.
    pub reasons: Vec<String>,
    /// When discovery found it (epoch ms).
    pub picked_at: i64,
    /// When GitHub last updated the notification that led here (epoch ms); absent for a search result.
    pub notified_at: Option<i64>,
}

/// A posted (or dry-run) review.
#[derive(Debug, Clone, PartialEq)]
pub struct Reviewed {
    pub verdict: &'static str,
    pub sha: String,
    pub result: Value,
    pub plan: ReviewPlan,
    pub review_id: Option<i64>,
    /// The dry-run files' path without extension.
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Skipped(String),
    Requeue(String),
    GaveUp(String),
    Posted(Box<Reviewed>),
    DryRun(Box<Reviewed>),
}

impl Outcome {
    /// The TypeScript `kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Skipped(_) => "skipped",
            Outcome::Requeue(_) => "requeue",
            Outcome::GaveUp(_) => "gave-up",
            Outcome::Posted(_) => "posted",
            Outcome::DryRun(_) => "dry-run",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Outcome::Skipped(reason) | Outcome::Requeue(reason) | Outcome::GaveUp(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn reviewed(&self) -> Option<&Reviewed> {
        match self {
            Outcome::Posted(reviewed) | Outcome::DryRun(reviewed) => Some(reviewed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewOptions {
    /// Review even when nothing new asks for it (`ultron autoreview review`).
    pub force: bool,
    pub dry_run: Option<bool>,
}

pub fn pull_state_key(account: &Account, r: &PullRef) -> String {
    format!("{}@{}", account.login, pull_key(r))
}

/// Whether `body` @mentions `login` (not as part of a longer name or an email address).
pub fn mentions(body: &str, login: &str) -> bool {
    let Ok(at) = Regex::new(&format!("(?i)^@{}", regex::escape(login))) else {
        return false;
    };
    let before_ok = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '-'));
    let after_ok = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '/'));
    body.char_indices().filter(|(_, c)| *c == '@').any(|(index, _)| {
        let Some(found) = at.find(&body[index..]) else {
            return false;
        };
        let before = body[..index].chars().next_back();
        let after = body[index + found.end()..].chars().next();
        before.is_none_or(before_ok) && after.is_none_or(after_ok)
    })
}

/// When the account was last @mentioned on the pull request by somebody else (ISO time), if ever.
pub fn latest_mention(login: &str, pull: &PullRequest, comments: &[Comment], reviews: &[Review]) -> Option<String> {
    latest_mention_except(login, pull, comments, reviews, &|_| false)
}

/// [`latest_mention`] without the mentions written by those `ignored` names (banned logins).
pub fn latest_mention_except(
    login: &str,
    pull: &PullRequest,
    comments: &[Comment],
    reviews: &[Review],
    ignored: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let me = login.to_lowercase();
    let mut latest: Option<String> = None;
    let mut consider = |author: &str, body: &str, at: &str| {
        if author.to_lowercase() == me || at.is_empty() || !mentions(body, login) || ignored(author) {
            return;
        }
        if latest
            .as_deref()
            .is_none_or(|latest| jsrt::utf16::cmp(at, latest).is_gt())
        {
            latest = Some(at.into());
        }
    };
    consider(&pull.author, &pull.body, &pull.created_at);
    for comment in comments {
        consider(&comment.user, &comment.body, &comment.updated_at);
    }
    for review in reviews {
        consider(&review.user, &review.body, &review.submitted_at);
    }
    latest
}

pub struct DecisionInput<'a> {
    pub pull: &'a PullRequest,
    /// The commit of this account's last review, and when it was made.
    pub last_sha: Option<String>,
    pub last_at: Option<String>,
    /// That last review requested changes: new commits are reviewed so the block is lifted or confirmed.
    pub last_requested_changes: bool,
    pub mention_at: Option<String>,
    /// A review by this account is requested now: directly, or through a requested team it belongs to.
    pub requested: bool,
    /// `autoreview.skipBots`: a bot's pull request is reviewed only when the account is @mentioned.
    pub skip_bots: bool,
    /// When the review was last requested; asked only when the head was already reviewed.
    pub requested_at: &'a dyn Fn() -> Result<Option<String>, Error>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Decision {
    pub review: bool,
    pub reason: String,
}

fn decision(review: bool, reason: impl Into<String>) -> Decision {
    Decision {
        review,
        reason: reason.into(),
    }
}

pub(super) fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Whether a review is due. A review ends the account's part unless something asks again: a pending request, a
/// re-request, a new mention, or (because this account then blocks the merge) its own request for changes.
pub fn decide(input: &DecisionInput) -> Result<Decision, Error> {
    let pull = input.pull;
    let later = |a: &str, b: &str| jsrt::utf16::cmp(a, b).is_gt();
    let new_mention = input
        .mention_at
        .as_deref()
        .is_some_and(|at| input.last_at.as_deref().is_none_or(|last| later(at, last)));
    let ended = if pull.merged { "merged" } else { "closed" };
    if pull.state != "open" {
        return Ok(if new_mention {
            decision(true, format!("mentioned on a {ended} pull request"))
        } else {
            decision(false, format!("the pull request is {ended}"))
        });
    }
    if pull.draft && !new_mention && !input.requested {
        return Ok(decision(false, "the pull request is a draft"));
    }
    if input.skip_bots && pull.author_is_bot && !new_mention {
        return Ok(decision(
            false,
            format!("the author {} is a bot (autoreview.skipBots)", pull.author),
        ));
    }
    let Some(last_sha) = &input.last_sha else {
        if input.requested {
            return Ok(decision(true, "review requested"));
        }
        if new_mention {
            return Ok(decision(true, "mentioned"));
        }
        return Ok(decision(false, "no review request or mention for this account"));
    };
    if new_mention {
        return Ok(decision(true, "mentioned again"));
    }
    if *last_sha != pull.head_sha {
        let since = format!("new commits since {}", short(last_sha));
        if input.requested {
            return Ok(decision(true, format!("{since}, review requested")));
        }
        if input.last_requested_changes {
            return Ok(decision(true, format!("{since}, after this account requested changes")));
        }
        return Ok(decision(
            false,
            format!(
                "{} was reviewed and nothing asks for a review of the new commits",
                short(last_sha)
            ),
        ));
    }
    if input.requested {
        let requested_at = (input.requested_at)()?;
        if let (Some(requested_at), Some(last_at)) = (&requested_at, &input.last_at)
            && later(requested_at, last_at)
        {
            return Ok(decision(true, "review requested again"));
        }
    }
    Ok(decision(
        false,
        format!(
            "{} is already reviewed and nothing new asks for it",
            short(&pull.head_sha)
        ),
    ))
}

/// GitHub rejects a comment body over 65,536 characters; the acknowledgement stays well under it.
pub const MAX_ACK_CHARS: usize = 60_000;

/// The acknowledgement posted when a review starts: the line as a quotation with its attribution, the commit, and
/// (with `autoreview.ackArt`) the art in a fenced block longer than any backtick run in the art; art that would take
/// the comment past the size limit is left out.
pub fn ack_body(line: &str, sha: &str, art: &str) -> String {
    let quote = jsrt::string::flatten(line);
    let body = format!("> *{quote}*\n> — Ultron\n\nReviewing `{}`.", short(sha));
    let art = art.replace("\r\n", "\n");
    let banner = art.trim_start_matches('\n').trim_end_matches(jsrt::string::is_space);
    if banner.is_empty() {
        return body;
    }
    let longest = Regex::new("`+")
        .expect("static pattern")
        .find_iter(banner)
        .map(|run| run.as_str().len())
        .fold(2, usize::max);
    let fence = "`".repeat(longest + 1);
    let full = format!("{body}\n\n{fence}text\n{banner}\n{fence}");
    if jsrt::utf16::len(&full) <= MAX_ACK_CHARS {
        full
    } else {
        body
    }
}

/// A line from `lines`, at random, other than the one used last on this pull request.
pub fn pick_ack_line(lines: &[String], previous: Option<&str>, random: f64) -> String {
    let choices: Vec<&String> = if lines.len() > 1 {
        lines.iter().filter(|line| Some(line.as_str()) != previous).collect()
    } else {
        lines.iter().collect()
    };
    if choices.is_empty() {
        return String::new();
    }
    let index = ((random * choices.len() as f64).floor().max(0.0) as usize).min(choices.len() - 1);
    choices[index].clone()
}

fn latest_own_review<'a>(reviews: &'a [Review], login: &str) -> Option<&'a Review> {
    let me = login.to_lowercase();
    let mut latest: Option<&Review> = None;
    for review in reviews {
        if review.user.to_lowercase() != me || review.state == "PENDING" || review.submitted_at.is_empty() {
            continue;
        }
        if latest.is_none_or(|latest| jsrt::utf16::cmp(&review.submitted_at, &latest.submitted_at).is_gt()) {
            latest = Some(review);
        }
    }
    latest
}

/// The dry-run markdown file.
pub fn dry_run_markdown(candidate: &Candidate, sha: &str, plan: &ReviewPlan, ack: Option<&str>) -> String {
    let mut lines = vec![
        format!(
            "# Would-be review of {} at {} as {}",
            pull_key(&candidate.pull),
            short(sha),
            candidate.account.login
        ),
        String::new(),
        format!("Event: {}", plan.event),
        String::new(),
    ];
    if let Some(ack) = ack {
        lines.extend([
            "## Acknowledgement comment".into(),
            String::new(),
            ack.into(),
            String::new(),
        ]);
    }
    lines.extend(["## Summary".into(), String::new(), plan.body.clone(), String::new()]);
    if !plan.comments.is_empty() {
        lines.extend(["## Inline comments".into(), String::new()]);
    }
    for comment in &plan.comments {
        let place = match comment.start_line {
            None => format!("{}:{}", comment.path, comment.line),
            Some(start) => format!("{}:{start}-{}", comment.path, comment.line),
        };
        lines.extend([
            format!("### {place}"),
            String::new(),
            comment.body.clone(),
            String::new(),
        ]);
    }
    format!("{}\n", lines.join("\n"))
}

/// A pull request whose review is due: what the decision read, and the acknowledgement.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub candidate: Candidate,
    pub dry_run: bool,
    pub pull: PullRequest,
    pub reason: String,
    pub last_sha: Option<String>,
    /// The acknowledgement comment (posted, or in a dry run only written to the dry-run file).
    pub ack: Option<String>,
    /// When the acknowledgement for this head commit was posted (epoch ms).
    pub ack_at: Option<i64>,
    /// Milliseconds from the notification to the acknowledgement, when both are known and it was posted now.
    pub tag_to_ack_ms: Option<i64>,
    /// The head was reviewed in full before and is asked about again: only the earlier findings are re-checked,
    /// with the replies in their threads, and nothing new is looked for.
    pub recheck_only: bool,
    /// Asked for on the command line: every line is in play, as in a first review.
    pub forced: bool,
}

pub(super) fn seconds(ms: i64) -> String {
    format!("{} s", jsrt::number::to_fixed(ms as f64 / 1000.0, 1))
}

pub(super) fn github_of(deps: &ReviewerDeps, account: &Account) -> GitHub {
    let tokens = deps.tokens.clone();
    let owner = account.clone();
    GitHub::new(
        deps.runner.clone(),
        account.clone(),
        Arc::new(move || tokens.env(&owner)),
        Some(deps.now.clone()),
    )
}

/// Either a prepared review or an outcome that ends here.
#[derive(Debug, Clone, PartialEq)]
pub enum Preparation {
    Due(Box<Prepared>),
    Done(Outcome),
}

/// Decide whether a pull request is due for a review by an account and, if so, acknowledge it at once (one comment
/// per pull request and head commit; none on a closed or merged pull request). The review itself is [`run_review`].
pub fn prepare_pull(deps: &ReviewerDeps, candidate: &Candidate, options: ReviewOptions) -> Result<Preparation, Error> {
    let (account, r) = (&candidate.account, &candidate.pull);
    let dry_run = options.dry_run.unwrap_or(deps.config.dry_run);
    let github = github_of(deps, account);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);

    let pull = github.pull(r)?;
    if pull.head_sha.is_empty() {
        return Err(Error::GitHub {
            message: format!("{name}: the pull request has no head commit"),
            status: 0,
        });
    }
    let bans = deps.bans.refresh();
    if let Some(rule) = bans.banned(account, &pull.author) {
        if options.force {
            return Err(Error::Other(refusal(&name, &pull.author, rule)));
        }
        return Ok(Preparation::Done(Outcome::Skipped(banned_reason(&pull.author))));
    }
    let head = pull.head_sha.clone();
    let reviews = github.reviews(r)?;
    let saved: PullState = deps.store.read().pulls.get(&key).cloned().unwrap_or_default();
    let saved_counts = saved.last_reviewed_sha.is_some() && (dry_run || saved.last_review_dry_run != Some(true));
    let own = latest_own_review(&reviews, &account.login);
    let mut last_sha = own.map(|review| review.commit_id.clone());
    let mut last_at = own.map(|review| review.submitted_at.clone());
    let mut last_requested_changes = own.is_some_and(|review| review.state == "CHANGES_REQUESTED");
    let saved_at = saved.last_reviewed_at.clone().unwrap_or_default();
    if saved_counts
        && last_at
            .as_deref()
            .is_none_or(|at| jsrt::utf16::cmp(&saved_at, at).is_gt())
    {
        last_sha = saved.last_reviewed_sha.clone();
        last_at = saved.last_reviewed_at.clone();
        last_requested_changes = saved.last_verdict.as_deref() == Some("request_changes");
    }

    let me = account.login.to_lowercase();
    let mut requested = pull.requested_reviewers.iter().any(|login| login.to_lowercase() == me);
    let gave_up = saved.head_attempts(&head) == HeadAttempts::GaveUp;
    let mut mention_at = None;
    let decided = if options.force {
        decision(true, "asked for on the command line")
    } else {
        let mut through_team = None;
        if !requested && pull.state == "open" && !pull.draft {
            for team in &pull.requested_teams {
                let member = github.in_team(&r.owner, team)?;
                if member.is_none() {
                    (deps.log)(&format!(
                        "{name}: membership of the requested team {}/{team} cannot be read; not counted",
                        r.owner
                    ));
                }
                if member == Some(true) {
                    requested = true;
                    through_team = Some(team.clone());
                    break;
                }
            }
        }
        let banned = |login: &str| bans.banned(account, login).is_some();
        let mut ignored = Vec::new();
        if requested
            && bans.applies_to(account)
            && let Some((_, actor)) = github.last_review_requester(r, through_team.as_deref())?
            && banned(&actor)
        {
            requested = false;
            ignored.push(format!("the review request by {actor}"));
        }
        if gave_up || !(requested && last_sha.is_none() && pull.state == "open" && !pull.draft) {
            let mut comments = github.issue_comments(r)?;
            comments.extend(github.review_comments(r)?);
            mention_at = latest_mention_except(&account.login, &pull, &comments, &reviews, &banned);
            if bans.applies_to(account)
                && let Some(any) = latest_mention(&account.login, &pull, &comments, &reviews)
                && mention_at
                    .as_deref()
                    .is_none_or(|kept| jsrt::utf16::cmp(&any, kept).is_gt())
            {
                ignored.push("a mention by a banned login".into());
            }
        }
        let requested_at = || github.last_review_request_at(r);
        let mut decided = decide(&DecisionInput {
            pull: &pull,
            last_sha: last_sha.clone(),
            last_at: last_at.clone(),
            last_requested_changes,
            mention_at: mention_at.clone(),
            requested,
            skip_bots: deps.config.skip_bots,
            requested_at: &requested_at,
        })?;
        let has_new_mention = mention_at
            .as_deref()
            .is_some_and(|m| last_at.as_deref().is_none_or(|l| m > l));
        if decided.review && deps.config.quiet_minutes.0 > 0.0 && !has_new_mention {
            let quiet_ms = deps.config.quiet_minutes.0 * 60_000.0;
            let last_activity =
                jsrt::date::parse_date(&pull.updated_at).or_else(|| jsrt::date::parse_date(&pull.created_at));
            if let Some(at) = last_activity
                && (jsrt::date::now_ms() as f64 - at as f64) < quiet_ms
            {
                decided.review = false;
                decided.reason = format!(
                    "waiting for quiet window ({} min since last update, autoreview.quietMinutes)",
                    deps.config.quiet_minutes.0 as u64
                );
            }
        }
        if !decided.review && !ignored.is_empty() {
            decided.reason = format!("{}; ignored {} (autoreview.ban)", decided.reason, ignored.join(" and "));
        }
        decided
    };
    if !decided.review {
        if pull.state != "open" && saved.last_verdict.is_some() {
            if saved.closed != Some(true)
                && deps.config.learn
                && saved.findings.iter().any(|finding| finding.status == "open")
            {
                let learned = (|| {
                    let threads = if pull.merged {
                        Some(github.review_threads(r)?)
                    } else {
                        None
                    };
                    let fates = fates_of(&FateInput {
                        pull: pull_key(r),
                        now: jsrt::date::iso_string((deps.now)()),
                        findings: saved.findings.iter().map(PostedFinding::ledger).collect(),
                        threads: threads.map_or_default(thread_states),
                        merged: pull.merged,
                        ..FateInput::default()
                    });
                    learn(
                        deps,
                        r,
                        &fates,
                        &saved.findings,
                        &format!("{} as {}", pull_key(r), account.login),
                    )
                })();
                if let Err(error) = learned {
                    (deps.log)(&format!("{}: the fates at close were not read: {error}", pull_key(r)));
                }
            }
            if saved.closed != Some(true) && deps.config.learn {
                record_notes(deps, r, &github, &format!("{} as {}", pull_key(r), account.login));
            }
            deps.store.update_pull(&key, |state, _| state.closed = Some(true))?;
        }
        return Ok(Preparation::Done(Outcome::Skipped(decided.reason)));
    }

    let mut attempts = saved.head_attempts(&head);
    if !options.force
        && attempts == HeadAttempts::GaveUp
        && deps
            .store
            .update_pull(&key, |state, _| state.retry_after_mention(&head, mention_at.as_deref()))?
    {
        (deps.log)(&format!(
            "{name}: mentioned after giving up on {}; trying again",
            short(&head)
        ));
        attempts = HeadAttempts::Fresh;
    }
    if !options.force && attempts == HeadAttempts::GaveUp {
        return Ok(Preparation::Done(Outcome::Skipped(
            "gave up on this commit earlier".into(),
        )));
    }
    if !options.force && matches!(attempts, HeadAttempts::Failed(failed) if failed >= MAX_ATTEMPTS) {
        if !dry_run {
            github.post_comment(
                r,
                &format!(
                    "I could not review `{}` after {MAX_ATTEMPTS} attempts. Push a new commit or mention me again to retry.",
                    short(&head)
                ),
            )?;
        }
        let at = jsrt::date::iso_string((deps.now)());
        deps.store.update_pull(&key, |state, _| state.gave_up(&head, at))?;
        (deps.log)(&format!(
            "{name}: gave up on {} after {MAX_ATTEMPTS} attempts",
            short(&head)
        ));
        return Ok(Preparation::Done(Outcome::GaveUp(format!(
            "{MAX_ATTEMPTS} attempts failed"
        ))));
    }

    let mut ack = None;
    let mut ack_at = if saved.last_ack_sha.as_deref() == Some(head.as_str()) {
        saved.last_ack_at.as_deref().and_then(jsrt::date::parse_date)
    } else {
        None
    };
    let mut tag_to_ack_ms = None;
    if deps.config.ack
        && !deps.config.ack_lines.is_empty()
        && saved.last_ack_sha.as_deref() != Some(head.as_str())
        && pull.state == "open"
    {
        let line = pick_ack_line(&deps.config.ack_lines, saved.last_ack_line.as_deref(), (deps.random)());
        let body = ack_body(&line, &head, &deps.config.ack_art);
        if !dry_run {
            let posted = github.post_comment(r, &body).and_then(|_| {
                let posted = (deps.now)();
                ack_at = Some(posted);
                if let Some(notified) = candidate.notified_at {
                    tag_to_ack_ms = Some((posted - notified).max(0));
                }
                deps.store.update_pull(&key, |state, _| {
                    state.last_ack_sha = Some(head.clone());
                    state.last_ack_line = Some(line.clone());
                    state.last_ack_at = Some(jsrt::date::iso_string(posted));
                })?;
                (deps.log)(&format!(
                    "{name}: acknowledged {} ({}){}",
                    short(&head),
                    decided.reason,
                    tag_to_ack_ms.map_or(String::new(), |ms| format!(", {} after the notification", seconds(ms)))
                ));
                Ok(())
            });
            match posted {
                Err(error @ Error::RateLimit { .. }) => return Err(error),
                Err(error) => (deps.log)(&format!("{name}: the acknowledgement was not posted: {error}")),
                Ok(()) => {}
            }
        }
        ack = Some(body);
    }
    let recheck_only = !options.force
        && last_sha.as_deref() == Some(head.as_str())
        && saved.last_reviewed_sha.as_deref() == Some(head.as_str())
        && saved.last_review_complete == Some(true);
    Ok(Preparation::Due(Box::new(Prepared {
        candidate: candidate.clone(),
        dry_run,
        pull,
        reason: decided.reason,
        last_sha,
        ack,
        ack_at,
        tag_to_ack_ms,
        recheck_only,
        forced: options.force,
    })))
}

/// Review one pull request as one account, if a review is due: the acknowledgement, then the review.
pub fn review_pull(deps: &ReviewerDeps, candidate: &Candidate, options: ReviewOptions) -> Result<Outcome, Error> {
    match prepare_pull(deps, candidate, options)? {
        Preparation::Done(outcome) => Ok(outcome),
        Preparation::Due(prepared) => run_review(deps, &prepared),
    }
}
