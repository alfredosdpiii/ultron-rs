//! GitHub as autoreview uses it (`autoreview/github.ts`): REST and GraphQL through `gh api -i`, authenticated per
//! account by the child's environment. Responses are requested with their headers, so conditional requests (ETag,
//! If-Modified-Since), `X-Poll-Interval` and rate limits (`Retry-After`, `X-RateLimit-Reset`) are honoured.
//!
//! Known differences from the TypeScript, outside recorded behaviour:
//! - Ids (`Number(item.id)`) that are not integers read as 0 (JavaScript kept NaN or the fraction); a pull request
//!   number past `u64` is not a pull request reference.
//! - A `null` entry in `requested_teams` or `errors` reads as empty (JavaScript threw a TypeError).
//! - Request bodies are written by serde_json (JavaScript's JSON.stringify differs only for non-integral numbers).

mod types;

pub use types::{
    ApiOutput, ApiResponse, Clock, Comment, EnvSource, Error, NOTIFICATION_REASONS, NotificationPage, NotifiedPull,
    PullRef, PullRequest, RequestOptions, Review, ReviewCommentInput, ReviewInput, ReviewThread, id_of,
    js_value_number, parse_api_output, parse_pull_target, pull_key, pull_ref_from_url, text,
};

use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::accounts::Account;
#[cfg(test)]
use crate::runner::Env;
use crate::runner::{RunOptions, Runner};
use types::{SleepFn, items, login};

/// Backoff delays (ms) for transient `GET` transport/gateway failures; covers the 0-3 s connect blip window.
const TRANSIENT_BACKOFF_MS: [u64; 3] = [500, 1_500, 3_000];

fn is_transient_failure(method: &str, error: &Error) -> bool {
    if !method.eq_ignore_ascii_case("GET") {
        return false;
    }
    match error {
        Error::GitHub { status: 0, message } => message.contains("error connecting to"),
        Error::GitHub { status: 502..=504, .. } => true,
        _ => false,
    }
}

/// One account's view of GitHub.
#[derive(Clone)]
pub struct GitHub {
    pub account: Account,
    runner: Arc<dyn Runner>,
    env: EnvSource,
    now: Clock,
    sleep: SleepFn,
}

const THREADS_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!,$after:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}nodes{id isResolved comments(first:1){totalCount nodes{databaseId reactionGroups{content reactors{totalCount}}}}}}}}}";
/// The review threads as notes for the knowledge graph: who said what where, and whether it was addressed. Its own
/// query: [`THREADS_QUERY`] is the TypeScript reviewer's, replayed from fixtures.
const NOTES_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!,$after:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}nodes{isResolved isOutdated path line originalLine comments(first:1){nodes{author{__typename login} body url createdAt}}}}}}}";
const RESOLVE_MUTATION: &str =
    "mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}";

impl GitHub {
    pub fn new(runner: Arc<dyn Runner>, account: Account, env: EnvSource, now: Option<Clock>) -> Self {
        Self {
            account,
            runner,
            env,
            now: now.unwrap_or_else(|| Arc::new(jsrt::date::now_ms)),
            sleep: Arc::new(|ms| std::thread::sleep(Duration::from_millis(ms))),
        }
    }

    fn base(r: &PullRef) -> String {
        format!("repos/{}/{}", r.owner, r.repo)
    }

    /// One API request, retrying transient `GET` connect and 502/503/504 failures with bounded backoff.
    /// Fails with [`Error::RateLimit`] when GitHub says to wait, [`Error::GitHub`] otherwise.
    pub fn request(&self, method: &str, path: &str, options: &RequestOptions) -> Result<ApiResponse, Error> {
        let mut argv: Vec<String> = [
            "gh",
            "api",
            "-i",
            "--method",
            method,
            "--hostname",
            &self.account.host,
            path,
        ]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
        for (name, value) in &options.headers {
            argv.push("-H".into());
            argv.push(format!("{name}: {value}"));
        }
        if options.body.is_some() {
            argv.push("--input".into());
            argv.push("-".into());
        }
        let run = RunOptions {
            env: (self.env)()?,
            input: options.body.as_ref().map(std::string::ToString::to_string),
            ..RunOptions::default()
        };
        for attempt in 0..=TRANSIENT_BACKOFF_MS.len() {
            match self.request_once(method, path, options, &argv, &run) {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let Some(&delay_ms) = TRANSIENT_BACKOFF_MS.get(attempt) else {
                        return Err(error);
                    };
                    if !is_transient_failure(method, &error) {
                        return Err(error);
                    }
                    (self.sleep)(delay_ms);
                }
            }
        }
        unreachable!("retry loop always returns on the final attempt")
    }

    fn request_once(
        &self,
        method: &str,
        path: &str,
        options: &RequestOptions,
        argv: &[String],
        run: &RunOptions,
    ) -> Result<ApiResponse, Error> {
        let result = self.runner.run(argv, run);
        let parsed = parse_api_output(&result.stdout);
        if parsed.status == 0 {
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            return Err(Error::GitHub {
                message: format!(
                    "gh api {method} {path} failed: {}",
                    jsrt::utf16::head(jsrt::string::trim(detail), 300)
                ),
                status: 0,
            });
        }
        let body = if jsrt::string::trim(&parsed.text).is_empty() {
            None
        } else {
            serde_json::from_str::<Value>(&parsed.text).ok()
        };
        let response = ApiResponse {
            status: parsed.status,
            headers: parsed.headers,
            text: parsed.text,
            body,
        };
        if response.status < 400 || options.allow.contains(&response.status) {
            return Ok(response);
        }
        let message = Some(text(response.body.as_ref().and_then(|body| body.get("message"))))
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("HTTP {}", response.status));
        if let Some(resume_at) = self.resume_at(response.status, &response.headers, &message) {
            return Err(Error::RateLimit {
                message: format!("GitHub rate limit: {message}"),
                resume_at,
            });
        }
        let full = format!("GitHub {method} {path}: {} {message}", response.status);
        Err(Error::GitHub {
            message: jsrt::utf16::head(&full, 400).to_string(),
            status: response.status,
        })
    }

    fn header_resume_at(headers: &IndexMap<String, String>, now: f64) -> Option<f64> {
        let header = |name: &str| headers.get(name).map_or(f64::NAN, |value| jsrt::number::parse(value));
        let retry_after = header("retry-after");
        if retry_after.is_finite() && retry_after > 0.0 {
            return Some(now + retry_after * 1000.0);
        }
        if headers.get("x-ratelimit-remaining").map(String::as_str) == Some("0") {
            let reset = header("x-ratelimit-reset");
            if reset.is_finite() && reset > 0.0 {
                return Some((now + 1000.0).max(reset * 1000.0));
            }
        }
        None
    }

    /// When a 403 or 429 is a rate limit: the time to resume at.
    fn resume_at(&self, status: u16, headers: &IndexMap<String, String>, message: &str) -> Option<f64> {
        if status != 403 && status != 429 {
            return None;
        }
        let now = (self.now)() as f64;
        if let Some(at) = Self::header_resume_at(headers, now) {
            return Some(at);
        }
        let lower = message.to_lowercase();
        if status == 429 || lower.contains("rate limit") || lower.contains("abuse") {
            return Some(now + 60_000.0);
        }
        None
    }

    fn get(&self, path: &str) -> Result<ApiResponse, Error> {
        self.request("GET", path, &RequestOptions::default())
    }

    fn post(&self, path: &str, body: Value) -> Result<ApiResponse, Error> {
        self.request(
            "POST",
            path,
            &RequestOptions {
                body: Some(body),
                ..RequestOptions::default()
            },
        )
    }

    /// Every page of a list endpoint (100 per page, at most `max_pages`).
    pub fn list(&self, path: &str, max_pages: u32) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        let separator = if path.contains('?') { "&" } else { "?" };
        for page in 1..=max_pages {
            let response = self.get(&format!("{path}{separator}per_page=100&page={page}"))?;
            let found = items(response.body.as_ref());
            let full = found.len() >= 100;
            out.extend(found);
            if !full {
                break;
            }
        }
        Ok(out)
    }

    /// Notifications about pull requests this account was asked to review or was mentioned on.
    pub fn notifications(
        &self,
        since: &str,
        last_modified: Option<&str>,
        etag: Option<&str>,
    ) -> Result<NotificationPage, Error> {
        let mut headers = Vec::new();
        if let Some(etag) = etag.filter(|etag| !etag.is_empty()) {
            headers.push(("If-None-Match".to_string(), etag.to_string()));
        } else if let Some(last) = last_modified.filter(|last| !last.is_empty()) {
            headers.push(("If-Modified-Since".to_string(), last.to_string()));
        }
        let response = self.request(
            "GET",
            &format!(
                "notifications?all=true&per_page=50&since={}",
                jsrt::uri::encode_component(since)
            ),
            &RequestOptions {
                headers,
                allow: vec![304],
                ..RequestOptions::default()
            },
        )?;
        let interval = response
            .headers
            .get("x-poll-interval")
            .map_or(f64::NAN, |v| jsrt::number::parse(v));
        let header = |name: &str| response.headers.get(name).filter(|value| !value.is_empty()).cloned();
        let poll_interval = (interval.is_finite() && interval > 0.0).then_some(jsrt::json::Num(interval));
        if response.status == 304 {
            return Ok(NotificationPage {
                not_modified: true,
                pulls: Vec::new(),
                last_modified: header("last-modified").or(last_modified.filter(|v| !v.is_empty()).map(String::from)),
                etag: header("etag").or(etag.filter(|v| !v.is_empty()).map(String::from)),
                poll_interval,
            });
        }
        let mut pulls = Vec::new();
        for item in items(response.body.as_ref()) {
            let subject = item.get("subject");
            let reason = text(item.get("reason"));
            if text(subject.and_then(|s| s.get("type"))) != "PullRequest"
                || !NOTIFICATION_REASONS.contains(&reason.as_str())
            {
                continue;
            }
            if let Some(pull) = pull_ref_from_url(&text(subject.and_then(|s| s.get("url"))), &self.account.host) {
                pulls.push(NotifiedPull {
                    pull,
                    reason,
                    updated_at: text(item.get("updated_at")),
                });
            }
        }
        Ok(NotificationPage {
            not_modified: false,
            pulls,
            last_modified: header("last-modified"),
            etag: header("etag"),
            poll_interval,
        })
    }

    /// Pull requests matching a search query.
    pub fn search(&self, query: &str) -> Result<Vec<PullRef>, Error> {
        Ok(self
            .search_with_authors(query)?
            .into_iter()
            .map(|(found, _)| found)
            .collect())
    }

    /// [`GitHub::search`] with each pull request's author (empty when the result names none).
    pub fn search_with_authors(&self, query: &str) -> Result<Vec<(PullRef, String)>, Error> {
        let response = self.get(&format!(
            "search/issues?per_page=50&sort=updated&order=desc&q={}",
            jsrt::uri::encode_component(query)
        ))?;
        let mut out = Vec::new();
        for item in items(response.body.as_ref().and_then(|body| body.get("items"))) {
            let url = format!(
                "{}/pulls/{}",
                text(item.get("repository_url")),
                jsrt::value::string(item.get("number"))
            );
            if let Some(found) = pull_ref_from_url(&url, &self.account.host)
                && item.get("pull_request").is_some()
            {
                out.push((found, login(item.get("user"))));
            }
        }
        Ok(out)
    }

    pub fn pull(&self, r: &PullRef) -> Result<PullRequest, Error> {
        let response = self.get(&format!("{}/pulls/{}", Self::base(r), r.number))?;
        let item = response.body.unwrap_or(json!({}));
        let head = item.get("head");
        let base = item.get("base");
        let author = login(item.get("user"));
        let bot = Regex::new(r"(?i)\[bot\]$").expect("static pattern");
        Ok(PullRequest {
            number: r.number,
            title: text(item.get("title")),
            body: text(item.get("body")),
            state: if item.get("state") == Some(&json!("open")) {
                "open"
            } else {
                "closed"
            }
            .into(),
            merged: item.get("merged") == Some(&Value::Bool(true))
                || matches!(item.get("merged_at"), Some(Value::String(_))),
            draft: item.get("draft") == Some(&Value::Bool(true)),
            author_is_bot: item.get("user").and_then(|user| user.get("type")) == Some(&json!("Bot"))
                || bot.is_match(&author),
            author,
            head_sha: text(head.and_then(|h| h.get("sha"))),
            base_sha: text(base.and_then(|b| b.get("sha"))),
            base_ref: text(base.and_then(|b| b.get("ref"))),
            url: text(item.get("html_url")),
            created_at: text(item.get("created_at")),
            updated_at: text(item.get("updated_at")),
            requested_reviewers: items(item.get("requested_reviewers"))
                .iter()
                .map(|v| login(Some(v)))
                .collect(),
            requested_teams: items(item.get("requested_teams"))
                .iter()
                .map(|team| text(team.get("slug")))
                .collect(),
            merge_commit_sha: Some(text(item.get("merge_commit_sha"))).filter(|sha| !sha.is_empty()),
            default_branch: text(
                base.and_then(|b| b.get("repo"))
                    .and_then(|repo| repo.get("default_branch")),
            ),
            additions: item.get("additions").and_then(Value::as_u64).unwrap_or(0),
            deletions: item.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    /// The pull request's diff as GitHub computes it (merge base to head).
    pub fn pull_diff(&self, r: &PullRef) -> Result<String, Error> {
        let response = self.request(
            "GET",
            &format!("{}/pulls/{}", Self::base(r), r.number),
            &RequestOptions {
                headers: vec![("Accept".into(), "application/vnd.github.v3.diff".into())],
                ..RequestOptions::default()
            },
        )?;
        Ok(response.text)
    }

    pub fn reviews(&self, r: &PullRef) -> Result<Vec<Review>, Error> {
        let found = self.list(&format!("{}/pulls/{}/reviews", Self::base(r), r.number), 5)?;
        Ok(found
            .iter()
            .map(|item| Review {
                id: id_of(item.get("id")),
                user: login(item.get("user")),
                state: text(item.get("state")),
                commit_id: text(item.get("commit_id")),
                submitted_at: text(item.get("submitted_at")),
                body: text(item.get("body")),
            })
            .collect())
    }

    fn comment(item: &Value) -> Comment {
        let number = |key: &str| match item.get(key) {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            _ => None,
        };
        let created = text(item.get("created_at"));
        let updated = text(item.get("updated_at"));
        Comment {
            id: id_of(item.get("id")),
            user: login(item.get("user")),
            body: text(item.get("body")),
            updated_at: if updated.is_empty() { created.clone() } else { updated },
            created_at: created,
            path: match item.get("path") {
                Some(Value::String(path)) => Some(path.clone()),
                _ => None,
            },
            line: number("line").or_else(|| number("original_line")),
            review_id: number("pull_request_review_id"),
            in_reply_to: number("in_reply_to_id"),
        }
    }

    /// Comments on the pull request's conversation.
    pub fn issue_comments(&self, r: &PullRef) -> Result<Vec<Comment>, Error> {
        let found = self.list(&format!("{}/issues/{}/comments", Self::base(r), r.number), 5)?;
        Ok(found.iter().map(Self::comment).collect())
    }

    /// Inline review comments on the pull request.
    pub fn review_comments(&self, r: &PullRef) -> Result<Vec<Comment>, Error> {
        let found = self.list(&format!("{}/pulls/{}/comments", Self::base(r), r.number), 5)?;
        Ok(found.iter().map(Self::comment).collect())
    }

    /// When this account was last asked to review the pull request (ISO time), from the issue's events.
    pub fn last_review_request_at(&self, r: &PullRef) -> Result<Option<String>, Error> {
        let events = self.list(&format!("{}/issues/{}/events", Self::base(r), r.number), 5)?;
        let me = self.account.login.to_lowercase();
        let mut latest: Option<String> = None;
        for event in events {
            if event.get("event") != Some(&json!("review_requested")) {
                continue;
            }
            if login(event.get("requested_reviewer")).to_lowercase() != me {
                continue;
            }
            let at = text(event.get("created_at"));
            if latest
                .as_ref()
                .is_none_or(|latest| jsrt::utf16::cmp(&at, latest).is_gt())
            {
                latest = Some(at);
            }
        }
        Ok(latest)
    }

    /// Who last asked this account for a review of the pull request, directly or through `team` (a requested team
    /// it is a member of): the latest `review_requested` event's actor, with its time. `None` when no event says.
    pub fn last_review_requester(&self, r: &PullRef, team: Option<&str>) -> Result<Option<(String, String)>, Error> {
        let events = self.list(&format!("{}/issues/{}/events", Self::base(r), r.number), 5)?;
        let me = self.account.login.to_lowercase();
        let mut latest: Option<(String, String)> = None;
        for event in events {
            if event.get("event") != Some(&json!("review_requested")) {
                continue;
            }
            let direct = login(event.get("requested_reviewer")).to_lowercase() == me;
            let slug = text(event.get("requested_team").and_then(|team| team.get("slug")));
            let through_team = team.is_some_and(|team| !slug.is_empty() && slug.eq_ignore_ascii_case(team));
            if !direct && !through_team {
                continue;
            }
            let at = text(event.get("created_at"));
            if latest
                .as_ref()
                .is_none_or(|(known, _)| jsrt::utf16::cmp(&at, known).is_gt())
            {
                latest = Some((at, login(event.get("actor"))));
            }
        }
        Ok(latest.filter(|(_, actor)| !actor.is_empty()))
    }

    /// Whether this account is an active member of a team of the repository's organization; `None` when the
    /// membership cannot be read (the token lacks `read:org`, or the team is not visible).
    pub fn in_team(&self, org: &str, slug: &str) -> Result<Option<bool>, Error> {
        let path = format!("orgs/{org}/teams/{slug}/memberships/{}", self.account.login);
        let options = RequestOptions {
            allow: vec![404],
            ..RequestOptions::default()
        };
        match self.request("GET", &path, &options) {
            Ok(response) if response.status == 404 => Ok(Some(false)),
            Ok(response) => Ok(Some(
                response.body.as_ref().and_then(|body| body.get("state")) == Some(&json!("active")),
            )),
            Err(error @ Error::RateLimit { .. }) => Err(error),
            Err(_) => Ok(None),
        }
    }

    /// Whether this account may push to the repository (false when that cannot be read).
    pub fn can_push(&self, r: &PullRef) -> Result<bool, Error> {
        match self.get(&Self::base(r)) {
            Ok(response) => {
                let permissions = response.body.as_ref().and_then(|body| body.get("permissions"));
                let set = |key: &str| permissions.and_then(|p| p.get(key)) == Some(&Value::Bool(true));
                Ok(set("push") || set("admin") || set("maintain"))
            }
            Err(error @ Error::RateLimit { .. }) => Err(error),
            Err(_) => Ok(false),
        }
    }

    fn failed_check_detail(name: &str, run: &Value) -> Option<String> {
        let output = run.get("output")?;
        let title = text(output.get("title"));
        let summary = text(output.get("summary"));
        let excerpt: String = [title, summary]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if excerpt.is_empty() {
            return None;
        }
        let clipped: String = excerpt.chars().take(200).collect();
        Some(format!("{name}: {clipped}"))
    }

    fn format_check_summary(passed: usize, pending: usize, failed: &[String], details: &[String]) -> String {
        let mut parts = vec![format!("{passed} passed")];
        if !failed.is_empty() {
            let shown: Vec<&str> = failed.iter().take(5).map(String::as_str).collect();
            parts.push(format!("{} failed ({})", failed.len(), shown.join(", ")));
        }
        if pending > 0 {
            parts.push(format!("{pending} pending"));
        }
        let mut summary = parts.join(", ");
        if !details.is_empty() {
            summary.push_str("\nFailed check details:\n- ");
            summary.push_str(&details.join("\n- "));
        }
        summary
    }

    /// One line on the head commit's CI checks, or `None` when there are none or they cannot be read.
    pub fn check_summary(&self, r: &PullRef, sha: &str) -> Result<Option<String>, Error> {
        let response = match self.get(&format!("{}/commits/{sha}/check-runs?per_page=100", Self::base(r))) {
            Ok(response) => response,
            Err(error @ Error::RateLimit { .. }) => return Err(error),
            Err(_) => return Ok(None),
        };
        let runs = items(response.body.as_ref().and_then(|body| body.get("check_runs")));
        if runs.is_empty() {
            return Ok(None);
        }
        let (mut passed, mut pending) = (0, 0);
        let mut failed: Vec<String> = Vec::new();
        let mut details: Vec<String> = Vec::new();
        for run in runs {
            if run.get("status") != Some(&json!("completed")) {
                pending += 1;
            } else if ["success", "neutral", "skipped"].contains(&text(run.get("conclusion")).as_str()) {
                passed += 1;
            } else {
                let name = text(run.get("name"));
                if details.len() < 5
                    && let Some(detail) = Self::failed_check_detail(&name, &run)
                {
                    details.push(detail);
                }
                failed.push(name);
            }
        }
        Ok(Some(Self::format_check_summary(passed, pending, &failed, &details)))
    }

    /// Post a comment on the pull request's conversation; returns its id.
    pub fn post_comment(&self, r: &PullRef, body: &str) -> Result<i64, Error> {
        let response = self.post(
            &format!("{}/issues/{}/comments", Self::base(r), r.number),
            json!({ "body": body }),
        )?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    /// Post a review in one request; returns its id.
    pub fn post_review(&self, r: &PullRef, review: &ReviewInput) -> Result<i64, Error> {
        let body = serde_json::to_value(review).unwrap_or(Value::Null);
        let response = self.post(&format!("{}/pulls/{}/reviews", Self::base(r), r.number), body)?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    /// Dismiss a review (this account's own, whose block a later review lifted), with the reason shown on it.
    pub fn dismiss_review(&self, r: &PullRef, review_id: i64, message: &str) -> Result<(), Error> {
        self.request(
            "PUT",
            &format!("{}/pulls/{}/reviews/{review_id}/dismissals", Self::base(r), r.number),
            &RequestOptions {
                body: Some(json!({ "message": message, "event": "DISMISS" })),
                ..RequestOptions::default()
            },
        )
        .map(|_| ())
    }

    /// The inline comments GitHub kept of a posted review.
    pub fn comments_of_review(&self, r: &PullRef, review_id: i64) -> Result<Vec<Comment>, Error> {
        let path = format!("{}/pulls/{}/reviews/{review_id}/comments", Self::base(r), r.number);
        Ok(self.list(&path, 5)?.iter().map(Self::comment).collect())
    }

    /// Post one inline comment outside a review; returns its id.
    pub fn post_review_comment(
        &self,
        r: &PullRef,
        commit_id: &str,
        comment: &ReviewCommentInput,
    ) -> Result<i64, Error> {
        let mut body = json!({ "commit_id": commit_id });
        if let (Value::Object(target), Ok(Value::Object(fields))) = (&mut body, serde_json::to_value(comment)) {
            target.extend(fields);
        }
        let response = self.post(&format!("{}/pulls/{}/comments", Self::base(r), r.number), body)?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, Error> {
        let response = self.post("graphql", json!({ "query": query, "variables": variables }))?;
        let body = response.body.unwrap_or(json!({}));
        if let Some(Value::Array(errors)) = body.get("errors")
            && let Some(first) = errors.first()
        {
            let message = format!("GitHub GraphQL: {}", text(first.get("message")));
            return Err(Error::GitHub {
                message: jsrt::utf16::head(&message, 300).to_string(),
                status: 200,
            });
        }
        Ok(match body.get("data") {
            None | Some(Value::Null) => json!({}),
            Some(data) => data.clone(),
        })
    }

    /// Review threads by the database id of their first comment: id, resolved, the reactions on that comment and
    /// the replies.
    pub fn review_threads(&self, r: &PullRef) -> Result<IndexMap<i64, ReviewThread>, Error> {
        let mut out = IndexMap::new();
        let mut after = Value::Null;
        for _ in 0..10 {
            let data = self.graphql(
                THREADS_QUERY,
                json!({ "owner": r.owner, "repo": r.repo, "number": r.number, "after": after }),
            )?;
            let threads = data
                .get("repository")
                .and_then(|repo| repo.get("pullRequest"))
                .and_then(|pull| pull.get("reviewThreads"))
                .filter(|threads| !threads.is_null())
                .cloned()
                .unwrap_or(json!({}));
            for node in items(threads.get("nodes")) {
                let comments = node
                    .get("comments")
                    .filter(|c| !c.is_null())
                    .cloned()
                    .unwrap_or(json!({}));
                let first = comments.get("nodes").and_then(|nodes| nodes.get(0)).cloned();
                let (Some(first), Some(Value::String(id))) = (first, node.get("id")) else {
                    continue;
                };
                let Some(Value::Number(database_id)) = first.get("databaseId") else {
                    continue;
                };
                let reactions = |content: &str| -> u32 {
                    items(first.get("reactionGroups"))
                        .iter()
                        .find(|group| group.get("content") == Some(&json!(content)))
                        .and_then(|group| group.get("reactors"))
                        .and_then(|reactors| reactors.get("totalCount"))
                        .and_then(Value::as_f64)
                        .map_or(0, |count| count as u32)
                };
                let replies = comments
                    .get("totalCount")
                    .and_then(Value::as_f64)
                    .map_or(0, |total| (total - 1.0).max(0.0) as u32);
                let key = database_id
                    .as_i64()
                    .unwrap_or_else(|| database_id.as_f64().unwrap_or(0.0) as i64);
                out.insert(
                    key,
                    ReviewThread {
                        id: id.clone(),
                        resolved: node.get("isResolved") == Some(&Value::Bool(true)),
                        thumbs_up: reactions("THUMBS_UP"),
                        thumbs_down: reactions("THUMBS_DOWN"),
                        replies,
                    },
                );
            }
            let page = threads.get("pageInfo");
            match (
                page.and_then(|p| p.get("hasNextPage")),
                page.and_then(|p| p.get("endCursor")),
            ) {
                (Some(Value::Bool(true)), Some(Value::String(cursor))) => after = json!(cursor),
                _ => break,
            }
        }
        Ok(out)
    }

    /// The review threads of a pull request as knowledge-graph notes: the first comment of each, `addressed` when
    /// the thread was resolved, `outdated` when only a later commit changed its lines, else `open`.
    pub fn review_notes(&self, r: &PullRef) -> Result<Vec<knowledge::Note>, Error> {
        let mut out = Vec::new();
        let mut after = Value::Null;
        for _ in 0..10 {
            let data = self.graphql(
                NOTES_QUERY,
                json!({ "owner": r.owner, "repo": r.repo, "number": r.number, "after": after }),
            )?;
            let threads = data
                .get("repository")
                .and_then(|repo| repo.get("pullRequest"))
                .and_then(|pull| pull.get("reviewThreads"))
                .filter(|threads| !threads.is_null())
                .cloned()
                .unwrap_or(json!({}));
            for node in items(threads.get("nodes")) {
                let Some(first) = node
                    .get("comments")
                    .and_then(|comments| comments.get("nodes"))
                    .and_then(|nodes| nodes.get(0))
                else {
                    continue;
                };
                let (path, url) = (text(node.get("path")), text(first.get("url")));
                if path.is_empty() || url.is_empty() {
                    continue;
                }
                let flag = |name: &str| node.get(name) == Some(&Value::Bool(true));
                let line = node
                    .get("line")
                    .and_then(Value::as_i64)
                    .or_else(|| node.get("originalLine").and_then(Value::as_i64));
                out.push(knowledge::Note {
                    pull: r.number as i64,
                    path,
                    line,
                    author: first
                        .get("author")
                        .and_then(|author| author.get("login"))
                        .and_then(Value::as_str)
                        .unwrap_or("ghost")
                        .to_string(),
                    body: text(first.get("body")),
                    url,
                    at: text(first.get("createdAt")),
                    state: if flag("isResolved") {
                        "addressed"
                    } else if flag("isOutdated") {
                        "outdated"
                    } else {
                        "open"
                    }
                    .into(),
                    human: first
                        .get("author")
                        .and_then(|author| author.get("__typename"))
                        .is_some_and(|kind| kind == "User"),
                    missed: false,
                });
            }
            let page = threads.get("pageInfo");
            match (
                page.and_then(|p| p.get("hasNextPage")),
                page.and_then(|p| p.get("endCursor")),
            ) {
                (Some(Value::Bool(true)), Some(Value::String(cursor))) => after = json!(cursor),
                _ => break,
            }
        }
        Ok(out)
    }

    /// The numbers of a repository's closed pull requests, most recently updated first.
    pub fn closed_pulls(&self, owner: &str, repo: &str, max_pages: u32) -> Result<Vec<u64>, Error> {
        let pulls = self.list(
            &format!("repos/{owner}/{repo}/pulls?state=closed&sort=updated&direction=desc"),
            max_pages,
        )?;
        Ok(pulls
            .iter()
            .filter_map(|pull| pull.get("number").and_then(Value::as_u64))
            .collect())
    }

    /// The merged pull requests that brought commit `sha` into a branch.
    pub fn commit_pulls(&self, owner: &str, repo: &str, sha: &str) -> Result<Vec<u64>, Error> {
        let pulls = self.list(&format!("repos/{owner}/{repo}/commits/{sha}/pulls"), 1)?;
        Ok(pulls
            .iter()
            .filter(|pull| pull.get("merged_at").is_some_and(|at| !at.is_null()))
            .filter_map(|pull| pull.get("number").and_then(Value::as_u64))
            .collect())
    }

    pub fn resolve_thread(&self, thread_id: &str) -> Result<(), Error> {
        self.graphql(RESOLVE_MUTATION, json!({ "threadId": thread_id }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::runner::RunResult;

    struct QueueRunner {
        replies: Mutex<VecDeque<RunResult>>,
        attempts: Mutex<usize>,
    }

    impl Runner for QueueRunner {
        fn run(&self, _argv: &[String], _options: &RunOptions) -> RunResult {
            *self.attempts.lock().expect("attempts") += 1;
            self.replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("scripted reply")
        }
    }

    fn ok_reply() -> RunResult {
        RunResult {
            code: 0,
            stdout: "HTTP/2.0 200 OK\r\n\r\n{\"ok\":true}".into(),
            stderr: String::new(),
        }
    }

    fn connect_err_reply() -> RunResult {
        RunResult {
            code: 1,
            stdout: String::new(),
            stderr: "error connecting to api.github.com\ncheck your internet connection".into(),
        }
    }

    fn status_reply(status: u16, body: &str) -> RunResult {
        RunResult {
            code: 1,
            stdout: format!("HTTP/2.0 {status} Error\r\n\r\n{body}"),
            stderr: format!("gh: HTTP {status}"),
        }
    }

    fn make_client(replies: Vec<RunResult>) -> (GitHub, Arc<QueueRunner>, Arc<Mutex<Vec<u64>>>) {
        let runner = Arc::new(QueueRunner {
            replies: Mutex::new(replies.into()),
            attempts: Mutex::new(0),
        });
        let sleeps = Arc::new(Mutex::new(Vec::new()));
        let recorded = sleeps.clone();
        let mut client = GitHub::new(
            runner.clone(),
            Account::new("bot", "github.com"),
            Arc::new(|| Ok(Env::new())),
            Some(Arc::new(|| 1_791_108_000_000)),
        );
        client.sleep = Arc::new(move |ms| recorded.lock().expect("sleeps").push(ms));
        (client, runner, sleeps)
    }

    #[test]
    fn get_retries_connect_errors_until_success_or_exhaustion() {
        for failures in 0..=TRANSIENT_BACKOFF_MS.len() {
            let mut replies = vec![connect_err_reply(); failures];
            replies.push(ok_reply());
            let (client, runner, sleeps) = make_client(replies);
            let response = client
                .request("GET", "repos/o/r/pulls/1", &RequestOptions::default())
                .expect("should recover within retry budget");
            assert_eq!(response.status, 200);
            assert_eq!(*runner.attempts.lock().expect("attempts"), failures + 1);
            assert_eq!(*sleeps.lock().expect("sleeps"), TRANSIENT_BACKOFF_MS[..failures]);
        }

        let total_attempts = TRANSIENT_BACKOFF_MS.len() + 1;
        let (client, runner, sleeps) = make_client(vec![connect_err_reply(); total_attempts]);
        let error = client
            .request("GET", "repos/o/r/pulls/1", &RequestOptions::default())
            .expect_err("should fail after exhausting retries");
        assert!(matches!(error, Error::GitHub { status: 0, .. }));
        assert_eq!(*runner.attempts.lock().expect("attempts"), total_attempts);
        assert_eq!(*sleeps.lock().expect("sleeps"), TRANSIENT_BACKOFF_MS);
    }

    #[test]
    fn get_retries_502_503_504_but_not_500_404_or_429() {
        for status in [502, 503, 504] {
            let (client, runner, sleeps) = make_client(vec![status_reply(status, "upstream"), ok_reply()]);
            let response = client
                .request("GET", "notifications", &RequestOptions::default())
                .expect("gateway error should retry");
            assert_eq!(response.status, 200);
            assert_eq!(*runner.attempts.lock().expect("attempts"), 2);
            assert_eq!(*sleeps.lock().expect("sleeps"), vec![500]);
        }

        for status in [500, 404, 429] {
            let reply = status_reply(status, "{\"message\":\"err\"}");
            let (client, runner, sleeps) = make_client(vec![reply]);
            let result = client.request("GET", "repos/o/r/pulls/1", &RequestOptions::default());
            assert!(result.is_err(), "status {status} should fail immediately");
            assert_eq!(*runner.attempts.lock().expect("attempts"), 1);
            assert!(sleeps.lock().expect("sleeps").is_empty());
        }
    }

    #[test]
    fn stops_retrying_when_later_attempt_hits_non_transient_error() {
        let (client, runner, sleeps) = make_client(vec![
            connect_err_reply(),
            status_reply(404, "{\"message\":\"Not Found\"}"),
        ]);
        let error = client
            .request("GET", "repos/o/r/pulls/1", &RequestOptions::default())
            .expect_err("404 after transient failure should stop immediately");
        assert!(matches!(error, Error::GitHub { status: 404, .. }));
        assert_eq!(*runner.attempts.lock().expect("attempts"), 2);
        assert_eq!(*sleeps.lock().expect("sleeps"), vec![500]);
    }

    #[test]
    fn non_get_never_retries_transient_errors() {
        for reply in [connect_err_reply(), status_reply(503, "busy")] {
            let (client, runner, sleeps) = make_client(vec![reply]);
            let result = client.request("POST", "repos/o/r/pulls/1/reviews", &RequestOptions::default());
            assert!(result.is_err());
            assert_eq!(*runner.attempts.lock().expect("attempts"), 1);
            assert!(sleeps.lock().expect("sleeps").is_empty());
        }
    }
}
