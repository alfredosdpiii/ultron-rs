use std::sync::Arc;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runner::Env;

/// What a GitHub call (or the work around it) failed with.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// GitHub asked us to wait: nothing is sent for this account before `resume_at` (epoch ms).
    RateLimit {
        message: String,
        resume_at: f64,
    },
    /// Any other API failure; status 0 when `gh` produced no HTTP response.
    GitHub {
        message: String,
        status: u16,
    },
    /// The pull request's head is no longer the commit that was to be reviewed (`StaleHeadError`).
    StaleHead(String),
    /// Another `ultron autoreview run` holds the agent directory (`DaemonRunningError`).
    DaemonRunning(String),
    Other(String),
}

impl Error {
    pub fn message(&self) -> &str {
        match self {
            Error::RateLimit { message, .. } | Error::GitHub { message, .. } => message,
            Error::StaleHead(message) | Error::DaemonRunning(message) | Error::Other(message) => message,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

fn js_array_number(items: &[Value]) -> f64 {
    match items {
        [] => 0.0,
        [one] if !one.is_object() && !one.is_array() => js_value_number(Some(one)),
        _ => f64::NAN,
    }
}

/// `Number(value)` of a JSON value (undefined: NaN).
pub fn js_value_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Object(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => jsrt::number::parse(s),
        Some(Value::Array(items)) => js_array_number(items),
    }
}

/// An id read with `Number(...)`: integral values, else 0.
pub fn id_of(value: Option<&Value>) -> i64 {
    let n = js_value_number(value);
    if n.is_finite() && n.fract() == 0.0 { n as i64 } else { 0 }
}

/// `typeof value === "string" ? value : ""`.
pub fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

pub(super) fn login(value: Option<&Value>) -> String {
    text(value.and_then(|value| value.get("login")))
}

pub(super) fn items(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PullRef {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PullRef {
    pub fn new(host: &str, owner: &str, repo: &str, number: u64) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }
}

pub fn pull_key(r: &PullRef) -> String {
    format!("{}/{}/{}#{}", r.host, r.owner, r.repo, r.number)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiOutput {
    pub status: u16,
    pub headers: IndexMap<String, String>,
    pub text: String,
}

/// Split `gh api -i` output into the status, the headers and the body.
pub fn parse_api_output(stdout: &str) -> ApiOutput {
    let status_line = Regex::new(r"^HTTP/[0-9.]+ ([0-9]{3})[^\n]*\r?\n").expect("static pattern");
    let Some(found) = status_line.captures(stdout) else {
        return ApiOutput {
            status: 0,
            headers: IndexMap::new(),
            text: stdout.into(),
        };
    };
    let rest = &stdout[found[0].len()..];
    let end = Regex::new(r"^\r?\n|\r?\n\r?\n").expect("static pattern").find(rest);
    let head = end.map_or(rest, |end| &rest[..end.start()]);
    let mut headers = IndexMap::new();
    for line in Regex::new(r"\r?\n").expect("static pattern").split(head) {
        if let Some(colon) = line.find(':').filter(|colon| *colon > 0) {
            headers.insert(
                jsrt::string::trim(&line[..colon]).to_lowercase(),
                jsrt::string::trim(&line[colon + 1..]).to_string(),
            );
        }
    }
    ApiOutput {
        status: found[1].parse().unwrap_or(0),
        headers,
        text: end.map_or(String::new(), |end| rest[end.end()..].to_string()),
    }
}

/// `https://api.github.com/repos/o/r/pulls/7` (or `.../issues/7`, or a web URL) as a PullRef.
pub fn pull_ref_from_url(url: &str, host: &str) -> Option<PullRef> {
    static PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"/([^/]+)/([^/]+)/(?:pulls|pull|issues)/([0-9]+)(?:[/?#][^\n\r\u{2028}\u{2029}]*)?$")
            .expect("static pattern")
    });
    let found = PATTERN.captures(url)?;
    Some(PullRef::new(host, &found[1], &found[2], found[3].parse().ok()?))
}

/// `owner/repo#N` or a pull request URL.
pub fn parse_pull_target(target: &str, default_host: &str) -> Option<PullRef> {
    static SHORT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)#([0-9]+)$").expect("static pattern")
    });
    if let Some(found) = SHORT.captures(target) {
        return Some(PullRef::new(default_host, &found[1], &found[2], found[3].parse().ok()?));
    }
    static URL: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^https?://([^/]+)/([^/]+)/([^/]+)/pull/([0-9]+)(?:[/?#][^\n\r\u{2028}\u{2029}]*)?$")
            .expect("static pattern")
    });
    let found = URL.captures(target)?;
    Some(PullRef::new(&found[1], &found[2], &found[3], found[4].parse().ok()?))
}

pub const NOTIFICATION_REASONS: [&str; 3] = ["review_requested", "mention", "team_mention"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// `open` or `closed`.
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub author: String,
    /// A GitHub App (Dependabot, Renovate, ...): `user.type` is `Bot`, or the login ends in `[bot]`.
    pub author_is_bot: bool,
    pub head_sha: String,
    pub base_sha: String,
    pub base_ref: String,
    pub url: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
    pub requested_reviewers: Vec<String>,
    pub requested_teams: Vec<String>,
    /// The commit a merge made on the base branch (squash, merge or rebase); a cherry-pick's source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    /// The repository's default branch; empty when GitHub did not say.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub default_branch: String,
    /// Added and removed lines, as GitHub counts them: how large the review will be.
    #[serde(skip_serializing_if = "is_zero")]
    pub additions: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub deletions: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub id: i64,
    pub user: String,
    pub state: String,
    pub commit_id: String,
    pub submitted_at: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: i64,
    pub user: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    /// Review comments only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_id: Option<i64>,
    /// The comment this one replies to in its review thread (review comments only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub id: String,
    pub resolved: bool,
    pub thumbs_up: u32,
    pub thumbs_down: u32,
    pub replies: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCommentInput {
    pub path: String,
    pub line: i64,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewInput {
    pub commit_id: String,
    /// `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
    pub event: String,
    pub body: String,
    pub comments: Vec<ReviewCommentInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifiedPull {
    #[serde(flatten)]
    pub pull: PullRef,
    pub reason: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPage {
    pub not_modified: bool,
    pub pulls: Vec<NotifiedPull>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Seconds GitHub asks to wait between polls (`X-Poll-Interval`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<jsrt::json::Num>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    pub allow: Vec<u16>,
}

pub type EnvSource = Arc<dyn Fn() -> Result<Env, Error> + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub(super) type SleepFn = Arc<dyn Fn(u64) + Send + Sync>;
