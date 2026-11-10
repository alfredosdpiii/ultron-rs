//! Pinned, read-only GitHub observations. Remote prose never enters a scheduled instruction.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    Watch,
    Babysit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation {
    Initial,
    Snapshot {
        value: Value,
    },
    Failed {
        previous: Option<Value>,
        count: u32,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub repo: String,
    pub pr: u32,
    pub mode: WatchMode,
    pub observation: Observation,
    pub polls: u64,
    pub last_poll: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    Loop,
    Pr(Watch),
}

/// argv-only runner; stderr is deliberately excluded from persisted errors (it may contain credentials).
#[derive(Clone)]
pub struct GitHub {
    pub executable: PathBuf,
    environment: Option<Vec<(String, String)>>,
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            executable: "gh".into(),
            environment: None,
        }
    }
}

impl GitHub {
    pub fn executable(path: impl Into<PathBuf>) -> Self {
        Self {
            executable: path.into(),
            environment: None,
        }
    }

    pub fn for_env(var: &dyn Fn(&str) -> Option<String>) -> Self {
        let environment: Vec<_> = [
            "PATH",
            "HOME",
            "GH_CONFIG_DIR",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ]
        .iter()
        .filter_map(|key| var(key).map(|value| ((*key).to_string(), value)))
        .collect();
        let executable = var("PATH")
            .and_then(|path| jsrt::process::which("gh", &path))
            .unwrap_or_else(|| PathBuf::from("gh"));
        Self {
            executable,
            environment: Some(environment),
        }
    }

    fn json(&self, args: &[String], deadline: Instant) -> Result<Value, String> {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or("PR poll time limit exceeded")?;
        let mut command = Command::new(&self.executable);
        if let Some(environment) = &self.environment {
            command
                .env_clear()
                .envs(environment.iter().map(|(key, value)| (key, value)));
        }
        command
            .args(args)
            .env("GH_HOST", "github.com")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let output = signals::child::run(
            &mut command,
            None,
            signals::child::Bound {
                timeout: Some(timeout),
                keep: Some(2 * 1024 * 1024),
                stop: signals::child::Stop {
                    group: true,
                    term_grace: None,
                },
            },
        )
        .map_err(|_| "could not start gh".to_string())?;
        if !output.success() || output.cut {
            return Err("gh request failed, timed out, or exceeded output limit".into());
        }
        serde_json::from_slice(&output.stdout).map_err(|_| "gh returned invalid JSON".into())
    }

    pub fn poll(&self, watch: &Watch) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let view = self.json(
            &[
                "pr".into(),
                "view".into(),
                watch.pr.to_string(),
                "--repo".into(),
                format!("https://github.com/{}", watch.repo),
                "--json".into(),
                "state,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup,headRefOid,reviews".into(),
            ],
            deadline,
        )?;
        validate_view(&view)?;
        if view["state"] != "OPEN" {
            return snapshot(view, Vec::new());
        }
        let (owner, repo) = watch.repo.split_once('/').ok_or("invalid pinned repository")?;
        let mut threads = Vec::new();
        let mut cursors = std::collections::HashSet::new();
        let mut cursor = None;
        // ponytail: cap at 100 pages; fail visibly rather than silently ignore a truncated PR.
        for _ in 0..100 {
            let mut args = vec![
                "api".into(),
                "--hostname".into(),
                "github.com".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={THREADS}"),
                "-f".into(),
                format!("owner={owner}"),
                "-f".into(),
                format!("repo={repo}"),
                "-F".into(),
                format!("number={}", watch.pr),
            ];
            if let Some(cursor) = &cursor {
                args.extend(["-f".into(), format!("after={cursor}")]);
            }
            let response = self.json(&args, deadline)?;
            if response
                .get("errors")
                .is_some_and(|e| e.as_array().is_none_or(|e| !e.is_empty()))
            {
                return Err("GitHub GraphQL errors".into());
            }
            let page = &response["data"]["repository"]["pullRequest"]["reviewThreads"];
            let nodes = page["nodes"].as_array().ok_or("missing review threads")?;
            for node in nodes {
                let id = node["id"].as_str().ok_or("missing thread id")?;
                let resolved = node["isResolved"].as_bool().ok_or("missing thread resolution")?;
                let count = node["comments"]["totalCount"]
                    .as_u64()
                    .ok_or("missing thread comment count")?;
                // Only unresolved thread metadata is needed. No comment bodies or instructions are included.
                if !resolved {
                    threads.push(json!({"id": id, "comments": count}));
                }
            }
            match page["pageInfo"]["hasNextPage"].as_bool() {
                Some(false) => return snapshot(view, threads),
                Some(true) => {
                    let next = page["pageInfo"]["endCursor"]
                        .as_str()
                        .filter(|c| !c.is_empty())
                        .ok_or("missing review thread cursor")?
                        .to_string();
                    if !cursors.insert(next.clone()) {
                        return Err("repeated review thread cursor".into());
                    }
                    cursor = Some(next);
                }
                None => return Err("missing review thread pagination".into()),
            }
        }
        Err("review thread pagination limit exceeded".into())
    }
}

const THREADS: &str = "query($owner:String!,$repo:String!,$number:Int!,$after:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}nodes{id isResolved comments(first:1){totalCount}}}}}}";

pub fn repository(repo: &str) -> Result<(), String> {
    let pieces: Vec<_> = repo.split('/').collect();
    if pieces.len() != 2
        || pieces.iter().any(|p| {
            p.is_empty()
                || p.len() > 100
                || p.starts_with(['-', '.'])
                || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("watch repository must be owner/repo on github.com".into());
    }
    Ok(())
}

fn validate_view(view: &Value) -> Result<(), String> {
    if !matches!(view["state"].as_str(), Some("OPEN" | "CLOSED" | "MERGED"))
        || view["headRefOid"].as_str().is_none_or(str::is_empty)
        || view["mergeable"].as_str().is_none()
        || view["mergeStateStatus"].as_str().is_none()
        || view["reviewDecision"].as_str().is_none()
        || view.get("statusCheckRollup").is_none()
        || !(view["statusCheckRollup"].is_array() || view["statusCheckRollup"].is_null())
        || !view["reviews"].is_array()
    {
        return Err("incomplete PR status from gh".into());
    }
    Ok(())
}

fn snapshot(view: Value, threads: Vec<Value>) -> Result<Value, String> {
    let mut checks = Vec::new();
    for check in view["statusCheckRollup"].as_array().into_iter().flatten() {
        if check["name"].as_str().or_else(|| check["context"].as_str()).is_none()
            || (check["status"].as_str().is_none() && check["state"].as_str().is_none())
        {
            return Err("incomplete CI check from gh".into());
        }
        checks.push(json!({"name": check["name"], "context": check["context"],
            "status": check["status"], "conclusion": check["conclusion"], "state": check["state"],
            "id": check["detailsUrl"].as_str().or_else(|| check["targetUrl"].as_str())}));
    }
    let reviews: Vec<_> = view["reviews"].as_array().ok_or("missing reviews")?.iter().map(|r| {
        json!({"id": r["id"], "author": r["author"]["login"], "state": r["state"], "submittedAt": r["submittedAt"], "commit": r["commit"]["oid"]})
    }).collect();
    let ready = view["state"] == "OPEN"
        && view["mergeable"] == "MERGEABLE"
        && view["mergeStateStatus"] == "CLEAN"
        && matches!(view["reviewDecision"].as_str(), Some("APPROVED" | ""))
        && threads.is_empty()
        && checks.iter().all(|c| {
            c["state"] == "SUCCESS"
                || (c["status"] == "COMPLETED"
                    && matches!(c["conclusion"].as_str(), Some("SUCCESS" | "NEUTRAL" | "SKIPPED")))
        });
    Ok(normalize(json!({"state": view["state"], "head": view["headRefOid"],
        "mergeable": view["mergeable"], "mergeState": view["mergeStateStatus"],
        "reviewDecision": view["reviewDecision"], "checks": checks, "reviews": reviews,
        "unresolvedThreads": threads, "ready": ready})))
}

fn normalize(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            let mut items: Vec<_> = items.into_iter().map(normalize).collect();
            items.sort_by_cached_key(Value::to_string);
            items.dedup();
            items.into()
        }
        Value::Object(items) => {
            let mut items: Vec<_> = items.into_iter().collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(items.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

impl Watch {
    /// Reduce one fetched observation. Returns whether to wake and whether monitoring ends.
    pub fn observe(&mut self, result: Result<Value, String>, now: i64) -> (bool, bool) {
        self.polls += 1;
        self.last_poll = Some(now);
        let (next, wake, done) = match result {
            Ok(value) => {
                let terminal = value["state"] != "OPEN" || (self.mode == WatchMode::Babysit && value["ready"] == true);
                let wake = match &self.observation {
                    Observation::Initial => self.mode == WatchMode::Babysit || terminal,
                    Observation::Snapshot { value: old } => *old != value,
                    Observation::Failed { .. } => true,
                };
                (Observation::Snapshot { value }, wake, terminal)
            }
            Err(error) => {
                let (previous, count) = match &self.observation {
                    Observation::Initial => (None, 1),
                    Observation::Snapshot { value } => (Some(value.clone()), 1),
                    Observation::Failed { previous, count, .. } => (previous.clone(), count + 1),
                };
                (
                    Observation::Failed { previous, count, error },
                    count == 1 || count >= 3,
                    count >= 3,
                )
            }
        };
        self.observation = next;
        (wake, done)
    }
}
