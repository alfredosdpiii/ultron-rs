//! `sessions.list` / `sessions.search` / `sessions.read` (`sessions` of sessions_api.py): the stored sessions of
//! this project, or of every project, listed, grepped and read read-only through [`session_journal::search`]. A
//! finding cites its turns as a span, `session://<id>#<from>..<to>`, which `sessions.read` reads back.

use crate::util::Clock;
use crate::{ApiModule, Request};
use serde_json::{Map, Value, json};
use session_journal::search::{self, Branch, EntryText, Matcher, Query, ReadQuery, SpanRef};
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 50;
/// The most hits a search returns.
const MAX_LIMIT: usize = 1000;
const DEFAULT_PER_SESSION: usize = 10;
const DEFAULT_SNIPPET: usize = 300;
const DEFAULT_READ_LIMIT: usize = 200;
const DEFAULT_TOOL_CHARS: usize = 2000;
/// `cwd="all"`: every project's sessions.
const ALL: &str = "all";

pub struct SessionsModule {
    /// Where sessions are stored, searched in order (the first holding an id wins).
    roots: Vec<PathBuf>,
    /// This session: `sessions.read` reads it by default, and listings mark it.
    session_id: String,
    /// This session's cwd, the default scope.
    cwd: String,
    now: Clock,
}

impl SessionsModule {
    pub fn new(roots: Vec<PathBuf>, session_id: &str, cwd: &str, now: Clock) -> Self {
        let mut unique: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !unique.contains(&root) {
                unique.push(root);
            }
        }
        Self {
            roots: unique,
            session_id: session_id.into(),
            cwd: cwd.into(),
            now,
        }
    }

    /// `cwd`: absent for this session's cwd, "all" for every cwd, else an absolute path.
    fn scope(&self, payload: &Map<String, Value>) -> Result<Option<String>, String> {
        match payload.get("cwd") {
            None | Some(Value::Null) => Ok(Some(self.cwd.clone())),
            Some(Value::String(cwd)) if cwd == ALL => Ok(None),
            Some(Value::String(cwd)) if cwd.starts_with('/') => Ok(Some(cwd.clone())),
            _ => Err("sessions cwd must be an absolute path or \"all\"".into()),
        }
    }

    fn since(&self, payload: &Map<String, Value>) -> Result<Option<f64>, String> {
        match payload.get("days") {
            None | Some(Value::Null) => Ok(None),
            Some(days) => match days.as_f64() {
                Some(days) if days > 0.0 => Ok(Some((self.now)() as f64 - days * 86_400_000.0)),
                _ => Err("sessions days must be a positive number".into()),
            },
        }
    }

    fn list(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        let limit = count(payload, "limit", DEFAULT_LIMIT, 1)?.min(MAX_LIMIT);
        let sessions: Vec<Value> = search::list(&self.roots, self.scope(payload)?.as_deref(), self.since(payload)?)
            .into_iter()
            .take(limit)
            .map(|metadata| {
                let listing = session_journal::names::listing(metadata);
                let m = &listing.metadata;
                json!({
                    "id": m.id,
                    "cwd": m.cwd,
                    "path": m.path.display().to_string(),
                    "created": m.created_at,
                    "modified": m.modified_at as i64,
                    "parent": m.parent_session_id,
                    "name": listing.name,
                    "first_message": listing.first_message,
                    "messages": listing.message_count,
                    "current": m.id == self.session_id,
                })
            })
            .collect();
        Ok(json!({"sessions": sessions, "current": self.session_id}))
    }

    fn search(&self, payload: &Map<String, Value>, request: &Request<'_>) -> Result<Value, String> {
        let ignore_case = match payload.get("ignore_case") {
            None => true,
            Some(Value::Bool(yes)) => *yes,
            _ => return Err("sessions.search ignore_case must be True or False".into()),
        };
        let matcher = match (payload.get("pattern"), payload.get("terms")) {
            (Some(Value::String(pattern)), None) if !pattern.is_empty() => Matcher::pattern(pattern, ignore_case)?,
            (None, Some(terms)) => Matcher::terms(&strings(Some(terms), "terms")?)?,
            _ => return Err("sessions.search takes a pattern or terms".into()),
        };
        let query = Query {
            matcher,
            cwd: self.scope(payload)?,
            sessions: strings(payload.get("sessions"), "sessions")?,
            since_ms: self.since(payload)?,
            branch: branch(payload)?,
            roles: strings(payload.get("roles"), "roles")?,
            limit: count(payload, "limit", DEFAULT_LIMIT, 1)?.min(MAX_LIMIT),
            per_session: count(payload, "per_session", DEFAULT_PER_SESSION, 1)?.min(MAX_LIMIT),
            snippet: count(payload, "snippet", DEFAULT_SNIPPET, 1)?,
        };
        let found = search::grep(&self.roots, &query, &|| request.signal.is_aborted());
        let hits: Vec<Value> = found
            .hits
            .into_iter()
            .map(|hit| {
                json!({
                    "session": hit.session,
                    "cwd": hit.cwd,
                    "entry": hit.entry,
                    "seq": hit.seq,
                    "timestamp": hit.timestamp,
                    "role": hit.role,
                    "tool": hit.tool,
                    "snippet": hit.snippet,
                    "matches": hit.matches,
                    "span": hit.span.to_string(),
                })
            })
            .collect();
        Ok(json!({
            "hits": hits,
            "searched": found.searched,
            "truncated": found.truncated,
            "errors": found.errors,
            "current": self.session_id,
        }))
    }

    fn read(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        let text = |key: &str| match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
            _ => Err(format!("sessions.read {key} must be a nonempty string")),
        };
        let span = match text("span")? {
            Some(span) => {
                if ["session", "from_entry", "to_entry"]
                    .iter()
                    .any(|k| payload.contains_key(*k))
                {
                    return Err("sessions.read takes a span or session/from_entry/to_entry, not both".into());
                }
                SpanRef::parse(&span)?
            }
            None => SpanRef {
                session: text("session")?.unwrap_or_else(|| self.session_id.clone()),
                from: text("from_entry")?,
                to: text("to_entry")?,
            },
        };
        let lane = match branch(payload)? {
            Branch::Lane(lane) => lane,
            Branch::All => return Err("sessions.read reads one branch: a lane name, not \"all\"".into()),
        };
        let read = search::read(
            &self.roots,
            &ReadQuery {
                span,
                lane,
                around: count(payload, "around", 0, 0)?,
                limit: count(payload, "limit", DEFAULT_READ_LIMIT, 1)?,
                tool_chars: count(payload, "tool_chars", DEFAULT_TOOL_CHARS, 1)?,
            },
        )?;
        let m = &read.metadata;
        Ok(json!({
            "session": {
                "id": m.id,
                "cwd": m.cwd,
                "path": m.path.display().to_string(),
                "created": m.created_at,
                "modified": m.modified_at as i64,
                "parent": m.parent_session_id,
                "name": read.name,
                "current": m.id == self.session_id,
            },
            "span": read.span.to_string(),
            "entries": read.entries.iter().map(entry).collect::<Vec<_>>(),
            "truncated": read.truncated,
        }))
    }
}

fn entry(text: &EntryText) -> Value {
    json!({
        "id": text.id,
        "seq": text.seq,
        "timestamp": text.timestamp,
        "type": text.kind,
        "role": text.role,
        "tool": text.tool,
        "text": text.text,
    })
}

/// An integer field of at least `min`, else `default`.
fn count(payload: &Map<String, Value>, key: &str, default: usize, min: u64) -> Result<usize, String> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => match value.as_u64() {
            Some(n) if n >= min => Ok(n as usize),
            _ => Err(format!("sessions {key} must be an integer of at least {min}")),
        },
    }
}

fn strings(value: Option<&Value>, name: &str) -> Result<Vec<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("sessions {name} must be nonempty strings"))
            })
            .collect(),
        _ => Err(format!("sessions {name} must be a list of strings")),
    }
}

/// `branch`: a lane's branch (default `main`), or "all" for every entry.
fn branch(payload: &Map<String, Value>) -> Result<Branch, String> {
    match payload.get("branch") {
        None | Some(Value::Null) => Ok(Branch::Lane(search::MAIN_LANE.into())),
        Some(Value::String(all)) if all == ALL => Ok(Branch::All),
        Some(Value::String(lane)) if !lane.is_empty() => Ok(Branch::Lane(lane.clone())),
        _ => Err("sessions branch must be a lane name or \"all\"".into()),
    }
}

impl ApiModule for SessionsModule {
    fn prefixes(&self) -> &[&str] {
        &["sessions."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        let payload = request.payload;
        if kernel::ops::find(request.kind).is_none() {
            return Err(kernel::ops::not_wired(request.kind));
        }
        kernel::ops::fields(request.kind, payload)?;
        match request.kind {
            "sessions.list" => self.list(payload),
            "sessions.search" => self.search(payload, request),
            "sessions.read" => self.read(payload),
            other => Err(kernel::ops::not_wired(other)),
        }
    }
}
