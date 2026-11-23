//! Path, snapshot, shell-write detection and statistics support types for [`crate::file_hooks`].

use fancy_regex::Regex;
use jsrt::sync;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::time::Duration;

/// A picture of a working directory: relative path (with `/`) to a metadata signature (workspace-snapshot.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub root: PathBuf,
    pub entries: BTreeMap<String, String>,
    pub complete: bool,
}

pub type Snapshotter = Arc<dyn Fn(&Path) -> Result<Snapshot, String> + Send + Sync>;

/// How long an after-cell check waits for work outside the cells still running (Loki's setup commit: 30 s for
/// the commit and its hooks, then a picture of the repository).
pub const OUTSIDE_WAIT: Duration = Duration::from_secs(60);

/// Files the session's own work changed outside the cells: Loki's setup commit, whose pre-commit hooks may rewrite
/// files (a formatter rewrote `.loki/loki.json`, which the next after-cell check then reported as that cell's
/// `protected file`). An after-cell check waits for such work and leaves out the files it changed, unless the cell
/// changed them again.
#[derive(Clone, Default)]
pub struct OutsideChanges {
    state: Arc<(Mutex<OutsideState>, Condvar)>,
}

#[derive(Default)]
struct OutsideState {
    running: usize,
    /// Absolute path -> its signature once the work finished (None: the work deleted it).
    after: HashMap<PathBuf, Option<String>>,
}

/// Work outside the cells that has begun ([`OutsideChanges::begin`]); dropping it without [`Self::record`] ends it
/// with nothing recorded.
pub struct OutsideWork {
    changes: OutsideChanges,
}

impl OutsideChanges {
    /// Work outside the cells begins now: after-cell checks wait for it from this moment.
    pub fn begin(&self) -> OutsideWork {
        sync::lock(&self.state.0).running += 1;
        OutsideWork { changes: self.clone() }
    }

    /// Wait (at most `limit`) for the work that has begun to end.
    pub(crate) fn wait(&self, limit: Duration) {
        let (state, ended) = &*self.state;
        let mut state = sync::lock(state);
        let end = std::time::Instant::now() + limit;
        while state.running > 0 {
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            state = ended
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Whether `path` (absolute) with `signature` (None: missing) is as the work outside the cells left it.
    pub(crate) fn explains(&self, path: &Path, signature: Option<&String>) -> bool {
        sync::lock(&self.state.0)
            .after
            .get(path)
            .is_some_and(|left| left.as_ref() == signature)
    }
}

impl OutsideWork {
    /// Run `work`, recording what it changed between pictures of `root` taken by `snapshot` before and after it.
    /// Picture only what the work changes (Loki's setup: `.loki/`): a cell's own write elsewhere in that time
    /// would otherwise be taken for the work's and go unchecked.
    pub fn record<R>(self, root: &Path, snapshot: &Snapshotter, work: impl FnOnce() -> R) -> R {
        let before = snapshot(root);
        let result = work();
        if let (Ok(before), Ok(after)) = (before, snapshot(root)) {
            let (added, modified, deleted, _) = diff_snapshots(&before, &after);
            let mut state = sync::lock(&self.changes.state.0);
            for relative in added.iter().chain(&modified) {
                let signature = after.entries.get(relative).cloned();
                state.after.insert(resolve_lexical(&after.root, relative), signature);
            }
            for relative in &deleted {
                state.after.insert(resolve_lexical(&after.root, relative), None);
            }
        }
        result
    }
}

impl Drop for OutsideWork {
    fn drop(&mut self) {
        let (state, ended) = &*self.changes.state;
        let mut state = sync::lock(state);
        state.running = state.running.saturating_sub(1);
        ended.notify_all();
    }
}

pub const DEFAULT_AFTER_CELL_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_BEFORE_WRITE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// One guard's time and outcomes, for the RLM pane and diagnostics (`GuardStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardStats {
    pub name: String,
    /// Before-write (and before-shell) checks run, those that blocked, those that timed out or failed.
    pub checks: u64,
    pub blocked: u64,
    pub unchecked: u64,
    /// After-cell checks run, and how many reported findings to fix (or could not check).
    pub after_checks: u64,
    pub after_findings: u64,
    /// Time spent in the guard: in all, and since the current root turn began.
    pub total_ms: u64,
    pub turn_ms: u64,
}

impl GuardStats {
    /// The TS object (`fileHooks.stats()` entries).
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "checks": self.checks,
            "blocked": self.blocked,
            "unchecked": self.unchecked,
            "afterChecks": self.after_checks,
            "afterFindings": self.after_findings,
            "totalMs": self.total_ms,
            "turnMs": self.turn_ms,
        })
    }
}

/// Every guard outcome, for logs (`GuardRecord`, ULTRON_LOKI_LOG).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRecord {
    pub guard: String,
    /// `before_write`, `before_shell` or `after_cell`.
    pub phase: &'static str,
    /// `allowed`, `blocked`, `unchecked`, `clean`, `advisory` or `findings`.
    pub outcome: &'static str,
    pub ms: u64,
    pub paths: Vec<String>,
    pub lane: String,
    pub detail: Option<String>,
}

impl GuardRecord {
    /// The record as TS writes it (`detail` left out when unset).
    pub fn to_json(&self) -> Value {
        let mut record = json!({
            "guard": self.guard,
            "phase": self.phase,
            "outcome": self.outcome,
            "ms": self.ms,
            "paths": self.paths,
            "lane": self.lane,
        });
        if let Some(detail) = &self.detail {
            record["detail"] = json!(detail);
        }
        record
    }
}

pub type OnRecord = Arc<dyn Fn(&GuardRecord) + Send + Sync>;
/// Milliseconds on some clock (TS `now`, default `Date.now`): only differences are used.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

static DEV_NULL: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>>?\s*/dev/null(?![A-Za-z0-9_])"));
static FD_DUP: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>&[0-9]+"));
static WRITERS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        re(r"(^|[^<>-])>{1,2}\|?(?!&)"),
        re(r"(^|[\s;&|(])(tee|cp|mv|install)\s"),
        re(r"(^|[\s;&|(])sed\s+(?:[^|;&]*\s)?(-i|--in-place)"),
        re(r"(^|[\s;&|(])perl\s+(?:[^|;&]*\s)?-[a-zA-Z]*i"),
        re(r"(^|[\s;&|(])dd\s[^|;&]*\bof="),
    ]
});

static PROTECTED_MUTATORS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        re(r"(^|[\s;&|(])(rm|unlink|truncate|chmod|chown|ln|rsync|patch)\s"),
        re(r"(^|[\s;&|(])(python3?|python)\s+[^|;&]*-c\b"),
        re(r"(^|[\s;&|(])(node|ruby|perl)\s+[^|;&]*-e\b"),
        re(r"(^|[\s;&|(])((sh|bash|zsh)\s+[^|;&]*-c\b|eval\s)"),
    ]
});

/// `mayWriteFiles`: whether a command may write a file, cheaply and generously (a redirect, `tee`, `sed -i`,
/// `perl -i`, `cp`/`mv`/`install`, `dd of=`). Redirects to `/dev/null` and fd duplications do not count.
pub fn may_write_files(command: &str) -> bool {
    let text = DEV_NULL.replace_all(command, " ");
    let text = FD_DUP.replace_all(&text, " ");
    WRITERS
        .iter()
        .any(|pattern| pattern.is_match(text.as_ref()).unwrap_or(false))
}

/// Whether a shell command may mutate or delete a protected path (all `may_write_files` patterns plus
/// destructive file commands and inline interpreters that can open/write files without shell redirects).
pub(crate) fn may_mutate_protected(command: &str) -> bool {
    may_write_files(command)
        || PROTECTED_MUTATORS
            .iter()
            .any(|pattern| pattern.is_match(command).unwrap_or(false))
}

/// `path.resolve(cwd, path)`: lexical, `..` and `.` removed.
pub fn resolve_lexical(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            _ => {}
        }
    }
    out
}

/// `path.relative(from, to)` when `to` is inside `from`, as a `/` path; None for `from` itself or outside it.
pub fn inside(from: &Path, to: &Path) -> Option<String> {
    let relative = to.strip_prefix(from).ok()?.to_string_lossy().into_owned();
    (!relative.is_empty() && !relative.starts_with("..")).then_some(relative)
}

/// `diffSnapshots`: added, modified and deleted paths (each sorted), and whether both pictures were complete.
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> (Vec<String>, Vec<String>, Vec<String>, bool) {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    for (path, value) in &after.entries {
        match before.entries.get(path) {
            None => added.push(path.clone()),
            Some(previous) if previous != value => modified.push(path.clone()),
            Some(_) => {}
        }
    }
    let deleted = before
        .entries
        .keys()
        .filter(|path| !after.entries.contains_key(*path))
        .cloned()
        .collect();
    (added, modified, deleted, before.complete && after.complete)
}

/// The first `limit` UTF-16 code units of `text` (JavaScript's `slice(0, limit)`), and how many there were in all.
pub(crate) fn js_cut(text: &str, limit: usize) -> (String, usize) {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= limit {
        return (text.to_string(), units.len());
    }
    (String::from_utf16_lossy(&units[..limit]), units.len())
}
