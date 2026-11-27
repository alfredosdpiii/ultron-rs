//! `memory.*` (port of `ultron/memory.ts` NativeMemoryService and the memory dispatch of `worker-services.ts`).
//!
//! These calls are the model's, made on purpose; the session's automatic memory (session-core `auto_memory`) uses
//! the Rust-only [`MemoryService::recall_for_turn`], [`MemoryService::retain_session_tail`] and
//! [`MemoryService::project_digest`]. The journal (`{version, namespace, operations}`) records every
//! prepare/propose/correct/forget/get with hashes and references, never memory text; text lives in the backend
//! (Hindsight). `memory.reflect` and `memory.invalidate` (Rust-only) are journaled nowhere. Only propose-created documents can be read, corrected or forgotten. A correction
//! supersedes the current claim on its document, and the local journal outranks a lagging backend index: recalled
//! entries of a superseded claim or a forgotten document are withheld (`excluded`). Interrupted operations are
//! settled on reopen and remote mutations are never replayed. A failed store write poisons the service.

mod dispatch;
mod dream;
mod journal;
mod recall;
mod refinements;
mod writes;

pub use dispatch::{MemoryApi, handle};
pub use dream::{BRIEF_MAX_CHARS, BRIEF_MAX_LINES, Decide, TOPIC_MAX_CHARS, TOPIC_MAX_LINES, page_budget};
use journal::read_journal;
pub use refinements::{PROJECT_REFINEMENT_PREFIX, REFINEMENT_TYPE};

use crate::memory_records::{IndexEntry, RecordIndex, Typed};
use crate::store::read_versioned;
use crate::util::{Clock, system_clock};
use dispatch::SCOPES;
use kernel::Signal;
use kernel::store::DocumentStore;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A memory server's outage: after this many calls in a row without an answer (or, for automatic recall, too slow
/// to wait for), calls stop for [`OUTAGE_PAUSE`], so a server that is down costs a session no timeouts. One
/// threshold for the memorantado client and automatic recall alike (session_core::auto_memory).
pub const OUTAGE_PAUSE_AFTER: u32 = 3;
pub const OUTAGE_PAUSE: std::time::Duration = std::time::Duration::from_secs(60);

/// A Hindsight-shaped document backend. An `Err` whose text is a memory error code (for example "HTTP_ERROR")
/// reports that code; any other text is an OPERATION_ERROR.
pub trait MemoryBackend: Send + Sync {
    fn namespace(&self) -> String;
    /// Scope → identity tags, such as `{"session": ["ultron:session:<id>"]}`.
    fn scope_tags(&self) -> Map<String, Value>;
    /// Whether the backend has `call`: the document calls ("recall", "retain", "get", "delete", "operation") by
    /// default; "fileEvidence" (a retain item's `evidence`: file snapshots it cites) and "claims" (the curation
    /// calls below) only where a backend says so.
    fn supports(&self, call: &str) -> bool {
        matches!(call, "recall" | "retain" | "get" | "delete" | "operation")
    }
    fn recall(&self, request: &Value, signal: &Signal) -> std::result::Result<Value, String>;
    fn retain(&self, request: &Value, signal: &Signal) -> std::result::Result<Value, String>;
    fn get(&self, document_id: &str, signal: &Signal) -> std::result::Result<Value, String>;
    fn delete(&self, document_id: &str, signal: &Signal) -> std::result::Result<Value, String>;
    fn operation(&self, operation_id: &str, signal: &Signal) -> std::result::Result<Value, String>;
    /// One memory unit (a fact or an observation) by id.
    fn unit(&self, _unit_id: &str, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// Curates a memory unit (`{"state": "invalidated", "reason"?}` retires it).
    fn curate(&self, _unit_id: &str, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// An answer reasoned over the bank's memories.
    fn reflect(&self, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// A mental model by id, or `Value::Null` when the bank has none.
    fn mental_model(&self, _id: &str, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    fn create_mental_model(&self, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// The claims of exactly `tags` (`{tags, status?, confirmedBefore?, sort?: "confirmed", limit?}`):
    /// `{claims: [claim]}`, least recently confirmed first with `sort: "confirmed"`.
    fn list(&self, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// Marks claim `id` re-verified now, with optional file evidence (`{evidence?: {source, content, quote}}`).
    fn confirm(&self, _id: &str, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// Replaces claim `id` with `{text, metadata, evidence?}` (tags and key carried over): the new claim.
    fn supersede(&self, _id: &str, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    /// Flags two claims as conflicting (`{id, conflictingId, reason}`), for a person or a later dream to settle.
    fn conflict(&self, _request: &Value, _signal: &Signal) -> std::result::Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
}

/// A stable, non-reversible id for a directory (`projectIdentity` of worker-services.ts).
pub fn directory_identity(path: &str) -> String {
    jsrt::hash::sha256_hex(path.as_bytes())[..16].to_string()
}

/// A session's scope tags, the same for every backend: its own id, its project directory's identity and the home
/// directory's identity.
pub fn worker_scope_tags(session_id: &str, cwd: &str, home: &str) -> Map<String, Value> {
    let tags = json!({
        "session": [format!("ultron:session:{session_id}")],
        "project": [format!("ultron:project:{}", directory_identity(cwd))],
        "global": [format!("ultron:global:{}", directory_identity(home))],
    });
    tags.as_object().cloned().unwrap_or_default()
}

/// Decides whether a recall runs (`{"retrieve", "probability"?}`) and whether a write is kept
/// (`{"action": "keep" | "skip" | "sensitive", "confidence"?}`).
pub type MemoryGate = Arc<dyn Fn(&Value, &Signal) -> std::result::Result<Value, String> + Send + Sync>;

/// The worker's gate: every recall runs; every write is kept unless it holds a secret.
pub fn worker_gate() -> MemoryGate {
    Arc::new(|request: &Value, _signal: &Signal| {
        if request["action"] == "recall" {
            return Ok(json!({"retrieve": true, "probability": 1}));
        }
        let text = request["text"].as_str().unwrap_or_default();
        let typed = request["situation"].as_str().unwrap_or_default();
        let action =
            if host::secrets::contains_sensitive_memory(text) || host::secrets::contains_sensitive_memory(typed) {
                "sensitive"
            } else {
                "keep"
            };
        Ok(json!({"action": action, "confidence": 1}))
    })
}

macro_rules! error_codes {
    ($($name:ident = $text:literal,)*) => {
        /// The codes a [`MemoryError`] and a journal record's `error` carry.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ErrorCode {
            $($name,)*
        }

        impl ErrorCode {
            const ALL: &[ErrorCode] = &[$(ErrorCode::$name,)*];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(ErrorCode::$name => $text,)*
                }
            }
        }
    };
}

error_codes! {
    InvalidInput = "INVALID_INPUT",
    InvalidJournal = "INVALID_JOURNAL",
    BackendMismatch = "BACKEND_MISMATCH",
    StoreError = "STORE_ERROR",
    UnsupportedScope = "UNSUPPORTED_SCOPE",
    UnsupportedApi = "UNSUPPORTED_API",
    InvalidGate = "INVALID_GATE",
    InvalidResponse = "INVALID_RESPONSE",
    UnknownMemory = "UNKNOWN_MEMORY",
    Busy = "BUSY",
    Forgotten = "FORGOTTEN",
    Aborted = "ABORTED",
    OwnerEnded = "OWNER_ENDED",
    OperationError = "OPERATION_ERROR",
    InvalidConfig = "INVALID_CONFIG",
    HttpError = "HTTP_ERROR",
    Timeout = "TIMEOUT",
    ResponseTooLarge = "RESPONSE_TOO_LARGE",
}

impl ErrorCode {
    fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|code| code.as_str() == text)
    }
}

impl PartialEq<&str> for ErrorCode {
    fn eq(&self, text: &&str) -> bool {
        self.as_str() == *text
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
const KINDS: &[&str] = &["prepare", "propose", "correct", "forget", "get"];
const EVIDENCE_CLASSES: &[&str] = &["hypothesis", "user_statement", "tool_evidence", "verified"];
const CLAIM_STATES: &[&str] = &["accepted", "stored", "unknown"];
const STATES: &[&str] = &[
    "started",
    "interrupted",
    "skipped",
    "sensitive",
    "recalled",
    "accepted",
    "stored",
    "forgotten",
    "read",
    "failed",
    "cancelled",
    "unknown",
];
const RECEIPT_STATUSES: &[&str] = &["pending", "processing", "completed", "failed", "cancelled", "not_found"];
const OPERATION_FIELDS: &[&str] = &[
    "id",
    "kind",
    "state",
    "phase",
    "createdAt",
    "updatedAt",
    "scope",
    "taskId",
    "memoryId",
    "tags",
    "queryHash",
    "textHash",
    "evidence",
    "evidenceClass",
    "supersedes",
    "gate",
    "references",
    "excluded",
    "error",
    "operationIds",
    "receipts",
    "observedState",
];

/// A memory error: "Memory operation did not complete (<CODE>)". No raw backend, store or gate error is exposed.
/// The one exception is a journal written in an unsupported format version, which reports that version (TS
/// `UnsupportedFormatVersionError`) so an older Ultron never overwrites it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryError {
    pub code: ErrorCode,
    format: Option<String>,
}

impl MemoryError {
    pub const fn code(code: ErrorCode) -> Self {
        Self { code, format: None }
    }

    fn of(text: &str) -> Self {
        Self::code(ErrorCode::parse(text).unwrap_or(ErrorCode::OperationError))
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.format {
            Some(message) => formatter.write_str(message),
            None => write!(formatter, "Memory operation did not complete ({})", self.code),
        }
    }
}

type Result<T> = std::result::Result<T, MemoryError>;

fn class_label(class: &str) -> &'static str {
    match class {
        "hypothesis" => "unconfirmed hypothesis",
        "user_statement" => "user statement",
        "tool_evidence" => "tool evidence",
        _ => "verified conclusion",
    }
}

fn is_mutation(kind: &str) -> bool {
    matches!(kind, "propose" | "correct" | "forget")
}

fn hash(text: &str) -> String {
    jsrt::hash::sha256_hex(text.as_bytes())
}

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !jsrt::string::trim(text).is_empty())
}

fn is_sha256(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn exact_fields(value: &Map<String, Value>, keys: &[&str]) -> bool {
    value.keys().all(|key| keys.contains(&key.as_str()))
}

fn string_array(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    let unique: HashSet<&str> = items.iter().filter_map(Value::as_str).collect();
    !items.is_empty() && items.iter().all(nonempty) && unique.len() == items.len()
}

fn same_tags(value: &Value, expected: &[String]) -> bool {
    string_array(value)
        && value.as_array().map(Vec::len) == Some(expected.len())
        && expected.iter().all(|tag| {
            value
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == tag.as_str()))
        })
}

fn require_text(value: &Value) -> Result<()> {
    if nonempty(value) {
        Ok(())
    } else {
        Err(MemoryError::code(ErrorCode::InvalidInput))
    }
}

fn scope_of(value: &Value) -> Result<&'static str> {
    match value.as_str() {
        Some("session") => Ok("session"),
        Some("project") => Ok("project"),
        Some("global") => Ok("global"),
        _ => Err(MemoryError::code(ErrorCode::UnsupportedScope)),
    }
}

/// `(?:^|:)<scope>:[^\s:][^\s]*$`
fn scope_tag(tag: &str, scope: &str) -> bool {
    let needle = format!("{scope}:");
    tag.match_indices(&needle).any(|(at, _)| {
        let rest = &tag[at + needle.len()..];
        (at == 0 || tag[..at].ends_with(':'))
            && rest
                .chars()
                .next()
                .is_some_and(|ch| !jsrt::string::is_space(ch) && ch != ':')
            && !rest.chars().any(jsrt::string::is_space)
    })
}

fn scope_tags(value: Option<&Value>, scope: &str) -> Result<Vec<String>> {
    let value = value.ok_or(MemoryError::code(ErrorCode::UnsupportedScope))?;
    if !string_array(value)
        || !value
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| scope_tag(tag.as_str().unwrap_or(""), scope)))
    {
        return Err(MemoryError::code(ErrorCode::UnsupportedScope));
    }
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.as_str().map(str::to_string))
        .collect())
}

fn evidence_refs(value: &Value) -> Result<Vec<Value>> {
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or(MemoryError::code(ErrorCode::InvalidInput))?;
    items
        .iter()
        .map(|item| {
            let map = item.as_object().ok_or(MemoryError::code(ErrorCode::InvalidInput))?;
            let sha = map.get("sha256");
            let span = map
                .get("ref")
                .and_then(Value::as_str)
                .filter(|r| r.starts_with("session://"));
            if !exact_fields(map, &["ref", "sha256"])
                || !map.get("ref").is_some_and(nonempty)
                || sha.is_some_and(|sha| !is_sha256(sha))
                || span.is_some_and(|span| session_journal::search::SpanRef::parse(span).is_err())
            {
                return Err(MemoryError::code(ErrorCode::InvalidInput));
            }
            let mut out = Map::new();
            out.insert("ref".into(), map["ref"].clone());
            if let Some(sha) = sha {
                out.insert("sha256".into(), sha.clone());
            }
            Ok(Value::Object(out))
        })
        .collect()
}

/// The most quotes one memory's evidence may carry, and the longest quote.
const MAX_QUOTES: usize = 4;
const MAX_QUOTE_BYTES: usize = 300;
/// The largest file a staleness check reads.
const MAX_QUOTED_FILE_BYTES: u64 = 4 << 20;

/// File text an evidence ref quotes (Rust-only): `{path, quote}`, kept in the memory's Hindsight metadata (never in
/// the journal, which holds hashes) so a later recall can tell whether the file still says it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Quote {
    path: String,
    quote: String,
}

/// The file an evidence ref names (`src/a.rs`, `src/a.rs:12`, `src/a.rs:12-30`): relative, inside the project.
fn quoted_path(reference: &str) -> Option<&str> {
    let path = match reference.rsplit_once(':') {
        Some((path, lines))
            if !lines.is_empty()
                && lines
                    .split('-')
                    .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            path
        }
        _ => reference,
    };
    let inside = Path::new(path)
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    (!path.is_empty() && !path.contains("://") && inside).then_some(path)
}

/// `evidence` without its quotes (as the journal keeps it), and the quotes. A quote needs a ref that names a
/// project file and at most [`MAX_QUOTE_BYTES`] of text.
fn split_evidence(value: &Value) -> Result<(Value, Vec<Quote>)> {
    let Some(items) = value.as_array() else {
        return Ok((value.clone(), Vec::new()));
    };
    let mut quotes = Vec::new();
    let mut stripped = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            stripped.push(item.clone());
            continue;
        };
        let mut map = map.clone();
        if let Some(quote) = map.remove("quote") {
            let text = quote.as_str().map_or_default(str::trim);
            let path = map.get("ref").and_then(Value::as_str).and_then(quoted_path);
            let (Some(path), false, true) = (path, text.is_empty(), text.len() <= MAX_QUOTE_BYTES) else {
                return Err(MemoryError::code(ErrorCode::InvalidInput));
            };
            quotes.push(Quote {
                path: path.to_string(),
                quote: text.to_string(),
            });
        }
        stripped.push(Value::Object(map));
    }
    if quotes.len() > MAX_QUOTES {
        return Err(MemoryError::code(ErrorCode::InvalidInput));
    }
    Ok((Value::Array(stripped), quotes))
}

/// A quoted file's text: a file of at most [`MAX_QUOTED_FILE_BYTES`], else `None`.
fn read_quoted(file: &Path) -> Option<String> {
    let meta = std::fs::metadata(file).ok()?;
    if !meta.is_file() || meta.len() > MAX_QUOTED_FILE_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&std::fs::read(file).ok()?).into_owned())
}

fn squeezed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quote_staleness(root: &Path, quote: &Value) -> Option<String> {
    let path = quote["path"].as_str().and_then(quoted_path)?;
    let text = quote["quote"].as_str()?;
    let file = root.join(path);
    match std::fs::metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(format!("{path} is gone")),
        Ok(meta) if meta.is_file() && meta.len() <= MAX_QUOTED_FILE_BYTES => {
            let bytes = std::fs::read(&file).ok()?;
            (!squeezed(&String::from_utf8_lossy(&bytes)).contains(&squeezed(text)))
                .then(|| format!("{path} no longer has the quoted text"))
        }
        _ => None,
    }
}

/// Why a memory is stale, from the quotes in its Hindsight `metadata` (autolith's check, with no model call): a
/// quoted file under `root` that is gone, or that no longer holds its quote (whitespace aside). `None` when every
/// quote still holds, the memory quotes nothing, or a file cannot be read for another reason.
pub fn staleness(root: &Path, metadata: &Value) -> Option<String> {
    let quotes: Vec<Value> = serde_json::from_str(metadata.get("ultron_quotes")?.as_str()?).ok()?;
    quotes
        .iter()
        .take(MAX_QUOTES)
        .find_map(|quote| quote_staleness(root, quote))
}

fn gate_score<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a Value>> {
    let Some(score) = map.get(key) else {
        return Ok(None);
    };
    if !score.as_f64().is_some_and(|score| (0.0..=1.0).contains(&score)) {
        return Err(MemoryError::code(ErrorCode::InvalidGate));
    }
    Ok(Some(score))
}

fn gate_decision(value: &Value, recall: bool) -> Result<Value> {
    let map = value.as_object().ok_or(MemoryError::code(ErrorCode::InvalidGate))?;
    let score_key = if recall { "probability" } else { "confidence" };
    let score = gate_score(map, score_key)?;
    let (field, decision) = if recall {
        let retrieve = map
            .get("retrieve")
            .filter(|v| v.is_boolean())
            .ok_or(MemoryError::code(ErrorCode::InvalidGate))?;
        ("retrieve", retrieve)
    } else {
        let action = map
            .get("action")
            .filter(|action| matches!(action.as_str(), Some("keep" | "skip" | "sensitive")))
            .ok_or(MemoryError::code(ErrorCode::InvalidGate))?;
        ("action", action)
    };
    let mut out = Map::new();
    out.insert(field.into(), decision.clone());
    if let Some(score) = score {
        out.insert(score_key.into(), score.clone());
    }
    Ok(Value::Object(out))
}

fn evidence_class_of(value: Option<&Value>, fallback: &str) -> Result<String> {
    match value {
        None => Ok(fallback.into()),
        Some(Value::String(class)) if EVIDENCE_CLASSES.contains(&class.as_str()) => Ok(class.clone()),
        Some(_) => Err(MemoryError::code(ErrorCode::InvalidInput)),
    }
}

fn check_abort(signal: &Signal) -> Result<()> {
    if signal.is_aborted() {
        Err(MemoryError::code(ErrorCode::Aborted))
    } else {
        Ok(())
    }
}

fn is_iso(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| !jsrt::string::trim(text).is_empty() && jsrt::date::parse_date(text).is_some())
}

struct Inner {
    records: Vec<Value>,
    /// `None` until loaded; a failed load is kept, as TS caches its loading promise.
    loaded: Option<Result<()>>,
    broken: bool,
    busy: HashSet<String>,
    /// Completed recall decisions by task/query/scope; any dispatched mutation clears them.
    reuse: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct MemoryService {
    store: Arc<dyn DocumentStore>,
    backend: Arc<dyn MemoryBackend>,
    gate: MemoryGate,
    namespace: String,
    filters: Map<String, Value>,
    clock: Clock,
    inner: Arc<Mutex<Inner>>,
    /// Typed records' metadata, shared by every session of the machine (crate::memory_records).
    records: Arc<RecordIndex>,
    /// The project directory evidence quotes are checked in ([`staleness`]); `None` checks nothing.
    root: Option<PathBuf>,
    /// Who settles what dreaming's plain-code checks cannot ([`MemoryService::with_judge`]).
    judge: Option<Decide>,
}

impl MemoryService {
    pub fn new(store: Arc<dyn DocumentStore>, backend: Arc<dyn MemoryBackend>, gate: MemoryGate) -> Result<Self> {
        let namespace = backend.namespace();
        if jsrt::string::trim(&namespace).is_empty() {
            return Err(MemoryError::code(ErrorCode::InvalidConfig));
        }
        let mut filters = Map::new();
        for (scope, tags) in backend.scope_tags() {
            let key = scope_of(&Value::String(scope))?;
            filters.insert(key.into(), json!(scope_tags(Some(&tags), key)?));
        }
        Ok(Self {
            store,
            backend,
            gate,
            namespace,
            filters,
            clock: system_clock(),
            inner: Arc::new(Mutex::new(Inner {
                records: Vec::new(),
                loaded: None,
                broken: false,
                busy: HashSet::new(),
                reuse: HashMap::new(),
            })),
            records: Arc::new(RecordIndex::in_memory()),
            root: None,
            judge: None,
        })
    }

    /// Check evidence quotes of `project` and `session` memories against the files under `root` when they are
    /// recalled (the composition root passes the session's working directory). A `global` memory's quotes name
    /// files of whatever project it came from, so they are never checked.
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// The clock the service reads "now" from (fixtures pin it; the composition root keeps the system clock).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The files `quotes` cite, as file evidence for a backend that snapshots them (`supports("fileEvidence")`):
    /// `{source: "repo://<path>", content, quote}` for each quoted file under the root that can be read.
    fn quoted_files(&self, quotes: &[Quote]) -> Vec<Value> {
        match self.root.as_deref() {
            Some(root) => self.quoted_files_in(root, quotes),
            None => Vec::new(),
        }
    }

    /// [`Self::quoted_files`] under `root`.
    fn quoted_files_in(&self, root: &Path, quotes: &[Quote]) -> Vec<Value> {
        if !self.backend.supports("fileEvidence") {
            return Vec::new();
        }
        quotes
            .iter()
            .filter_map(|quote| {
                let content = read_quoted(&root.join(&quote.path))?;
                Some(json!({"source": format!("repo://{}", quote.path), "content": content, "quote": quote.quote}))
            })
            .collect()
    }

    /// Why a recalled memory of `scope` with Hindsight `metadata` is stale, if it is ([`staleness`]).
    fn stale(&self, scope: &str, metadata: &Value) -> Option<String> {
        let root = self.root.as_deref().filter(|_| scope != "global")?;
        staleness(root, metadata)
    }

    /// Keep typed records' metadata in `records` (the composition root passes the agent directory's shared index).
    pub fn with_records(mut self, records: Arc<RecordIndex>) -> Self {
        self.records = records;
        self
    }

    /// The typed record index this service writes.
    pub fn records(&self) -> &Arc<RecordIndex> {
        &self.records
    }

    /// Scope → identity tags of this service's backend.
    pub fn filters(&self) -> &Map<String, Value> {
        &self.filters
    }

    /// The visible indexed record with document id `id`.
    fn indexed(&self, id: &str) -> Option<IndexEntry> {
        self.records
            .entry(id)
            .ok()
            .flatten()
            .filter(|entry| entry.visible(&self.filters))
    }

    /// Whether this session's journal holds the propose that created document `id`.
    fn journal_owns(&self, id: &str) -> bool {
        jsrt::sync::lock(&self.inner)
            .records
            .iter()
            .any(|op| op["kind"] == "propose" && op["memoryId"] == id && op["phase"] == "backend")
    }

    /// After a write: index the typed record when the backend took it, and show its typed fields in `op`.
    fn index_written(&self, mut op: Value, typed: Option<&Typed>, text: &str) -> Value {
        let Some(typed) = typed else {
            return op;
        };
        let taken = op["phase"] == "backend" && op["state"].as_str().is_some_and(|s| CLAIM_STATES.contains(&s));
        let mut record = typed.to_json();
        if taken {
            let entry = IndexEntry {
                id: op["memoryId"].as_str().unwrap_or_default().to_string(),
                typed: typed.clone(),
                scope: op["scope"].as_str().unwrap_or_default().to_string(),
                tags: op["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|tag| tag.as_str().map(str::to_string))
                    .collect(),
                text_hash: hash(text),
                updated_at: self.now(),
            };
            record["indexed"] = self.records.upsert(entry).is_ok().into();
        } else {
            record["indexed"] = false.into();
        }
        op["record"] = record;
        op
    }

    fn now(&self) -> String {
        jsrt::date::iso_string((self.clock)())
    }

    fn commit(&self, inner: &mut Inner, change: impl FnOnce(&[Value]) -> Vec<Value>) -> Result<()> {
        if inner.broken {
            return Err(MemoryError::code(ErrorCode::StoreError));
        }
        let next = change(&inner.records);
        let document = json!({"version": 1, "namespace": self.namespace, "operations": next});
        if self.store.write(&document).is_err() {
            inner.broken = true;
            return Err(MemoryError::code(ErrorCode::StoreError));
        }
        inner.records = next;
        Ok(())
    }

    fn settle_interrupted_op(op: &Value, now: &str) -> Value {
        if op["state"] != "started" {
            return op.clone();
        }
        let mut op = op.clone();
        let kind = op["kind"].as_str().unwrap_or("");
        op["state"] = if is_mutation(kind) && op["phase"] == "backend" {
            "unknown".into()
        } else {
            "interrupted".into()
        };
        op["error"] = json!({"code": "OWNER_ENDED"});
        op["updatedAt"] = now.into();
        op
    }

    fn load_from_store(&self, inner: &mut Inner) -> Result<()> {
        let saved = self
            .store
            .read()
            .map_err(|_| MemoryError::code(ErrorCode::StoreError))?;
        if let Some(saved) = saved {
            let saved = read_versioned("ultron.memory.state/root", Some(saved), "version")
                .map_err(|message| MemoryError {
                    code: ErrorCode::OperationError,
                    format: Some(message),
                })?
                .unwrap_or(Value::Null);
            inner.records = read_journal(&saved, &self.namespace)?;
        }
        if inner.records.iter().any(|op| op["state"] == "started") {
            let now = self.now();
            self.commit(inner, |records| {
                records.iter().map(|op| Self::settle_interrupted_op(op, &now)).collect()
            })?;
        }
        Ok(())
    }

    fn load(&self) -> Result<()> {
        let mut inner = jsrt::sync::lock(&self.inner);
        if inner.loaded.is_none() {
            let outcome = self.load_from_store(&mut inner);
            inner.loaded = Some(outcome);
        }
        inner.loaded.clone().expect("set above")?;
        if inner.broken {
            return Err(MemoryError::code(ErrorCode::StoreError));
        }
        Ok(())
    }

    fn start(&self, kind: &str, fields: Vec<(&str, Value)>) -> Result<String> {
        let id = jsrt::random::uuid_v4();
        let now = self.now();
        let mut op: Map<String, Value> = fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        for (key, value) in [
            ("id", Value::from(id.clone())),
            ("kind", kind.into()),
            ("state", "started".into()),
            ("phase", "gate".into()),
            ("createdAt", now.clone().into()),
            ("updatedAt", now.into()),
        ] {
            op.insert(key.into(), value);
        }
        let mut inner = jsrt::sync::lock(&self.inner);
        self.commit(&mut inner, |records| {
            let mut next = records.to_vec();
            next.push(Value::Object(op));
            next
        })?;
        Ok(id)
    }

    fn operation(&self, id: &str) -> Value {
        jsrt::sync::lock(&self.inner)
            .records
            .iter()
            .find(|op| op["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn update(&self, id: &str, fields: Vec<(&str, Value)>) -> Result<Value> {
        let now = self.now();
        let mut inner = jsrt::sync::lock(&self.inner);
        self.commit(&mut inner, |records| {
            records
                .iter()
                .map(|op| {
                    if op["id"] != id {
                        return op.clone();
                    }
                    let mut op = op.clone();
                    for (key, value) in &fields {
                        op[*key] = value.clone();
                    }
                    op["updatedAt"] = now.clone().into();
                    op
                })
                .collect()
        })?;
        Ok(inner
            .records
            .iter()
            .find(|op| op["id"] == id)
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn tags(&self, scope: &str) -> Result<Vec<String>> {
        scope_tags(self.filters.get(scope), scope)
    }

    fn attempt<T>(&self, id: &str, signal: &Signal, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let outcome = check_abort(signal).and_then(|()| action());
        let Err(error) = outcome else {
            return outcome;
        };
        let op = self.operation(id);
        let mut code = if signal.is_aborted() {
            ErrorCode::Aborted
        } else {
            error.code
        };
        let broken = jsrt::sync::lock(&self.inner).broken;
        if !broken && op["state"] == "started" {
            let kind = op["kind"].as_str().unwrap_or("");
            let state = if is_mutation(kind) && op["phase"] == "backend" {
                "unknown"
            } else if signal.is_aborted() {
                "cancelled"
            } else {
                "failed"
            };
            if self
                .update(
                    id,
                    vec![("state", state.into()), ("error", json!({"code": code.as_str()}))],
                )
                .is_err()
            {
                code = ErrorCode::StoreError;
            }
        }
        if jsrt::sync::lock(&self.inner).broken {
            code = ErrorCode::StoreError;
        }
        Err(MemoryError::code(code))
    }

    /// The propose operation that created document `id`, checked against the configured scope tags.
    fn owned(&self, id: &Value) -> Result<(String, Vec<String>)> {
        require_text(id)?;
        let owner = jsrt::sync::lock(&self.inner)
            .records
            .iter()
            .find(|op| op["kind"] == "propose" && op["memoryId"] == *id && op["phase"] == "backend")
            .cloned()
            .filter(|op| op.get("tags").is_some())
            .ok_or(MemoryError::code(ErrorCode::UnknownMemory))?;
        let scope = owner["scope"].as_str().unwrap_or("").to_string();
        if !same_tags(&owner["tags"], &self.tags(&scope)?) {
            return Err(MemoryError::code(ErrorCode::UnsupportedScope));
        }
        let tags = owner["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();
        Ok((scope, tags))
    }

    fn latest(records: &[Value], id: &str) -> Result<Value> {
        records
            .iter()
            .rev()
            .find(|op| {
                is_mutation(op["kind"].as_str().unwrap_or("")) && op["memoryId"] == id && op["phase"] == "backend"
            })
            .cloned()
            .ok_or(MemoryError::code(ErrorCode::UnknownMemory))
    }

    fn locked<T>(&self, id: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        if !jsrt::sync::lock(&self.inner).busy.insert(id.to_string()) {
            return Err(MemoryError::code(ErrorCode::Busy));
        }
        let outcome = action();
        jsrt::sync::lock(&self.inner).busy.remove(id);
        outcome
    }

    fn editable(&self, id: &str) -> Result<()> {
        let op = Self::latest(&jsrt::sync::lock(&self.inner).records, id)?;
        match op["state"].as_str() {
            Some("started" | "accepted" | "unknown") => Err(MemoryError::code(ErrorCode::Busy)),
            Some("forgotten") => Err(MemoryError::code(ErrorCode::Forgotten)),
            _ => Ok(()),
        }
    }

    /// The latest dispatched claim on a document.
    fn current_claim(records: &[Value], memory_id: &str) -> Option<Value> {
        records
            .iter()
            .rev()
            .find(|op| {
                matches!(op["kind"].as_str(), Some("propose" | "correct"))
                    && op["memoryId"] == memory_id
                    && op["phase"] == "backend"
                    && op["state"].as_str().is_some_and(|state| CLAIM_STATES.contains(&state))
            })
            .cloned()
    }

    /// A requested forget outranks anything the backend still returns, even if completion is unknown.
    fn withdrawn(records: &[Value], memory_id: &str) -> bool {
        Self::latest(records, memory_id)
            .is_ok_and(|op| op["kind"] == "forget" && matches!(op["state"].as_str(), Some("forgotten" | "unknown")))
    }

    pub fn list(&self) -> Result<Value> {
        self.load()?;
        let mut records = jsrt::sync::lock(&self.inner).records.clone();
        let links: Vec<(String, Value)> = records
            .iter()
            .filter(|op| {
                op["kind"] == "correct"
                    && op.get("supersedes").is_some_and(nonempty)
                    && op["state"].as_str().is_some_and(|state| CLAIM_STATES.contains(&state))
            })
            .map(|op| (op["supersedes"].as_str().unwrap_or("").to_string(), op["id"].clone()))
            .collect();
        for (previous, by) in links {
            if let Some(op) = records.iter_mut().find(|op| op["id"] == previous.as_str()) {
                op["supersededBy"] = by;
            }
        }
        Ok(Value::Array(records))
    }

    pub fn why(&self, task_id: &Value) -> Result<Value> {
        require_text(task_id)?;
        self.load()?;
        let records = jsrt::sync::lock(&self.inner).records.clone();
        Ok(Value::Array(
            records
                .into_iter()
                .filter(|op| op["kind"] == "prepare" && op["taskId"] == *task_id)
                .collect(),
        ))
    }

    /// Automatic retain of a session's distilled notes (Rust-only; session-core `auto_memory` sends them): `text` is
    /// the keyed claim `document_id` under the `project` scope, replacing the claim's text, and is marked as distilled
    /// up to `through`, the session entry (or compaction) it came from. A text the write gate finds sensitive, or a
    /// backend without `retain`, is not sent (`Ok(false)`).
    pub fn retain_session_tail(
        &self,
        document_id: &str,
        text: &str,
        context: &str,
        through: &str,
        signal: &Signal,
    ) -> Result<bool> {
        let tags = self.tags("project")?;
        if !self.backend.supports("retain") || jsrt::string::trim(text).is_empty() {
            return Ok(false);
        }
        let request = json!({"action": "propose", "text": text, "scope": "project"});
        let decision = gate_decision(
            &(self.gate)(&request, signal).map_err(|_| MemoryError::code(ErrorCode::OperationError))?,
            false,
        )?;
        if decision["action"] != "keep" {
            return Ok(false);
        }
        let request = json!({
            "items": [{
                "content": text, "context": context, "timestamp": jsrt::date::iso_string((self.clock)()), "document_id": document_id,
                "tags": tags, "update_mode": "replace",
                "metadata": {"ultron_source": "auto", "ultron_through": through},
            }],
            "async": true,
        });
        self.backend
            .retain(&request, signal)
            .map(|_| true)
            .map_err(|error| MemoryError::of(&error))
    }

    /// Whether `document_id` already holds notes distilled up to `through` (kept by an earlier
    /// [`MemoryService::retain_session_tail`]), so that part of the session is not distilled again. A document the
    /// backend cannot read counts as not kept.
    pub fn tail_kept_through(&self, document_id: &str, through: &str, signal: &Signal) -> bool {
        let kept = self.backend.get(document_id, signal).unwrap_or(Value::Null);
        kept["document_metadata"]["ultron_through"] == through
    }

    /// Marks the memories `ids` re-confirmed in the backend: what the context showed and the model relied on. Only
    /// numeric ids are claims (`episode-*` are not); a backend without `confirm`, or a failed call, is ignored.
    pub fn confirm_recalled(&self, ids: &[String], signal: &Signal) {
        for id in ids.iter().filter(|id| id.parse::<u64>().is_ok()) {
            let _ = self.backend.confirm(id, &json!({}), signal);
        }
    }

    /// The scopes a session reads beyond its own: `project` and `global`, those it has tags for.
    fn read_scopes(&self) -> Vec<(&'static str, Vec<String>)> {
        ["project", "global"]
            .into_iter()
            .filter_map(|scope| self.tags(scope).ok().map(|tags| (scope, tags)))
            .collect()
    }

    /// `memory.reflect` (Rust-only): Hindsight's answer to `query`, reasoned over the memories of `scope`, else of
    /// the `project` and `global` scopes; `{text}`. Read-only and journaled nowhere. Unlike a recall, the answer is
    /// written by Hindsight's model and may draw on a document the user forgot that the bank has not yet dropped.
    pub fn reflect(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let query = payload.get("query").and_then(Value::as_str).unwrap_or_default();
        let tags: Vec<String> = match payload.get("scope") {
            Some(scope) => self.tags(scope_of(scope)?)?,
            None => self.read_scopes().into_iter().flat_map(|(_, tags)| tags).collect(),
        };
        if tags.is_empty() {
            return Err(MemoryError::code(ErrorCode::UnsupportedScope));
        }
        let request = json!({
            "query": query, "budget": "low", "max_tokens": 1024, "tags": tags, "tags_match": "any_strict",
        });
        let response = self
            .backend
            .reflect(&request, signal)
            .map_err(|error| MemoryError::of(&error))?;
        let text = response
            .get("text")
            .and_then(Value::as_str)
            .ok_or(MemoryError::code(ErrorCode::InvalidResponse))?;
        Ok(json!({"text": text}))
    }

    /// `memory.invalidate` (Rust-only): retires the fact `id` that a recall showed (`[fact <id>]`), with the reason,
    /// so no later recall returns it. Only a unit tagged with one of this session's scopes exactly may be retired
    /// (UNKNOWN_MEMORY otherwise); a reason the write gate finds sensitive is not sent. Hindsight derives an
    /// observation from facts and refuses to curate it, and only some of those facts may be wrong: an observation
    /// retires nothing and answers `{id, state: "observation", sources: [{id, text}]}`, its live source facts, so the
    /// caller invalidates the wrong ones and Hindsight rewrites the observation from the rest.
    pub fn invalidate(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let id = payload.get("id").and_then(Value::as_str).unwrap_or_default();
        let unit = self.backend.unit(id, signal).map_err(|error| MemoryError::of(&error))?;
        let tags = unit.get("tags").unwrap_or(&Value::Null);
        let owned = SCOPES
            .iter()
            .any(|scope| self.tags(scope).is_ok_and(|own| same_tags(tags, &own)));
        if !owned {
            return Err(MemoryError::code(ErrorCode::UnknownMemory));
        }
        if unit["type"] == "observation" {
            let sources: Vec<Value> = unit["source_memories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|source| source["state"].as_str().is_none_or(|state| state == "valid"))
                .map(|source| json!({"id": source["id"], "text": source["text"]}))
                .collect();
            return Ok(json!({"id": id, "state": "observation", "sources": sources}));
        }
        let mut request = json!({"state": "invalidated"});
        if let Some(reason) = payload.get("reason").and_then(Value::as_str) {
            let check = json!({"action": "propose", "text": reason, "scope": "session"});
            let decision = gate_decision(
                &(self.gate)(&check, signal).map_err(|_| MemoryError::code(ErrorCode::OperationError))?,
                false,
            )?;
            if decision["action"] == "keep" {
                request["reason"] = reason.into();
            }
        }
        self.backend
            .curate(id, &request, signal)
            .map_err(|error| MemoryError::of(&error))?;
        Ok(json!({"id": id, "state": "invalidated"}))
    }

    /// The project's brief (Rust-only; automatic memory opens a context with it): the Hindsight mental model
    /// `ultron-project-<identity>`, which Hindsight rewrites from the project and global memories after each
    /// consolidation, at most hourly. `{id, content, refreshedAt}`, or `None` while it has no content. The first
    /// call for a project creates the model, whose first content Hindsight writes in the background.
    pub fn project_digest(&self, signal: &Signal) -> Result<Option<Value>> {
        let Some(identity) = self.identity("project") else {
            return Ok(None);
        };
        let id = Self::page_id("project", &identity, None);
        let tags: Vec<String> = self.read_scopes().into_iter().flat_map(|(_, tags)| tags).collect();
        let name = format!("Ultron project brief {identity}");
        self.brief(&id, &name, DIGEST_QUERY, &tags, signal)
    }

    /// The user's brief, like [`Self::project_digest`] but written from the global memories only:
    /// `ultron-user-<global identity>`, what the user prefers, corrects and asks for in every project. A project's
    /// own rules stay out of it, so one repo's convention never reaches another as the user's.
    pub fn user_brief(&self, signal: &Signal) -> Result<Option<Value>> {
        let Some(identity) = self.identity("global") else {
            return Ok(None);
        };
        let id = Self::page_id("global", &identity, None);
        let tags = self.tags("global").unwrap_or_default();
        let name = format!("Ultron user brief {identity}");
        self.brief(&id, &name, USER_QUERY, &tags, signal)
    }

    /// The identity in the first tag of `scope` (`ultron:project:<identity>`), lowercase alphanumerics only.
    fn identity(&self, scope: &str) -> Option<String> {
        tag_identity(&self.tags(scope).ok()?.into_iter().next()?)
    }
}

/// The identity a scope tag names (`ultron:project:<identity>`), lowercase alphanumerics only.
fn tag_identity(tag: &str) -> Option<String> {
    let identity: String = tag
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (!identity.is_empty()).then_some(identity)
}

/// The final-score floor of a turn's recall: Hindsight returns its best matches however weak. Fitted on the real
/// bank (150 real user messages, 1188 recalled hits labeled useful or not by a model): F1 is flat from 0.002 to
/// 0.02 on held-out halves; at 0.005 a turn keeps 45% of the useful hits and drops 90% of the rest (unfloored, 4 in
/// 5 hits shown were noise). 0.05 kept a quarter of the useful ones.
// ponytail: one fixed floor; the reranker is not calibrated across queries, so refit if the bank or reranker changes.
const RECALL_MIN_SCORE: f64 = 0.005;

/// An automatically kept memory (`ultron_source: auto`) not confirmed for this long is withheld from a turn's recall;
/// `memory.prepare` still finds it.
// ponytail: hard cutoff; a ranking prior in memorantado would fade old memories instead of dropping them at 90 days.
const AUTO_MEMORY_MAX_AGE_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// What a project's brief answers (see [`MemoryService::project_digest`]).
const DIGEST_QUERY: &str = "What should a coding agent know before working in this project? Its conventions, the \
    decisions made and why, pitfalls and their fixes, the commands that build, test and run it, and the user's \
    preferences. Leave out finished one-off tasks and progress notes.";
const USER_QUERY: &str = "What should a coding agent know about this user before working with them in any project? \
    How they want answers and code (tone, length, style), what they corrected an agent for and what it should do \
    instead, the requests and procedures they repeat across sessions, and their tools and environment. Only what \
    holds across projects and was seen more than once or stated by the user; leave out project-specific facts, \
    one-off tasks and progress notes.";
