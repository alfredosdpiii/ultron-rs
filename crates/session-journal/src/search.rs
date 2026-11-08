//! Read-only search over stored sessions: list them, grep their conversations, and read a span of one. Files are
//! replayed without opening them ([`replay`]: no torn tail is cut, no legacy file upgraded), so a session another
//! worker is writing, or this process's own, can be read at any time.
//!
//! A span is a run of entries on one branch, named `session://<id>#<from>..<to>` ([`SpanRef`]) so a finding can
//! cite the exact turns it came from. Entry ids are UUIDv7s, ordered by their sequence in the file.

use crate::catalog::{self, scan_writes};
use crate::codec::{self, ParsedHeader};
use crate::legacy_v3::LegacyV3Source;
use crate::ns::{BRANCH_TIP, SESSION_NAME};
use crate::repo::Metadata;
use crate::state::State;
use fancy_regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// The lane whose branch is the session's conversation.
pub const MAIN_LANE: &str = "main";
const SCHEME: &str = "session://";

/// `session://<session>#<from>..<to>`: entries `from` through `to` on one branch of a session (`#<entry>` for one
/// entry; no fragment for the whole branch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanRef {
    pub session: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

impl SpanRef {
    pub fn entry(session: &str, entry: &str) -> SpanRef {
        SpanRef {
            session: session.into(),
            from: Some(entry.into()),
            to: Some(entry.into()),
        }
    }

    pub fn parse(text: &str) -> Result<SpanRef, String> {
        let invalid = || format!("Invalid session span {text:?}: expected session://<id>#<from>..<to>");
        let rest = text.strip_prefix(SCHEME).ok_or_else(invalid)?;
        let (session, fragment) = match rest.split_once('#') {
            Some((session, fragment)) => (session, Some(fragment)),
            None => (rest, None),
        };
        if session.is_empty() {
            return Err(invalid());
        }
        let (from, to) = match fragment {
            None => (None, None),
            Some(fragment) => {
                let (from, to) = fragment.split_once("..").unwrap_or((fragment, fragment));
                let id = |part: &str| (!part.is_empty()).then(|| part.to_string());
                if id(from).is_none() && id(to).is_none() {
                    return Err(invalid());
                }
                (id(from), id(to))
            }
        };
        Ok(SpanRef {
            session: session.into(),
            from,
            to,
        })
    }
}

impl fmt::Display for SpanRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}", self.session)?;
        match (&self.from, &self.to) {
            (None, None) => Ok(()),
            (Some(from), Some(to)) if from == to => write!(f, "#{from}"),
            (from, to) => write!(f, "#{}..{}", from.as_deref().unwrap_or(""), to.as_deref().unwrap_or("")),
        }
    }
}

/// A session file's state, replayed read-only: a format-4 file's complete transactions (a line still being written,
/// or one that does not apply, is skipped), or a legacy Pi v3 file read as format 4 (its entry ids are derived
/// deterministically, so a span into it names the same entries on every read until the file is upgraded).
pub fn replay(path: &Path) -> Result<State, String> {
    let (line, _) =
        crate::io::first_line(path).map_err(|e| format!("Failed to read session {}: {e}", path.display()))?;
    let mut state = State::default();
    match codec::parse_header(&line) {
        Ok(ParsedHeader::V4(_)) => scan_writes(
            path,
            |_| false,
            |writes| {
                if let Some(writes) = writes
                    && state.validate(&writes).is_ok()
                {
                    state.apply(writes);
                }
            },
        )
        .map_err(|e| format!("Failed to read session {}: {e}", path.display()))?,
        Ok(ParsedHeader::V3(_)) => {
            let source = LegacyV3Source::read(path, &mut crate::TestClock::new(0, 0))?;
            for write in source.writes(None)? {
                let writes = vec![write];
                if state.validate(&writes).is_ok() {
                    state.apply(writes);
                }
            }
        }
        Err(_) => return Err(format!("Not a session file: {}", path.display())),
    }
    Ok(state)
}

/// The session's name (`ultron.session.name`).
pub fn name_of(state: &State) -> Option<String> {
    state
        .value(SESSION_NAME, "")
        .and_then(|v| v.value.as_str())
        .map(str::to_string)
}

/// The entries of `lane`'s branch, root first (none when the lane has no tip).
pub fn lane_path<'a>(state: &'a State, lane: &str) -> Result<Vec<&'a Value>, String> {
    let tip = state.value(BRANCH_TIP, lane).and_then(|v| v.value.as_str());
    state.branch_path_refs(tip)
}

/// One entry as text: a message (text parts, tool calls as `[name]` and their code, tool results; thinking left
/// out), a compaction or branch summary, or a custom message. Other entries have no text.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryText {
    pub id: String,
    pub seq: i64,
    pub timestamp: Option<i64>,
    /// The entry's `type`.
    pub kind: String,
    /// The message's role, `compaction` or `branch_summary` for a summary.
    pub role: String,
    /// A tool result's tool.
    pub tool: Option<String>,
    pub text: String,
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn cut(text: &mut String, max: usize) {
    if text.len() > max {
        text.truncate(text.floor_char_boundary(max));
        text.push_str("...");
    }
}

fn content_text(content: &Value, tool_chars: usize) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match str_field(part, "type") {
                "text" => Some(str_field(part, "text").to_string()),
                "toolCall" => {
                    let arguments = &part["arguments"];
                    let body = match (arguments.get("code"), arguments.get("command")) {
                        (Some(Value::String(code)), _) | (_, Some(Value::String(code))) => code.clone(),
                        _ => {
                            let mut json = jsrt::json::stringify(arguments);
                            cut(&mut json, tool_chars);
                            json
                        }
                    };
                    Some(format!("[{}]\n{body}", str_field(part, "name")))
                }
                "image" => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// [`EntryText`] of `entry`, a tool result's text cut to `tool_chars` bytes.
pub fn entry_text(entry: &Value, tool_chars: usize) -> Option<EntryText> {
    let kind = str_field(entry, "type");
    let (role, tool, text) = match kind {
        "message" => {
            let message = &entry["message"];
            let role = str_field(message, "role");
            let mut text = match message.get("content") {
                Some(content) => content_text(content, tool_chars),
                // A `!` command's record: what ran and what it printed.
                None => [str_field(message, "command"), str_field(message, "output")]
                    .iter()
                    .filter(|part| !part.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            if role == "toolResult" || role == "bashExecution" {
                cut(&mut text, tool_chars);
            }
            let tool = message.get("toolName").and_then(Value::as_str).map(str::to_string);
            (role.to_string(), tool, text)
        }
        "compaction" | "branch_summary" => (kind.to_string(), None, str_field(entry, "summary").to_string()),
        "custom_message" => ("custom".to_string(), None, content_text(&entry["content"], tool_chars)),
        _ => return None,
    };
    Some(EntryText {
        id: str_field(entry, "id").into(),
        seq: crate::state::seq_of(entry),
        timestamp: entry.get("timestamp").and_then(Value::as_i64),
        kind: kind.into(),
        role,
        tool,
        text,
    })
}

/// The sessions under `roots` (the first root holding an id wins) started in `cwd` (every cwd when `None`) and
/// modified at or after `since_ms`, most recently modified first.
pub fn list(roots: &[PathBuf], cwd: Option<&str>, since_ms: Option<f64>) -> Vec<Metadata> {
    let mut seen = HashSet::new();
    let mut found: Vec<Metadata> = roots
        .iter()
        .flat_map(|root| catalog::list(root, cwd))
        .filter(|m| since_ms.is_none_or(|since| m.modified_at >= since))
        .filter(|m| seen.insert(m.id.clone()))
        .collect();
    catalog::sort_newest(&mut found);
    found
}

/// The session `id` in the first of `roots` that has it.
pub fn find(roots: &[PathBuf], id: &str) -> Result<Metadata, String> {
    for root in roots {
        if let Some(found) = catalog::find(root, id)? {
            return Ok(found);
        }
    }
    Err(format!("No session {id}"))
}

/// What an entry's text must contain.
pub enum Matcher {
    Pattern(Regex),
    /// Every term, compared lowercased.
    Terms(Vec<String>),
}

impl Matcher {
    pub fn pattern(pattern: &str, ignore_case: bool) -> Result<Matcher, String> {
        RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map(Matcher::Pattern)
            .map_err(|e| format!("Invalid pattern {pattern:?}: {e}"))
    }

    pub fn terms(terms: &[String]) -> Result<Matcher, String> {
        let terms: Vec<String> = terms
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if terms.is_empty() {
            return Err("Search terms must not be empty".into());
        }
        Ok(Matcher::Terms(terms))
    }

    /// Whether a file can hold a match, from its raw text (terms must each appear, JSON-escaped); a pattern always
    /// can.
    fn may_match(&self, path: &Path) -> bool {
        let Matcher::Terms(terms) = self else {
            return true;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return false;
        };
        let raw = raw.to_lowercase();
        terms.iter().all(|term| {
            let escaped = jsrt::json::stringify(&Value::String(term.clone()));
            raw.contains(&escaped[1..escaped.len() - 1])
        })
    }

    /// The byte offset of the first match in `text` and the number of matches, or `None`.
    fn find(&self, text: &str) -> Option<(usize, usize)> {
        match self {
            Matcher::Pattern(regex) => {
                let mut matches = regex.find_iter(text).filter_map(Result::ok);
                let first = matches.next()?.start();
                Some((first, 1 + matches.count()))
            }
            Matcher::Terms(terms) => {
                let lower = text.to_lowercase();
                // Lowercasing can change byte lengths: offsets index the lowercased text only when it kept them.
                let same = lower.len() == text.len();
                let mut first = usize::MAX;
                let mut count = 0;
                for term in terms {
                    let at = lower.find(term.as_str())?;
                    first = first.min(at);
                    count += lower.matches(term.as_str()).count();
                }
                Some((if same { first } else { 0 }, count))
            }
        }
    }
}

/// Which entries of a session are searched.
#[derive(Clone, Debug, PartialEq)]
pub enum Branch {
    /// The branch of a lane's tip (`main`: the conversation).
    Lane(String),
    /// Every entry of the file, in sequence order (every branch and lane).
    All,
}

pub struct Query {
    pub matcher: Matcher,
    /// Sessions started in this cwd; `None`: every cwd.
    pub cwd: Option<String>,
    /// Only these sessions (empty: every session in scope).
    pub sessions: Vec<String>,
    /// Sessions modified at or after this time (ms since the epoch).
    pub since_ms: Option<f64>,
    pub branch: Branch,
    /// Only entries with these roles (empty: every role).
    pub roles: Vec<String>,
    /// At most this many hits in all.
    pub limit: usize,
    /// At most this many hits per session.
    pub per_session: usize,
    /// The snippet's length around the first match, in bytes.
    pub snippet: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub session: String,
    pub cwd: String,
    pub entry: String,
    pub seq: i64,
    pub timestamp: Option<i64>,
    pub role: String,
    pub tool: Option<String>,
    /// The text around the first match.
    pub snippet: String,
    pub matches: usize,
    /// The entry as a span (`session://<session>#<entry>`).
    pub span: SpanRef,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    /// Newest session first; a session's hits in branch order.
    pub hits: Vec<Hit>,
    /// Sessions read (the rest of the scope was not reached when `truncated`).
    pub searched: usize,
    /// The limit or a stop ended the search before the scope did.
    pub truncated: bool,
    /// Sessions that could not be read, with why.
    pub errors: Vec<String>,
}

fn snippet(text: &str, at: usize, width: usize) -> String {
    let start = text.floor_char_boundary(at.saturating_sub(width / 2));
    let end = text.ceil_char_boundary((start + width).min(text.len()));
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(&text[start..end]);
    if end < text.len() {
        out.push_str("...");
    }
    out
}

/// Search the sessions under `roots`. `stop` is asked between sessions (a cancelled request ends the search early,
/// as `truncated`).
pub fn grep(roots: &[PathBuf], query: &Query, stop: &dyn Fn() -> bool) -> Found {
    let mut found = Found::default();
    let scope = list(roots, query.cwd.as_deref(), query.since_ms);
    for metadata in scope
        .iter()
        .filter(|m| query.sessions.is_empty() || query.sessions.contains(&m.id))
    {
        if found.hits.len() >= query.limit || stop() {
            found.truncated = true;
            break;
        }
        found.searched += 1;
        if !query.matcher.may_match(&metadata.path) {
            continue;
        }
        let state = match replay(&metadata.path) {
            Ok(state) => state,
            Err(error) => {
                found.errors.push(error);
                continue;
            }
        };
        let entries = match &query.branch {
            Branch::All => state.entries().iter().collect(),
            Branch::Lane(lane) => match lane_path(&state, lane) {
                Ok(path) => path,
                Err(error) => {
                    found.errors.push(format!("{}: {error}", metadata.id));
                    continue;
                }
            },
        };
        let mut in_session = 0;
        for entry in entries {
            if in_session >= query.per_session || found.hits.len() >= query.limit {
                found.truncated = true;
                break;
            }
            let Some(text) = entry_text(entry, usize::MAX) else {
                continue;
            };
            if !query.roles.is_empty() && !query.roles.contains(&text.role) {
                continue;
            }
            let Some((at, matches)) = query.matcher.find(&text.text) else {
                continue;
            };
            in_session += 1;
            found.hits.push(Hit {
                session: metadata.id.clone(),
                cwd: metadata.cwd.clone(),
                span: SpanRef::entry(&metadata.id, &text.id),
                entry: text.id,
                seq: text.seq,
                timestamp: text.timestamp,
                role: text.role,
                tool: text.tool,
                snippet: snippet(&text.text, at, query.snippet),
                matches,
            });
        }
    }
    found
}

pub struct ReadQuery {
    pub span: SpanRef,
    /// The lane whose branch `to` (or, without `to`, the lane's tip) is read on.
    pub lane: String,
    /// Entries with text added before `from` and after `to`.
    pub around: usize,
    /// At most this many entries.
    pub limit: usize,
    /// Tool results cut to this many bytes.
    pub tool_chars: usize,
}

#[derive(Debug)]
pub struct Span {
    pub metadata: Metadata,
    pub name: Option<String>,
    /// The entries returned, as a span.
    pub span: SpanRef,
    pub entries: Vec<EntryText>,
    /// `limit` cut the span short.
    pub truncated: bool,
}

/// The entries of a span with text, oldest first: from `from` (default: the root) to `to` (default: the lane's
/// tip) on the lane's branch, or on the branch ending at `to` when `to` is on another one. `from` must be on that
/// branch, at or before `to`.
pub fn read(roots: &[PathBuf], query: &ReadQuery) -> Result<Span, String> {
    let metadata = find(roots, &query.span.session)?;
    let state = replay(&metadata.path)?;
    let lane = lane_path(&state, &query.lane)?;
    let path = match &query.span.to {
        None => lane,
        Some(to) if lane.iter().any(|e| str_field(e, "id") == to) => lane,
        Some(to) => {
            if state.entry(to).is_none() {
                return Err(format!("No entry {to} in session {}", metadata.id));
            }
            state.branch_path_refs(Some(to))?
        }
    };
    let position = |id: &str| path.iter().position(|e| str_field(e, "id") == id);
    let end = match &query.span.to {
        None => path.len().saturating_sub(1),
        Some(to) => position(to).ok_or_else(|| format!("No entry {to} in session {}", metadata.id))?,
    };
    let start = match &query.span.from {
        None => 0,
        Some(from) => match position(from) {
            Some(at) if at <= end => at,
            _ => {
                return Err(format!(
                    "Entry {from} is not on the branch before {} in session {}",
                    query.span.to.as_deref().unwrap_or("the tip"),
                    metadata.id
                ));
            }
        },
    };
    let texts: Vec<(usize, EntryText)> = path
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| entry_text(entry, query.tool_chars).map(|text| (at, text)))
        .collect();
    let first = texts.iter().position(|(at, _)| *at >= start).unwrap_or(texts.len());
    let last = texts.iter().rposition(|(at, _)| *at <= end).map_or(0, |last| last + 1);
    let first = first.saturating_sub(query.around);
    let last = (last + query.around).min(texts.len());
    let mut entries: Vec<EntryText> = texts
        .into_iter()
        .skip(first)
        .take(last.saturating_sub(first))
        .map(|(_, text)| text)
        .collect();
    let truncated = entries.len() > query.limit;
    entries.truncate(query.limit);
    let span = SpanRef {
        session: metadata.id.clone(),
        from: entries.first().map(|e| e.id.clone()),
        to: entries.last().map(|e| e.id.clone()),
    };
    Ok(Span {
        name: name_of(&state),
        metadata,
        span,
        entries,
        truncated,
    })
}
