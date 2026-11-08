//! `SessionManager`: an append-only session tree persisted as JSONL.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::context::{
    self, Leaf, SessionContext, SessionProjection, entry_type, id_key, is_message_with_role, object, str_key,
};
use crate::env::Env;
use crate::file::{
    self, CURRENT_SESSION_VERSION, HeaderScanError, generate_id, header_cwd, load_entries_from_file,
    migrate_session_entries, read_session_header,
};
use crate::js;
use crate::paths::{normalize_path, parent, resolve_path};
use jsrt::path::join;

/// Errors from session operations. `Invalid` messages are the TypeScript error messages.
#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Io(io::Error),
}

impl Error {
    /// The Node error code an I/O failure maps to (`EEXIST`, `ENOENT`, ...).
    pub fn code(&self) -> Option<&'static str> {
        let Error::Io(e) = self else { return None };
        Some(match e.kind() {
            io::ErrorKind::AlreadyExists => "EEXIST",
            io::ErrorKind::NotFound => "ENOENT",
            io::ErrorKind::PermissionDenied => "EACCES",
            io::ErrorKind::IsADirectory => "EISDIR",
            io::ErrorKind::NotADirectory => "ENOTDIR",
            _ => "EIO",
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(message) => f.write_str(message),
            Error::Io(e) => write!(f, "{}: {e}", self.code().unwrap_or("EIO")),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::Invalid(message.into()))
}

/// `NewSessionOptions`.
#[derive(Clone, Debug, Default)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// `assertValidSessionId`: alphanumerics with interior `.`, `_`, `-`.
pub fn assert_valid_session_id(id: &str) -> Result<(), Error> {
    let bytes = id.as_bytes();
    let edge_ok = |b: Option<&u8>| b.is_some_and(u8::is_ascii_alphanumeric);
    let valid = edge_ok(bytes.first())
        && edge_ok(bytes.last())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        invalid(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        )
    }
}

/// A node of [`SessionTree`]; `children` index into [`SessionTree::nodes`].
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode<'a> {
    pub entry: &'a Value,
    pub children: Vec<usize>,
    pub label: Option<&'a Value>,
    pub label_timestamp: Option<&'a Value>,
}

/// `getTree()` as an arena (deep sessions stay iterative): roots are entries with a `null`,
/// self-referencing or missing parent; children are ordered by timestamp, oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTree<'a> {
    pub nodes: Vec<TreeNode<'a>>,
    pub roots: Vec<usize>,
}

#[derive(Clone, Debug)]
struct LabelState {
    label: Value,
    timestamp: Value,
}

/// `SessionManager`: entries form a tree through `id`/`parentId`; the leaf is the current position.
/// Appends add a child of the leaf; branching moves the leaf. Persisted sessions are written only once
/// the first assistant message exists, then appended line by line.
pub struct SessionManager {
    env: Box<dyn Env>,
    session_id: String,
    session_file: Option<String>,
    session_dir: String,
    cwd: String,
    persist: bool,
    flushed: bool,
    file_entries: Vec<Value>,
    by_id: IndexMap<String, usize>,
    labels: IndexMap<String, LabelState>,
    leaf_id: Option<Value>,
}

impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionManager")
            .field("session_id", &self.session_id)
            .field("session_file", &self.session_file)
            .field("leaf_id", &self.leaf_id)
            .finish_non_exhaustive()
    }
}

fn append_line(path: &str, entries: &[&Value]) -> io::Result<()> {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&jsrt::json::stringify(entry));
        text.push('\n');
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(text.as_bytes())
}

fn write_all(path: &str, entries: &[Value], create_new: bool) -> io::Result<()> {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&jsrt::json::stringify(entry));
        text.push('\n');
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    options.open(path)?.write_all(text.as_bytes())
}

fn file_timestamp(iso: &str) -> String {
    iso.replace([':', '.'], "-")
}

impl SessionManager {
    fn construct(
        env: Box<dyn Env>,
        cwd: &str,
        session_dir: &str,
        session_file: Option<&str>,
        persist: bool,
        options: Option<NewSessionOptions>,
        preloaded: Option<Vec<Value>>,
    ) -> Result<Self, Error> {
        let mut manager = SessionManager {
            env,
            session_id: String::new(),
            session_file: None,
            session_dir: normalize_path(session_dir),
            cwd: resolve_path(cwd),
            persist,
            flushed: false,
            file_entries: Vec::new(),
            by_id: IndexMap::new(),
            labels: IndexMap::new(),
            leaf_id: None,
        };
        if persist && !manager.session_dir.is_empty() && !Path::new(&manager.session_dir).exists() {
            fs::create_dir_all(&manager.session_dir)?;
        }
        match (session_file, preloaded) {
            (Some(file), preloaded) => manager.set_session_file_with(file, preloaded)?,
            (None, Some(entries)) if !entries.is_empty() => manager.load_entries(entries, options)?,
            _ => {
                manager.new_session(options)?;
            }
        }
        Ok(manager)
    }

    /// `create`: a new persisted session in `session_dir` (default: the cwd's dir under the agent dir).
    pub fn create(
        env: Box<dyn Env>,
        cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, Error> {
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir),
            None => file::default_session_dir(cwd, &env.agent_dir())?,
        };
        Self::construct(env, cwd, &dir, None, true, options, None)
    }

    /// `open`: a specific session file. The cwd comes from `cwd_override`, else the header, else
    /// the process cwd; `session_dir` defaults to the file's directory.
    pub fn open(
        env: Box<dyn Env>,
        path: &str,
        session_dir: Option<&str>,
        cwd_override: Option<&str>,
    ) -> Result<Self, Error> {
        let resolved = resolve_path(path);
        let mut header = None;
        let mut preloaded = None;
        if cwd_override.is_none() && Path::new(&resolved).exists() {
            match read_session_header(&resolved) {
                Ok(found) => header = found,
                Err(HeaderScanError::Io(e)) => return Err(e.into()),
                // The bounded scan only speeds up discovery; a full load stays authoritative.
                Err(HeaderScanError::ScanLimit) => {
                    let entries = load_entries_from_file(&resolved)?;
                    header = entries.first().filter(|e| entry_type(e) == Some("session")).cloned();
                    preloaded = Some(entries);
                }
            }
        }
        let process_cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let cwd = cwd_override
            .or_else(|| header.as_ref().and_then(header_cwd))
            .unwrap_or(&process_cwd)
            .to_string();
        let dir = session_dir
            .filter(|d| !d.is_empty())
            .map_or_else(|| parent(&resolved), normalize_path);
        Self::construct(env, &cwd, &dir, Some(&resolved), true, None, preloaded)
    }

    /// `inMemory`: a session that is never written, optionally adopting stored entries.
    pub fn in_memory(
        env: Box<dyn Env>,
        cwd: &str,
        options: Option<NewSessionOptions>,
        entries: Option<Vec<Value>>,
    ) -> Result<Self, Error> {
        Self::construct(env, cwd, "", None, false, options, entries)
    }

    /// `forkFrom`: copies another session's entries into a new session for `target_cwd`.
    pub fn fork_from(
        mut env: Box<dyn Env>,
        source_path: &str,
        target_cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, Error> {
        let source = resolve_path(source_path);
        let target = resolve_path(target_cwd);
        let entries = load_entries_from_file(&source)?;
        if entries.is_empty() {
            return invalid(format!(
                "Cannot fork: source session file is empty or invalid: {source}"
            ));
        }
        if !entries.iter().any(|e| entry_type(e) == Some("session")) {
            return invalid(format!("Cannot fork: source session has no header: {source}"));
        }
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir),
            None => file::default_session_dir(&target, &env.agent_dir())?,
        };
        fs::create_dir_all(&dir)?;
        let options = options.unwrap_or_default();
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        let id = match options.id {
            Some(id) => id,
            None => env.session_id(),
        };
        let timestamp = jsrt::date::iso_string(env.now_ms());
        let path = join(&[&dir, &format!("{}_{id}.jsonl", file_timestamp(&timestamp))]);
        let header = json!({
            "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
            "timestamp": timestamp, "cwd": target, "parentSession": source,
        });
        write_all(&path, &[header], true)?;
        let body: Vec<&Value> = entries.iter().filter(|e| entry_type(e) != Some("session")).collect();
        append_line(&path, &body)?;
        Self::construct(env, &target, &dir, Some(&path), true, None, None)
    }

    fn set_session_file_with(&mut self, session_file: &str, preloaded: Option<Vec<Value>>) -> Result<(), Error> {
        let path = resolve_path(session_file);
        self.session_file = Some(path.clone());
        if !Path::new(&path).exists() {
            self.new_session(None)?;
            self.session_file = Some(path);
            return Ok(());
        }
        let entries = match preloaded {
            Some(entries) => entries,
            None => load_entries_from_file(&path)?,
        };
        if entries.is_empty() {
            // An empty file gets a header; any other non-session content is left untouched.
            if fs::metadata(&path)?.len() > 0 {
                return invalid(format!("Session file is not a valid ultron session: {path}"));
            }
            self.new_session(None)?;
            self.session_file = Some(path);
            self.rewrite_file()?;
            self.flushed = true;
            return Ok(());
        }
        self.load_entries(entries, None)?;
        self.flushed = true;
        Ok(())
    }

    /// `newSession`: a fresh header and no entries. Returns the new session file path (persisted only).
    pub fn new_session(&mut self, options: Option<NewSessionOptions>) -> Result<Option<String>, Error> {
        let options = options.unwrap_or_default();
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = match options.id {
            Some(id) => id,
            None => self.env.session_id(),
        };
        let timestamp = jsrt::date::iso_string(self.env.now_ms());
        let header = object(vec![
            ("type", Some("session".into())),
            ("version", Some(CURRENT_SESSION_VERSION.into())),
            ("id", Some(self.session_id.clone().into())),
            ("timestamp", Some(timestamp.clone().into())),
            ("cwd", Some(self.cwd.clone().into())),
            ("parentSession", options.parent_session.map(Value::from)),
        ]);
        self.file_entries = vec![header];
        self.by_id.clear();
        self.labels.clear();
        self.leaf_id = None;
        self.flushed = false;
        if self.persist {
            let name = format!("{}_{}.jsonl", file_timestamp(&timestamp), self.session_id);
            self.session_file = Some(join(&[&self.session_dir, &name]));
        }
        Ok(self.session_file.clone())
    }

    fn load_entries(&mut self, mut entries: Vec<Value>, options: Option<NewSessionOptions>) -> Result<(), Error> {
        match entries.iter().find(|e| entry_type(e) == Some("session")) {
            Some(header) => {
                self.session_id = match header.get("id") {
                    Some(Value::String(id)) => id.clone(),
                    other => js::join_string(other),
                };
                let migrated = migrate_session_entries(&mut entries, self.env.as_mut());
                self.file_entries = entries;
                if migrated {
                    self.rewrite_file()?;
                }
            }
            None => {
                self.new_session(options)?;
                self.file_entries.extend(entries);
            }
        }
        self.build_index();
        Ok(())
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels.clear();
        self.leaf_id = None;
        for (i, entry) in self.file_entries.iter().enumerate() {
            if entry_type(entry) == Some("session") {
                continue;
            }
            self.by_id.insert(id_key(entry.get("id")), i);
            self.leaf_id = entry.get("id").cloned();
            if entry_type(entry) == Some("label") {
                Self::apply_label(&mut self.labels, entry);
            }
        }
    }

    fn apply_label(labels: &mut IndexMap<String, LabelState>, entry: &Value) {
        let key = id_key(entry.get("targetId"));
        if jsrt::value::truthy(entry.get("label")) {
            labels.insert(
                key,
                LabelState {
                    label: entry["label"].clone(),
                    timestamp: entry.get("timestamp").cloned().unwrap_or(Value::Null),
                },
            );
        } else {
            labels.shift_remove(&key);
        }
    }

    fn rewrite_file(&self) -> io::Result<()> {
        match (&self.session_file, self.persist) {
            (Some(path), true) => write_all(path, &self.file_entries, false),
            _ => Ok(()),
        }
    }

    fn has_assistant(&self) -> bool {
        self.file_entries.iter().any(|e| is_message_with_role(e, "assistant"))
    }

    /// `_persist`: nothing reaches disk until an assistant message exists; then the whole session is
    /// written once (failing if the file already exists) and later entries are appended.
    fn persist_entry(&mut self, index: usize) -> io::Result<()> {
        let Some(path) = self.session_file.clone().filter(|_| self.persist) else {
            return Ok(());
        };
        if !self.has_assistant() {
            if self.flushed {
                append_line(&path, &[&self.file_entries[index]])?;
            }
            return Ok(());
        }
        if self.flushed {
            append_line(&path, &[&self.file_entries[index]])
        } else {
            write_all(&path, &self.file_entries, true)?;
            self.flushed = true;
            Ok(())
        }
    }

    fn append_entry(&mut self, entry: Value) -> Result<(), Error> {
        let index = self.file_entries.len();
        self.by_id.insert(id_key(entry.get("id")), index);
        self.leaf_id = entry.get("id").cloned();
        self.file_entries.push(entry);
        Ok(self.persist_entry(index)?)
    }

    fn new_id(&mut self) -> String {
        let by_id = &self.by_id;
        generate_id(self.env.as_mut(), |id| by_id.contains_key(&str_key(id)))
    }

    fn now_iso(&mut self) -> String {
        jsrt::date::iso_string(self.env.now_ms())
    }

    /// The common `type, id, parentId, timestamp` head followed by `fields`.
    fn entry_with(&mut self, kind: &str, fields: Vec<(&str, Option<Value>)>) -> Value {
        let mut pairs = vec![
            ("type", Some(Value::from(kind))),
            ("id", Some(self.new_id().into())),
            ("parentId", Some(self.leaf_id.clone().unwrap_or(Value::Null))),
            ("timestamp", Some(self.now_iso().into())),
        ];
        pairs.extend(fields);
        object(pairs)
    }

    fn append(&mut self, entry: Value) -> Result<String, Error> {
        let id = entry["id"].as_str().unwrap_or_default().to_string();
        self.append_entry(entry)?;
        Ok(id)
    }

    /// `appendMessage`: a message entry as child of the leaf. Returns its id.
    pub fn append_message(&mut self, message: Value) -> Result<String, Error> {
        let entry = self.entry_with("message", vec![("message", Some(message))]);
        self.append(entry)
    }

    /// `getSessionName`: the latest `session_info` name; empty clears it.
    pub fn session_name(&self) -> Option<String> {
        let entry = self
            .entries()
            .into_iter()
            .rev()
            .find(|e| entry_type(e) == Some("session_info"))?;
        entry
            .get("name")
            .and_then(Value::as_str)
            .map(jsrt::string::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    }

    /// `branch`: moves the leaf to an earlier entry; the next append starts a new branch there.
    pub fn branch(&mut self, branch_from_id: &str) -> Result<(), Error> {
        let Some(entry) = self.entry(branch_from_id) else {
            return invalid(format!("Entry {branch_from_id} not found"));
        };
        self.leaf_id = entry.get("id").cloned();
        Ok(())
    }

    /// `resetLeaf`: the next append creates a new root.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    // ---- reads ----

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn session_dir(&self) -> &str {
        &self.session_dir
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn session_file(&self) -> Option<&str> {
        self.session_file.as_deref()
    }

    /// The leaf id; `None` before the first entry (or after `reset_leaf`).
    pub fn leaf_id(&self) -> Option<&Value> {
        self.leaf_id.as_ref()
    }

    fn leaf_entry(&self) -> Option<&Value> {
        self.leaf_id
            .as_ref()
            .filter(|id| jsrt::value::truthy(Some(id)))
            .and_then(|id| self.lookup(&id_key(Some(id))))
    }

    fn lookup(&self, key: &str) -> Option<&Value> {
        self.by_id.get(key).map(|i| &self.file_entries[*i])
    }

    /// `getEntry`.
    pub fn entry(&self, id: &str) -> Option<&Value> {
        self.lookup(&str_key(id))
    }

    /// `getChildren`: direct children in index order.
    pub fn children(&self, parent_id: &str) -> Vec<&Value> {
        self.by_id
            .values()
            .map(|i| &self.file_entries[*i])
            .filter(|e| e.get("parentId").and_then(Value::as_str) == Some(parent_id))
            .collect()
    }

    /// `getLabel`.
    pub fn label(&self, id: &str) -> Option<&Value> {
        self.labels.get(&str_key(id)).map(|l| &l.label)
    }

    /// `getBranch`: root-first path to `from_id` (default: the leaf).
    pub fn branch_entries(&self, from_id: Option<&str>) -> Vec<&Value> {
        let start = match from_id {
            Some("") => None,
            Some(id) => self.entry(id),
            None => self.leaf_entry(),
        };
        context::walk_to_root(start, self.by_id.len(), |key| self.lookup(key))
    }

    fn leaf(&self) -> Leaf<'_> {
        self.leaf_id.as_ref().map_or(Leaf::Root, Leaf::Id)
    }

    /// `buildContextEntries` from the leaf.
    pub fn build_context_entries(&self) -> Vec<&Value> {
        context::context_entries(&self.entries(), self.leaf())
    }

    /// `buildSessionProjection` from the leaf.
    pub fn build_session_projection(&self) -> SessionProjection<'_> {
        context::session_projection(&self.entries(), self.leaf())
    }

    /// `buildSessionContext` from the leaf.
    pub fn build_session_context(&self) -> SessionContext {
        self.build_session_projection().into()
    }

    /// `getHeader`.
    pub fn header(&self) -> Option<&Value> {
        self.file_entries.iter().find(|e| entry_type(e) == Some("session"))
    }

    /// `getEntries`: every entry except headers.
    pub fn entries(&self) -> Vec<&Value> {
        self.file_entries
            .iter()
            .filter(|e| entry_type(e) != Some("session"))
            .collect()
    }

    /// `getTree`.
    pub fn tree(&self) -> SessionTree<'_> {
        let entries = self.entries();
        let mut node_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut nodes: Vec<TreeNode> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let key = id_key(entry.get("id"));
                node_of.insert(key.clone(), i);
                let label = self.labels.get(&key);
                TreeNode {
                    entry,
                    children: Vec::new(),
                    label: label.map(|l| &l.label),
                    label_timestamp: label.map(|l| &l.timestamp),
                }
            })
            .collect();
        let mut roots = Vec::new();
        for entry in &entries {
            let node = node_of[&id_key(entry.get("id"))];
            let parent = entry.get("parentId");
            if parent == Some(&Value::Null) || parent == entry.get("id") {
                roots.push(node);
            } else {
                match node_of.get(&id_key(parent)) {
                    Some(&p) => nodes[p].children.push(node),
                    None => roots.push(node),
                }
            }
        }
        let times: Vec<f64> = nodes
            .iter()
            .map(|n| js::date_value(n.entry.get("timestamp")).map_or(f64::NAN, |t| t as f64))
            .collect();
        for node in &mut nodes {
            js::sort_by(&mut node.children, |a, b| times[*a] - times[*b]);
        }
        SessionTree { nodes, roots }
    }
}
