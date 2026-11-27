//! Explicit, repeatable migration between Pi and Ultron sessions (`ultron/migration.ts`): [`import_pi_session`]
//! copies a whole Pi session tree into a new native format-4 session, [`export_native_session_to_pi`] writes a native
//! session's whole tree back as a Pi session (rollback). Nothing here reads Pi's data directory implicitly: every
//! source path is given by the caller.
//!
//! Pi entry mapping (Pi entry ids are kept as native entry ids, parents are unchanged):
//! - `message`, `compaction`, `branch_summary`, `custom` -> the native entry of the same type;
//! - `custom_message` -> a native `message` entry with a `custom` role message;
//! - every other Pi entry type -> a native `custom` entry `pi-session:<type>` whose data is the Pi entry without
//!   `type`/`id`/`parentId`/`timestamp`;
//! - at the end: the `main` tip is the Pi leaf, labels are Pi's resolved labels, the name is Pi's name, and the `main`
//!   lane configuration carries the model and thinking level Pi resolves at the leaf;
//! - a `context_edit` dominated by its target is baked into the target (replacement or omission); one that only some
//!   branches carry is applied per branch on copies (`<id>~<editId>`, recorded in `ultron.pi.copyOf`);
//! - Pi-reported usage becomes the usage ledger's imported historical root `import:pi:<piSessionId>`;
//! - an entry the native form cannot reproduce exactly keeps its Pi original in `ultron.pi.original`.
//!
//! The native file is byte-for-byte what the TypeScript product writes for the same Pi file
//! (`fixtures/migration/migration.json`).

use crate::pi_view::{native_session_as_pi, native_to_pi_entry};
use delegation::usage::{Measurement, UsageLedger, UsageOptions};
use kernel::store::DocumentStore;
use serde_json::{Map, Value, json};
use session_file::{Leaf, SessionManager, SystemEnv, build_context_entries, build_session_context, entry_messages, js};
use session_journal::ns::{BRANCH_TIP, ENTRY_LABEL, LANE_CONFIG, LANE_STATE, SESSION_NAME, USAGE_ROOT};
use session_journal::{Clock, SessionRepo, SharedClock, SystemClock, Write};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

pub const PI_ENTRY_CUSTOM_TYPE_PREFIX: &str = "pi-session:";
pub const PI_OMITTED_MESSAGE_CUSTOM_TYPE: &str = "pi-import:omitted-message";
pub const PI_LEAF_MARKER_CUSTOM_TYPE: &str = "ultron:pi-leaf";
pub const PI_ORIGINAL: &str = "ultron.pi.original";
pub const PI_COPY_OF: &str = "ultron.pi.copyOf";
/// Pi's `CURRENT_SESSION_VERSION`.
const PI_SESSION_VERSION: u64 = 3;

const KNOWN_PI_ENTRY_TYPES: [&str; 11] = [
    "message",
    "custom_message",
    "compaction",
    "branch_summary",
    "custom",
    "model_change",
    "thinking_level_change",
    "label",
    "session_info",
    "usage",
    "context_edit",
];

pub fn pi_import_usage_root(pi_session_id: &str) -> String {
    format!("import:pi:{pi_session_id}")
}

/// `nativeSessionIdForPiSession`: a UUID-shaped (version 8) digest of the Pi session id.
pub fn native_session_id_for_pi_session(pi_session_id: &str) -> String {
    let digest = jsrt::hash::sha256(format!("ultron:pi-import:{pi_session_id}"));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    jsrt::random::format_uuid(&bytes)
}

fn str_of<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

fn id_of(entry: &Value) -> &str {
    str_of(entry, "id").unwrap_or_default()
}

fn type_of(entry: &Value) -> &str {
    str_of(entry, "type").unwrap_or_default()
}

/// `parseTimestamp`: `Date.parse(value)`, else `fallback`.
fn parse_timestamp(value: Option<&Value>, fallback: i64) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(jsrt::date::parse_date)
        .unwrap_or(fallback)
}

/// Copy `key` of `from` into `out` when it is present (JavaScript drops `undefined` fields).
fn put(out: &mut Map<String, Value>, key: &str, from: &Value) {
    if let Some(value) = from.get(key) {
        out.insert(key.into(), value.clone());
    }
}

/// `applyEdit`: `None` is no edit, `Some(Null)` an omission, `Some(replacement)` a replacement.
fn apply_edit(messages: Vec<Value>, edit: Option<&Value>) -> Vec<Value> {
    match edit {
        None => messages,
        Some(Value::Null) => Vec::new(),
        Some(replacement) => messages
            .iter()
            .map(|message| session_journal::context::replace_content(message, replacement.get("content")))
            .collect(),
    }
}

/// Parent-first order that otherwise keeps file order (`PiTree`).
struct PiTree {
    by_id: HashMap<String, Value>,
    children: HashMap<String, Vec<String>>,
}

impl PiTree {
    fn new(entries: &[Value]) -> Self {
        let by_id: HashMap<String, Value> = entries.iter().map(|e| (id_of(e).to_string(), e.clone())).collect();
        let mut tree = PiTree {
            by_id,
            children: HashMap::new(),
        };
        for entry in entries {
            if let Some(parent) = tree.parent_of(entry) {
                tree.children.entry(parent).or_default().push(id_of(entry).to_string());
            }
        }
        tree
    }

    fn child_ids(&self, id: &str) -> &[String] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// The parent as Pi's tree resolves it: missing or self parents make the entry a root.
    fn parent_of(&self, entry: &Value) -> Option<String> {
        let parent = str_of(entry, "parentId")?;
        (parent != id_of(entry) && self.by_id.contains_key(parent)).then(|| parent.to_string())
    }

    /// Whether `descendant` is below `ancestor` and every leaf under `ancestor` passes through it.
    fn dominates(&self, ancestor: &str, descendant: &str) -> bool {
        let Some(mut current) = self.by_id.get(descendant) else {
            return false;
        };
        let mut steps = 0;
        while let Some(parent) = self.parent_of(current) {
            if self.child_ids(&parent).len() != 1 {
                return false;
            }
            if parent == ancestor {
                return true;
            }
            current = &self.by_id[&parent];
            steps += 1;
            if steps > self.by_id.len() {
                return false;
            }
        }
        false
    }

    fn insertion_order(&self, entries: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut placed: HashSet<String> = HashSet::new();
        let mut waiting: Vec<(String, Vec<Value>)> = Vec::new();
        fn take(waiting: &mut Vec<(String, Vec<Value>)>, id: &str) -> Option<Vec<Value>> {
            let at = waiting.iter().position(|(key, _)| key == id)?;
            Some(waiting.remove(at).1)
        }
        let place = |entry: &Value,
                     out: &mut Vec<Value>,
                     placed: &mut HashSet<String>,
                     waiting: &mut Vec<(String, Vec<Value>)>| {
            let mut stack = vec![entry.clone()];
            while let Some(next) = stack.pop() {
                let id = id_of(&next).to_string();
                out.push(next);
                placed.insert(id.clone());
                if let Some(mut children) = take(waiting, &id) {
                    children.reverse();
                    stack.extend(children);
                }
            }
        };
        for entry in entries {
            if placed.contains(id_of(entry)) {
                continue;
            }
            match self.parent_of(entry) {
                Some(parent) if !placed.contains(&parent) => match waiting.iter_mut().find(|(key, _)| *key == parent) {
                    Some((_, siblings)) => siblings.push(entry.clone()),
                    None => waiting.push((parent, vec![entry.clone()])),
                },
                _ => place(entry, &mut out, &mut placed, &mut waiting),
            }
        }
        let rest: Vec<Value> = waiting.iter().flat_map(|(_, children)| children.clone()).collect();
        for entry in rest {
            if !placed.contains(id_of(&entry)) {
                place(&entry, &mut out, &mut placed, &mut waiting);
            }
        }
        out
    }
}

/// `remap`: point an edit's or label's `targetId` (a compaction's `firstKeptEntryId`) at its copy.
fn remap(entry: &mut Value, mapping: &HashMap<String, String>) {
    let kind = type_of(entry);
    if !matches!(kind, "context_edit" | "label" | "compaction") {
        return;
    }
    let field = if kind == "compaction" {
        "firstKeptEntryId"
    } else {
        "targetId"
    };
    if let Some(next) = str_of(entry, field).and_then(|target| mapping.get(target)) {
        entry[field] = json!(next);
    }
}

/// `splitBranchLocalEdits`: every context edit on a message on its own path ends up dominated by its target, the
/// path from the target to the last fork copied for the edit's branch. Returns the entries and `copyOf`.
fn split_branch_local_edits(input: &[Value]) -> (Vec<Value>, HashMap<String, String>) {
    let mut entries = input.to_vec();
    let mut copy_of: HashMap<String, String> = HashMap::new();
    let mut ids: HashSet<String> = entries.iter().map(|e| id_of(e).to_string()).collect();
    let mut budget = 64 * (input.len() + 1);
    let mut changed = true;
    while changed && budget > 0 {
        changed = false;
        let edits: Vec<String> = entries
            .iter()
            .filter(|e| type_of(e) == "context_edit")
            .map(|e| id_of(e).to_string())
            .collect();
        for edit in edits {
            if budget > 0 && split_one_branch_edit(&edit, &mut entries, &mut copy_of, &mut ids) {
                changed = true;
                budget -= 1;
            }
        }
    }
    (entries, copy_of)
}

fn split_one_branch_edit(
    edit_id: &str,
    entries: &mut Vec<Value>,
    copy_of: &mut HashMap<String, String>,
    ids: &mut HashSet<String>,
) -> bool {
    let tree = PiTree::new(entries);
    let Some((target, path)) = edit_target_path(&tree, edit_id) else {
        return false;
    };
    let Some(fork) = last_fork_index(&tree, &path, edit_id) else {
        return false;
    };
    let (mapping, copies, parent) = clone_path_prefix(&path[..=fork], target, edit_id, copy_of, ids);
    let head = path_next_id(&path, fork, edit_id);
    let moved = collect_moved_subtree(&tree, &head);
    rebuild_entries_with_copies(entries, &head, &parent, &moved, &mapping, copies);
    true
}

fn edit_target_path<'a>(tree: &'a PiTree, edit_id: &str) -> Option<(&'a Value, Vec<&'a Value>)> {
    let edit = tree.by_id.get(edit_id)?;
    let target = str_of(edit, "targetId").and_then(|t| tree.by_id.get(t))?;
    if !matches!(type_of(target), "message" | "custom_message") || tree.dominates(id_of(target), edit_id) {
        return None;
    }
    let mut path: Vec<&Value> = Vec::new();
    let mut parent_id = tree.parent_of(edit);
    while let Some(id) = parent_id {
        let parent = &tree.by_id[&id];
        path.insert(0, parent);
        if id_of(parent) == id_of(target) || path.len() > tree.by_id.len() {
            break;
        }
        parent_id = tree.parent_of(parent);
    }
    (path.first().map(|e| id_of(e)) == Some(id_of(target))).then_some((target, path))
}

fn path_next_id(path: &[&Value], index: usize, edit_id: &str) -> String {
    match path.get(index + 1) {
        Some(entry) => id_of(entry).to_string(),
        None => edit_id.to_string(),
    }
}

fn last_fork_index(tree: &PiTree, path: &[&Value], edit_id: &str) -> Option<usize> {
    let mut fork = None;
    for (index, entry) in path.iter().enumerate() {
        let next = path_next_id(path, index, edit_id);
        if tree.child_ids(id_of(entry)).iter().any(|child| *child != next) {
            fork = Some(index);
        }
    }
    fork
}

fn clone_path_prefix(
    prefix: &[&Value],
    target: &Value,
    edit_id: &str,
    copy_of: &mut HashMap<String, String>,
    ids: &mut HashSet<String>,
) -> (HashMap<String, String>, HashMap<String, Value>, Value) {
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut copies: HashMap<String, Value> = HashMap::new();
    let mut parent = target.get("parentId").cloned().unwrap_or(Value::Null);
    let edit_base = copy_of.get(edit_id).cloned().unwrap_or_else(|| edit_id.to_string());
    for original in prefix {
        let original_id = id_of(original).to_string();
        let source = copy_of
            .get(&original_id)
            .cloned()
            .unwrap_or_else(|| original_id.clone());
        let id = unique_copy_id(&source, &edit_base, ids);
        mapping.insert(original_id.clone(), id.clone());
        copy_of.insert(id.clone(), source);
        let mut copy = (*original).clone();
        copy["id"] = json!(id);
        copy["parentId"] = parent;
        copies.insert(original_id, copy);
        parent = json!(id);
    }
    for copy in copies.values_mut() {
        remap(copy, &mapping);
    }
    (mapping, copies, parent)
}

fn unique_copy_id(source: &str, edit_base: &str, ids: &mut HashSet<String>) -> String {
    let base = format!("{source}~{edit_base}");
    let mut id = base.clone();
    let mut counter = 2;
    while ids.contains(&id) {
        id = format!("{base}~{counter}");
        counter += 1;
    }
    ids.insert(id.clone());
    id
}

fn collect_moved_subtree(tree: &PiTree, head: &str) -> HashSet<String> {
    let mut moved: HashSet<String> = HashSet::from([head.to_string()]);
    let mut queue = VecDeque::from([head.to_string()]);
    while let Some(id) = queue.pop_front() {
        for child in tree.child_ids(&id) {
            moved.insert(child.clone());
            queue.push_back(child.clone());
        }
    }
    moved
}

fn rebuild_entries_with_copies(
    entries: &mut Vec<Value>,
    head: &str,
    parent: &Value,
    moved: &HashSet<String>,
    mapping: &HashMap<String, String>,
    mut copies: HashMap<String, Value>,
) {
    let old = std::mem::take(entries);
    for mut entry in old {
        let id = id_of(&entry).to_string();
        if moved.contains(&id) {
            if id == head {
                entry["parentId"] = parent.clone();
            }
            remap(&mut entry, mapping);
        }
        entries.push(entry);
        if let Some(copy) = copies.remove(&id) {
            entries.push(copy);
        }
    }
}

/// `piUsageEntries`: Pi-reported usage per entry, as Pi's session stats count it.
fn pi_usage_entries(entries: &[&Value]) -> Vec<(i64, Value)> {
    let mut out = Vec::new();
    for entry in entries {
        let usage = match type_of(entry) {
            "usage" | "compaction" | "branch_summary" => entry.get("usage"),
            "message" if matches!(str_of(&entry["message"], "role"), Some("assistant" | "toolResult")) => {
                entry["message"].get("usage")
            }
            _ => None,
        };
        if let Some(usage) = usage.filter(|u| !u.is_null()) {
            out.push((parse_timestamp(entry.get("timestamp"), 0), usage.clone()));
        }
    }
    out
}

struct ImportPlan {
    /// The replacement per message entry id, for edits that reach every leaf under the target (`None`: an edit
    /// without a replacement field).
    message_edits: HashMap<String, Option<Value>>,
    unapplied_context_edits: Vec<String>,
    tails: HashMap<String, Vec<Value>>,
}

fn plan_import(entries: &[Value], tree: &PiTree) -> ImportPlan {
    let edits: Vec<&Value> = entries.iter().filter(|e| type_of(e) == "context_edit").collect();
    let mut message_edits = HashMap::new();
    let mut unapplied = Vec::new();
    for edit in &edits {
        let target = str_of(edit, "targetId").and_then(|t| tree.by_id.get(t));
        let editable = target.is_some_and(|t| matches!(type_of(t), "message" | "custom_message"));
        match target {
            Some(target) if editable && tree.dominates(id_of(target), id_of(edit)) => {
                message_edits.insert(id_of(target).to_string(), edit.get("replacement").cloned());
            }
            _ => unapplied.push(id_of(edit).to_string()),
        }
    }
    let tails = entries
        .iter()
        .filter(|e| type_of(e) == "compaction")
        .map(|compaction| {
            (
                id_of(compaction).to_string(),
                plan_compaction_tail(entries, tree, compaction, &edits),
            )
        })
        .collect();
    ImportPlan {
        message_edits,
        unapplied_context_edits: unapplied,
        tails,
    }
}

fn plan_compaction_tail(entries: &[Value], tree: &PiTree, compaction: &Value, edits: &[&Value]) -> Vec<Value> {
    let id = json!(id_of(compaction));
    let context = build_context_entries(entries, Leaf::Id(&id));
    let mut replacements: HashMap<String, Option<Value>> = HashMap::new();
    for entry in context.iter().filter(|e| type_of(e) == "context_edit") {
        replacements.insert(
            str_of(entry, "targetId").unwrap_or_default().to_string(),
            entry.get("replacement").cloned(),
        );
    }
    for edit in edits
        .iter()
        .filter(|edit| tree.dominates(id_of(compaction), id_of(edit)))
    {
        replacements.insert(
            str_of(edit, "targetId").unwrap_or_default().to_string(),
            edit.get("replacement").cloned(),
        );
    }
    context
        .iter()
        .skip(1)
        .filter(|entry| type_of(entry) != "compaction")
        .flat_map(|entry| {
            let edit = replacements.get(id_of(entry)).cloned().flatten();
            apply_edit(entry_messages(entry), edit.as_ref())
        })
        .collect()
}

fn object(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn edited_message_body(message: &Value, edit: Option<&Value>) -> Map<String, Value> {
    match edit {
        None => object(vec![("type", json!("message")), ("message", message.clone())]),
        Some(Value::Null) => object(vec![
            ("type", json!("custom")),
            ("customType", json!(PI_OMITTED_MESSAGE_CUSTOM_TYPE)),
            ("data", json!({ "message": message })),
        ]),
        Some(replacement) => object(vec![
            ("type", json!("message")),
            (
                "message",
                session_journal::context::replace_content(message, replacement.get("content")),
            ),
        ]),
    }
}

fn to_native_entry_body(entry: &Value, plan: &ImportPlan) -> Map<String, Value> {
    let edit = plan.message_edits.get(id_of(entry)).cloned().flatten();
    let mut out = Map::new();
    match type_of(entry) {
        "message" => return edited_message_body(&entry["message"], edit.as_ref()),
        "custom_message" => {
            let mut message = Map::new();
            message.insert("role".into(), json!("custom"));
            put(&mut message, "customType", entry);
            put(&mut message, "content", entry);
            put(&mut message, "display", entry);
            put(&mut message, "details", entry);
            message.insert("timestamp".into(), js::date_json(entry.get("timestamp")));
            return edited_message_body(&Value::Object(message), edit.as_ref());
        }
        "compaction" => {
            out.insert("type".into(), json!("compaction"));
            put(&mut out, "summary", entry);
            let tail = plan.tails.get(id_of(entry)).cloned().unwrap_or_default();
            out.insert("retainedTail".into(), Value::Array(tail));
            put(&mut out, "tokensBefore", entry);
            put(&mut out, "details", entry);
            put(&mut out, "usage", entry);
            out.insert("fromHook".into(), from_hook(entry));
        }
        "branch_summary" => {
            out.insert("type".into(), json!("branch_summary"));
            match entry.get("fromId") {
                Some(Value::String(from)) if from == "root" => {
                    out.insert("fromId".into(), Value::Null);
                }
                _ => put(&mut out, "fromId", entry),
            }
            put(&mut out, "summary", entry);
            put(&mut out, "details", entry);
            put(&mut out, "usage", entry);
            out.insert("fromHook".into(), from_hook(entry));
        }
        "custom" => {
            out.insert("type".into(), json!("custom"));
            put(&mut out, "customType", entry);
            put(&mut out, "data", entry);
        }
        other => {
            let mut data = entry.as_object().cloned().unwrap_or_default();
            for key in ["type", "id", "parentId", "timestamp"] {
                data.shift_remove(key);
            }
            out.insert("type".into(), json!("custom"));
            out.insert(
                "customType".into(),
                json!(format!("{PI_ENTRY_CUSTOM_TYPE_PREFIX}{other}")),
            );
            out.insert("data".into(), Value::Object(data));
        }
    }
    out
}

/// `fromHook ?? false`.
fn from_hook(entry: &Value) -> Value {
    match entry.get("fromHook") {
        None | Some(Value::Null) => json!(false),
        Some(value) => value.clone(),
    }
}

/// `reproducesPiEntry`: whether export rebuilds exactly this Pi entry from its native entry.
fn reproduces_pi_entry(original: &Value, native: &Map<String, Value>, timestamp: i64) -> bool {
    if native.get("type").and_then(Value::as_str) == Some("compaction") {
        return false;
    }
    let mut entry = native.clone();
    entry.insert("seq".into(), json!(0));
    entry.insert("timestamp".into(), json!(timestamp));
    let mut base = Map::new();
    base.insert("id".into(), native.get("id").cloned().unwrap_or(Value::Null));
    base.insert(
        "parentId".into(),
        native.get("parentId").cloned().unwrap_or(Value::Null),
    );
    base.insert("timestamp".into(), json!(jsrt::date::iso_string(timestamp)));
    native_to_pi_entry(&Value::Object(entry), &base) == *original
}

/// What an import wrote (`ImportPiSessionResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ImportResult {
    pub session_id: String,
    pub path: PathBuf,
    /// Native entries written for Pi entries (copies are not counted).
    pub imported: usize,
    /// Pi entries that could not be imported (always empty: every type is mapped or preserved).
    pub skipped: Vec<(String, usize)>,
    /// Unknown Pi entry types kept as `pi-session:<type>` custom entries, with counts, in first-seen order.
    pub preserved: Vec<(String, usize)>,
    pub unapplied_context_edits: Vec<String>,
    pub branch_copies: usize,
    /// `{root, entries, cost}` of the imported usage.
    pub usage_root: String,
    pub usage_entries: usize,
    pub usage_cost: f64,
}

impl ImportResult {
    /// The TypeScript result object.
    pub fn to_json(&self) -> Value {
        let counted = |items: &[(String, usize)]| -> Value {
            items
                .iter()
                .map(|(kind, count)| json!({"type": kind, "count": count}))
                .collect()
        };
        json!({
            "sessionId": self.session_id,
            "path": self.path.display().to_string(),
            "imported": self.imported,
            "skipped": counted(&self.skipped),
            "preserved": counted(&self.preserved),
            "unappliedContextEdits": self.unapplied_context_edits,
            "branchCopies": self.branch_copies,
            "importedUsage": {"root": self.usage_root, "entries": self.usage_entries,
                "cost": jsrt::json::number(self.usage_cost)},
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportError {
    /// `PiSessionAlreadyImportedError`: nothing was written; the native copy is `session_id` at `path`.
    AlreadyImported {
        pi_session_id: String,
        session_id: String,
        path: PathBuf,
    },
    Failed(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::AlreadyImported {
                pi_session_id,
                session_id,
                path,
            } => write!(
                f,
                "Pi session {pi_session_id} was already imported as native session {session_id}: {}",
                path.display()
            ),
            ImportError::Failed(message) => f.write_str(message),
        }
    }
}

/// The repository clock of an import: the Pi timestamps drive `now()` (ids come from the system generator).
struct PiClock {
    time: Arc<Mutex<i64>>,
}

impl Clock for PiClock {
    fn now(&mut self) -> i64 {
        *jsrt::sync::lock(&self.time)
    }
    fn uuidv7(&mut self, at: i64) -> String {
        SystemClock.uuidv7(at)
    }
    fn next_id(&mut self) -> String {
        SystemClock.next_id()
    }
}

/// The usage ledger's store during an import: it starts empty and keeps what the ledger writes.
struct Captured(Arc<Mutex<Option<Value>>>);

impl DocumentStore for Captured {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(None)
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(document.clone());
        Ok(())
    }
}

/// Called before each commit of an import with its 1-based number; an error fails that commit (the TypeScript tests
/// make `session.mutate` fail the same way).
pub type CommitHook<'a> = &'a mut dyn FnMut(usize) -> Result<(), String>;

/// `importPiSession`: the whole tree of the Pi session at `pi_session_path` as a new native session under
/// `sessions_root` (entry ids, parents and timestamps kept; the `main` tip is the Pi leaf). A failed import leaves
/// no session file behind.
pub fn import_pi_session(pi_session_path: &Path, sessions_root: &Path) -> Result<ImportResult, ImportError> {
    import_pi_session_with(pi_session_path, sessions_root, &mut |_| Ok(()))
}

struct CommitCounter<'a> {
    commits: usize,
    hook: CommitHook<'a>,
}

impl CommitCounter<'_> {
    fn commit(&mut self, session: &mut session_journal::Session, writes: &[Write]) -> Result<(), String> {
        self.commits += 1;
        (self.hook)(self.commits)?;
        session.commit(writes).map(drop)
    }
}

/// [`import_pi_session`] with a hook before each commit.
pub fn import_pi_session_with(
    pi_session_path: &Path,
    sessions_root: &Path,
    hook: CommitHook<'_>,
) -> Result<ImportResult, ImportError> {
    let (header, manager) = load_pi_session_file(pi_session_path)?;
    let cwd = str_of(&header, "cwd").unwrap_or_default().to_string();
    let pi_session_id = str_of(&header, "id").unwrap_or_default().to_string();
    let session_id = native_session_id_for_pi_session(&pi_session_id);
    let now = jsrt::date::now_ms();
    let time = Arc::new(Mutex::new(parse_timestamp(header.get("timestamp"), now)));
    let clock: SharedClock = Arc::new(Mutex::new(PiClock { time: time.clone() }));
    let repo = SessionRepo::new(sessions_root, clock);
    if let Some(existing) = repo.list(None).into_iter().find(|m| m.id == session_id) {
        return Err(ImportError::AlreadyImported {
            pi_session_id,
            session_id,
            path: existing.path,
        });
    }
    let mut session = repo
        .create(&cwd, Some(&session_id), None)
        .map_err(ImportError::Failed)?;
    let path = session.metadata().path.clone();
    let mut commits = CommitCounter { commits: 0, hook };
    let result = write_imported_session(
        &mut session,
        &manager,
        &pi_session_id,
        &session_id,
        &path,
        &time,
        &mut commits,
    );
    drop(session);
    result.map_err(|error| {
        let _ = std::fs::remove_file(&path);
        ImportError::Failed(error)
    })
}

fn load_pi_session_file(pi_session_path: &Path) -> Result<(Value, SessionManager), ImportError> {
    let pi_path = PathBuf::from(session_journal::catalog::absolute(
        &pi_session_path.display().to_string(),
    ));
    let shown = pi_path.display().to_string();
    let file_entries = session_file::load_entries_from_file(&session_file::SystemEnv, &shown)
        .map_err(|e| ImportError::Failed(format!("{shown}: {e}")))?;
    let header = match file_entries.first() {
        Some(header) if type_of(header) == "session" => header.clone(),
        _ => {
            return Err(ImportError::Failed(format!(
                "Not a Pi session file (missing session header): {shown}"
            )));
        }
    };
    let cwd = str_of(&header, "cwd").unwrap_or_default();
    let manager = SessionManager::in_memory(Box::new(SystemEnv), cwd, None, Some(file_entries))
        .map_err(|e| ImportError::Failed(e.to_string()))?;
    Ok((header, manager))
}

fn write_imported_session(
    session: &mut session_journal::Session,
    manager: &SessionManager,
    pi_session_id: &str,
    session_id: &str,
    path: &Path,
    time: &Arc<Mutex<i64>>,
    commits: &mut CommitCounter<'_>,
) -> Result<ImportResult, String> {
    let pi_entries: Vec<Value> = manager.entries().into_iter().cloned().collect();
    let (split, copy_of) = split_branch_local_edits(&pi_entries);
    let tree = PiTree::new(&split);
    let plan = plan_import(&split, &tree);
    let (imported, preserved) = import_entries(session, &pi_entries, &split, &copy_of, &tree, &plan, time, commits)?;
    let usage_root = pi_import_usage_root(pi_session_id);
    let refs: Vec<&Value> = pi_entries.iter().collect();
    let reported = pi_usage_entries(&refs);
    import_usage_history(session, &usage_root, &reported, commits)?;
    let meta_writes = build_import_metadata_writes(manager, &pi_entries, &tree);
    commits.commit(session, &meta_writes)?;
    let cost = reported.iter().fold(0.0, |total, (_, usage)| {
        total + usage["cost"]["total"].as_f64().unwrap_or(0.0)
    });
    Ok(ImportResult {
        session_id: session_id.to_string(),
        path: path.to_path_buf(),
        imported,
        skipped: Vec::new(),
        preserved,
        unapplied_context_edits: plan.unapplied_context_edits,
        branch_copies: copy_of.len(),
        usage_root,
        usage_entries: reported.len(),
        usage_cost: cost,
    })
}

#[expect(clippy::too_many_arguments, reason = "internal migration helper")]
fn import_entries(
    session: &mut session_journal::Session,
    pi_entries: &[Value],
    split: &[Value],
    copy_of: &HashMap<String, String>,
    tree: &PiTree,
    plan: &ImportPlan,
    time: &Arc<Mutex<i64>>,
    commits: &mut CommitCounter<'_>,
) -> Result<(usize, Vec<(String, usize)>), String> {
    let pi_by_id: HashMap<&str, &Value> = pi_entries.iter().map(|e| (id_of(e), e)).collect();
    let mut preserved: Vec<(String, usize)> = Vec::new();
    let mut imported = 0;
    let mut at = *jsrt::sync::lock(time);
    for entry in tree.insertion_order(split) {
        let id = id_of(&entry).to_string();
        let copied = copy_of.get(&id);
        if copied.is_none() {
            record_preserved_type(type_of(&entry), &mut preserved);
        }
        at = parse_timestamp(entry.get("timestamp"), at);
        *jsrt::sync::lock(time) = at;
        let mut native = to_native_entry_body(&entry, plan);
        native.insert("id".into(), json!(id));
        native.insert(
            "parentId".into(),
            tree.parent_of(&entry).map_or(Value::Null, Value::from),
        );
        let mut writes = vec![Write::Entry(native.clone())];
        match copied {
            Some(original) => writes.push(Write::set(PI_COPY_OF, &id, json!(original))),
            None => {
                let original = pi_by_id.get(id.as_str()).copied().unwrap_or(&Value::Null);
                if !reproduces_pi_entry(original, &native, at) {
                    writes.push(Write::set(PI_ORIGINAL, &id, original.clone()));
                }
                imported += 1;
            }
        }
        commits.commit(session, &writes)?;
    }
    Ok((imported, preserved))
}

fn record_preserved_type(kind: &str, preserved: &mut Vec<(String, usize)>) {
    if KNOWN_PI_ENTRY_TYPES.contains(&kind) {
        return;
    }
    match preserved.iter_mut().find(|(k, _)| k == kind) {
        Some((_, count)) => *count += 1,
        None => preserved.push((kind.to_string(), 1)),
    }
}

fn import_usage_history(
    session: &mut session_journal::Session,
    usage_root: &str,
    reported: &[(i64, Value)],
    commits: &mut CommitCounter<'_>,
) -> Result<(), String> {
    if reported.is_empty() {
        return Ok(());
    }
    let captured = Arc::new(Mutex::new(None));
    let ledger = UsageLedger::new(Box::new(Captured(captured.clone())), UsageOptions::default())?;
    let calls: Vec<(f64, Measurement, Option<String>)> = reported
        .iter()
        .map(|(at, usage)| {
            let measured = Measurement {
                input_tokens: usage["input"].as_f64(),
                output_tokens: usage["output"].as_f64(),
                total_tokens: usage["totalTokens"].as_f64(),
                cost: usage["cost"]["total"].as_f64(),
                wall_ms: Some(0.0),
            };
            (*at as f64, measured, None)
        })
        .collect();
    ledger.import_history(usage_root, "pi", &calls)?;
    let document = jsrt::sync::lock(&captured).take();
    if let Some(document) = document {
        commits.commit(session, &[Write::set(USAGE_ROOT.namespace, USAGE_ROOT.key, document)])?;
    }
    Ok(())
}

fn build_import_metadata_writes(manager: &SessionManager, pi_entries: &[Value], tree: &PiTree) -> Vec<Write> {
    let leaf = manager
        .leaf_id()
        .and_then(Value::as_str)
        .filter(|leaf| tree.by_id.contains_key(*leaf));
    let mut writes = vec![Write::set(BRANCH_TIP, "main", leaf.map_or(Value::Null, Value::from))];
    for entry in pi_entries {
        if let Some(label) = manager.label(id_of(entry)) {
            writes.push(Write::set(ENTRY_LABEL, id_of(entry), label.clone()));
        }
    }
    if let Some(name) = manager.session_name() {
        writes.push(Write::set(SESSION_NAME, "", json!(name)));
    }
    let context = manager.build_session_context();
    if let Some(model) = &context.model {
        let mut configuration = Map::new();
        configuration.insert(
            "model".into(),
            json!({"provider": model["provider"], "modelId": model["modelId"]}),
        );
        if let Some(level) = &context.thinking_level {
            configuration.insert("thinkingLevel".into(), level.clone());
        }
        configuration.insert("activeToolNames".into(), json!([]));
        writes.push(Write::set(LANE_CONFIG, "main", Value::Object(configuration)));
        writes.push(Write::set(
            LANE_STATE,
            "main",
            json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}),
        ));
    }
    writes
}

/// What an export wrote (`ExportNativeSessionToPiResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportResult {
    pub session_id: String,
    pub path: PathBuf,
    /// Pi entries written (without the header).
    pub entries: usize,
}

/// `piLabelsOf`: the labels Pi's label entries resolve to, in first-set order.
fn pi_labels_of(entries: &[Value]) -> Vec<(String, String)> {
    let mut labels: Vec<(String, String)> = Vec::new();
    for entry in entries.iter().filter(|e| type_of(e) == "label") {
        let target = str_of(entry, "targetId").unwrap_or_default().to_string();
        let label = entry
            .get("label")
            .filter(|l| jsrt::value::truthy(Some(l)))
            .and_then(Value::as_str);
        match label {
            Some(label) => match labels.iter_mut().find(|(t, _)| *t == target) {
                Some((_, existing)) => *existing = label.to_string(),
                None => labels.push((target, label.to_string())),
            },
            None => labels.retain(|(t, _)| *t != target),
        }
    }
    labels
}

/// `piNameOf`: the last `session_info` entry's trimmed name.
fn pi_name_of(entries: &[Value]) -> Option<String> {
    let entry = entries.iter().rev().find(|e| type_of(e) == "session_info")?;
    entry
        .get("name")
        .and_then(Value::as_str)
        .map(jsrt::string::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

struct PiExportBuilder {
    entries: Vec<Value>,
    ids: HashSet<String>,
    leaf: Option<String>,
    stamp: Value,
}

impl PiExportBuilder {
    fn append(&mut self, entry: Value, kind: &str) {
        let mut id = format!("ultron-{kind}");
        let mut counter = 1;
        while self.ids.contains(&id) {
            id = format!("ultron-{kind}-{counter}");
            counter += 1;
        }
        self.ids.insert(id.clone());
        let mut out = entry.as_object().cloned().unwrap_or_default();
        out.insert("id".into(), json!(id));
        out.insert("parentId".into(), self.leaf.clone().map_or(Value::Null, Value::from));
        out.insert("timestamp".into(), self.stamp.clone());
        self.entries.push(Value::Object(out));
        self.leaf = Some(id);
    }

    fn align_leaf(&mut self, converted: crate::pi_view::NativeAsPi, natives: &[Value]) {
        if let Some(tip) = &converted.tip_id
            && converted.leaf_id != self.leaf
        {
            if natives.iter().any(|e| str_of(e, "parentId") == Some(tip.as_str())) {
                self.leaf = converted.leaf_id;
                self.append(
                    json!({"type": "custom", "customType": PI_LEAF_MARKER_CUSTOM_TYPE}),
                    "leaf",
                );
            } else {
                let items = converted.tip_items;
                let moved: HashSet<String> = items.iter().map(|item| id_of(item).to_string()).collect();
                self.entries.retain(|e| !moved.contains(id_of(e)));
                self.entries.extend(items);
                self.leaf = converted.leaf_id;
            }
        }
        if let Some(found) = self.entries.iter().find(|e| Some(id_of(e)) == self.leaf.as_deref()) {
            self.stamp = found["timestamp"].clone();
        }
    }

    fn append_label_diffs(&mut self, state: &session_journal::state::State, native_ids: &HashSet<&str>) {
        let pi_labels = pi_labels_of(&self.entries);
        let native_labels: Vec<(String, String)> = state
            .scan_values(ENTRY_LABEL, "")
            .into_iter()
            .filter_map(|v| v.value.as_str().map(|l| (v.key.clone(), l.to_string())))
            .collect();
        let mut targets: Vec<String> = pi_labels.iter().map(|(t, _)| t.clone()).collect();
        for (target, _) in &native_labels {
            if !targets.contains(target) {
                targets.push(target.clone());
            }
        }
        for target in targets {
            let label = native_labels.iter().find(|(t, _)| *t == target).map(|(_, l)| l.clone());
            let pi = pi_labels.iter().find(|(t, _)| *t == target).map(|(_, l)| l.clone());
            if pi == label || !native_ids.contains(target.as_str()) {
                continue;
            }
            let mut entry = json!({"type": "label", "targetId": target});
            if let Some(label) = label {
                entry["label"] = json!(label);
            }
            self.append(entry, "label");
        }
    }

    fn append_lane_config_diffs(&mut self, configuration: &Value) {
        let leaf_value = self.leaf.clone().map(Value::from);
        let resolved = build_session_context(
            &self.entries,
            match &leaf_value {
                Some(id) => Leaf::Id(id),
                None => Leaf::Root,
            },
        );
        let model = &configuration["model"];
        let resolved_model = resolved.model.clone().unwrap_or(Value::Null);
        if resolved_model.get("provider") != model.get("provider")
            || resolved_model.get("modelId") != model.get("modelId")
        {
            let mut entry = json!({"type": "model_change"});
            for (key, value) in model.as_object().into_iter().flatten() {
                entry[key] = value.clone();
            }
            self.append(entry, "model");
        }
        if resolved.thinking_level.as_ref() != configuration.get("thinkingLevel") {
            let mut entry = json!({"type": "thinking_level_change"});
            if let Some(level) = configuration.get("thinkingLevel") {
                entry["thinkingLevel"] = level.clone();
            }
            self.append(entry, "thinking");
        }
    }
}

/// `exportNativeSessionToPi`: the native session at `session_path` (in `<sessionsRoot>/<cwd-dir>/`) written whole
/// as a Pi session at `output_path`, which must not exist. Pi resumes at its last entry, the native `main` tip;
/// labels, the name, and the `main` lane's model and thinking level that differ from what the entries say are
/// appended after it.
pub fn export_native_session_to_pi(session_path: &Path, output_path: &Path) -> Result<ExportResult, String> {
    let path = PathBuf::from(session_journal::catalog::absolute(&session_path.display().to_string()));
    let root = path.parent().and_then(Path::parent).unwrap_or(Path::new("/"));
    let repo = SessionRepo::new(root, SystemClock::shared());
    let metadata = repo
        .list(None)
        .into_iter()
        .find(|m| m.path == path)
        .ok_or_else(|| format!("Not a native session in its session directory: {}", path.display()))?;
    let session = repo.open(&metadata)?;
    let mut converted = native_session_as_pi(&session, true);
    let state = session.state();
    let natives = state.entries();
    let native_ids: HashSet<&str> = natives.iter().map(id_of).collect();
    let newest = natives
        .iter()
        .filter_map(|e| e["timestamp"].as_i64())
        .fold(metadata.created_at, i64::max);
    let entries = std::mem::take(&mut converted.entries);
    let mut builder = PiExportBuilder {
        ids: entries.iter().map(|e| id_of(e).to_string()).collect(),
        leaf: entries.last().map(|e| id_of(e).to_string()),
        stamp: json!(jsrt::date::iso_string(newest)),
        entries,
    };
    builder.align_leaf(converted, natives);
    builder.append_label_diffs(state, &native_ids);
    let name = session.name();
    if pi_name_of(&builder.entries) != name {
        builder.append(
            json!({"type": "session_info", "name": name.unwrap_or_default()}),
            "name",
        );
    }
    if let Some(configuration) = session.value(LANE_CONFIG, "main").cloned() {
        builder.append_lane_config_diffs(&configuration);
    }
    let header = json!({
        "type": "session", "version": PI_SESSION_VERSION, "id": metadata.id,
        "timestamp": jsrt::date::iso_string(metadata.created_at), "cwd": metadata.cwd,
    });
    let output = write_pi_export_file(output_path, &header, &builder.entries)?;
    Ok(ExportResult {
        session_id: metadata.id,
        path: output,
        entries: builder.entries.len(),
    })
}

fn write_pi_export_file(output_path: &Path, header: &Value, entries: &[Value]) -> Result<PathBuf, String> {
    let output = PathBuf::from(session_journal::catalog::absolute(&output_path.display().to_string()));
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut content = String::new();
    for line in std::iter::once(header).chain(entries.iter()) {
        content.push_str(&serde_json::to_string(line).map_err(|e| e.to_string())?);
        content.push('\n');
    }
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output)
        .map_err(|e| export_open_error(&e, &output))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(output)
}

/// Node's `open(..., "wx")` error text.
fn export_open_error(error: &std::io::Error, path: &Path) -> String {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return format!("EEXIST: file already exists, open '{}'", path.display());
    }
    format!("{error}: open '{}'", path.display())
}
