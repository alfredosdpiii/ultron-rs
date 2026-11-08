//! Generic file-write hooks for the RLM REPL (ultron/file-hooks.ts):
//!
//! 1. Before a write (blocking): the kernel's `edit()`/`write()` send each proposed write to the host before touching
//!    the file (`files.before_write`). A guard that blocks makes `write()` raise ValueError with its diagnostic
//!    (runtime.py `_check_write`); a guard that fails lets the write proceed with a visible note: it fails open,
//!    never silently.
//!    Before any guard, a write into a protected path ([`FileHooks::protect`]: the profile's settings, keys and
//!    skills, which are the user's to change) is blocked, and so is a shell command that may write files and names
//!    one.
//! 2. After a cell (advisory): files a cell changed by other means (`bash('sed -i ...')`, `Path.write_text`) are found
//!    by comparing workspace snapshots taken between cells, minus the checked writes whose content is unchanged.
//!    Guards' `after_cell_changes` run in the background, so a cell returns as fast as before, and what they report
//!    reaches the model with the lane's next cell result ([`FileHooks::take_pending`]).
//! 3. Before a shell command (blocking): commands that may write a file go to the guards before they run; a guard
//!    that blocks makes the host refuse the command ("bash command was not run: ...").

use crate::Guard;
use fancy_regex::Regex;
use jsrt::sync;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::time::Duration;

/// A guard's answer: block with a reason, or allow with an optional note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Block(String),
    Allow(Option<String>),
}

/// Files one cell changed (absolute paths).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellChanges {
    /// Added or modified files no before-write check saw (or whose checked content was changed again).
    pub files: Vec<PathBuf>,
    /// Files written through a checked `edit()`/`write()` whose content is what was checked.
    pub checked: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    /// False when a snapshot hit its size bound, so some changes may be missing.
    pub complete: bool,
}

/// Whether an after-cell report holds something to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterOutcome {
    /// Nothing, or only a note.
    Clean,
    /// Only for-your-information lines.
    Advisory,
    /// Something to fix.
    Findings,
}

/// What an after-cell check has to say (`AfterCellReport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCellReport {
    /// Text for the model, delivered with the lane's next cell result.
    pub message: Option<String>,
    pub outcome: AfterOutcome,
}

/// One registered guard (Loki). `Err` means the guard could not check (the write proceeds unchecked).
pub trait FileWriteGuard: Send + Sync {
    /// Shown in notes and statistics ("Loki").
    fn name(&self) -> &str;
    /// False while the guard has nothing to do (checked per call; file-hooks.ts `enabled`).
    fn enabled(&self) -> bool {
        true
    }
    /// Whether the guard checks proposed writes / shell commands at all (TS: whether `beforeWrite` / `beforeShell`
    /// is defined); only those calls are counted in its statistics.
    fn checks_writes(&self) -> bool {
        true
    }
    fn checks_shell(&self) -> bool {
        true
    }
    /// Per-call time limit of `before_write` and `before_shell` (default 5 s); `None` waits for the answer. A guard
    /// that does not answer in time lets the write (or command) proceed with a note.
    fn timeout(&self) -> Option<Duration> {
        Some(DEFAULT_BEFORE_WRITE_TIMEOUT)
    }
    /// A proposed write of the cell on `lane`: the absolute, normalized path and the complete new text; `cwd` is the
    /// lane's directory.
    fn before_write(&self, _path: &Path, _content: &str, _cwd: &Path, _lane: &str) -> Result<Verdict, String> {
        Ok(Verdict::Allow(None))
    }
    /// A shell command that may write files ([`may_write_files`]), before it runs in `cwd`.
    fn before_shell(&self, _command: &str, _cwd: &Path) -> Result<Verdict, String> {
        Ok(Verdict::Allow(None))
    }
    /// Whether the guard wants the files each cell changed (snapshots are taken only while one does); checked per
    /// cell.
    fn watches_cells(&self) -> bool {
        false
    }
    /// The files a cell on `lane` (working in `cwd`) changed by other means than checked writes.
    fn after_cell_changes(
        &self,
        _changes: &CellChanges,
        _cwd: &Path,
        _lane: &str,
    ) -> Result<Option<AfterCellReport>, String> {
        Ok(None)
    }
    /// How long `after_cell_changes` may take (default 120 s).
    fn after_timeout(&self) -> Duration {
        DEFAULT_AFTER_CELL_TIMEOUT
    }
}

/// Something that wants the files each cell changed without being a guard: no verdicts, no statistics, no records.
/// Its lines are delivered verbatim with the lane's next cell result (memory pointers, `kernel_apis::memory_pointers`).
pub trait CellObserver: Send + Sync {
    /// False while the observer has nothing to do (checked per cell, like a guard's `watches_cells`).
    fn enabled(&self) -> bool {
        true
    }
    /// The files a cell on `lane` (working in `cwd`) changed, checked writes included; lines for the model.
    fn cell_changed(&self, changes: &CellChanges, cwd: &Path, lane: &str) -> Vec<String>;
    /// A shell command a cell on `lane` is about to run: `Some(reason)` refuses it, with the reason for the model.
    fn before_shell(&self, _command: &str, _lane: &str) -> Option<String> {
        None
    }
}

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
    fn wait(&self, limit: Duration) {
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
    fn explains(&self, path: &Path, signature: Option<&String>) -> bool {
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

/// `Math.round(ms / 100) / 10` as JavaScript prints it ("5", "0.2").
fn js_seconds(timeout: Duration) -> String {
    jsrt::number::to_string(jsrt::number::round(timeout.as_millis() as f64 / 100.0) / 10.0)
}

/// How one timed guard call ended.
enum Answer<T> {
    Value(T),
    Error(String),
    TimedOut,
}

/// Run `call` on its own thread and wait at most `timeout` (file-hooks.ts `#timed`): a call that does not answer in
/// time is left to finish on its own.
fn timed<T: Send + 'static>(
    timeout: Option<Duration>,
    call: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Answer<T> {
    let (sender, answer) = channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    let received = match timeout {
        Some(timeout) => answer.recv_timeout(timeout).ok(),
        None => answer.recv().ok(),
    };
    match received {
        Some(Ok(value)) => Answer::Value(value),
        Some(Err(error)) => Answer::Error(error),
        None => Answer::TimedOut,
    }
}

/// A guard call's answer, once it comes (or its time is up).
type Pending = std::sync::mpsc::Receiver<Answer<Verdict>>;

/// A registered guard and its statistics.
#[derive(Clone)]
struct Registered {
    guard: Arc<dyn FileWriteGuard>,
    stats: Arc<Mutex<GuardStats>>,
}
/// Findings delivered with one cell result are capped; the rest are summarized.
const MAX_PENDING_CHARS: usize = 6_000;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

// JavaScript's `\b` after `null` is "no ASCII word character next"; `\d` is ASCII.
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

/// `mayWriteFiles`: whether a command may write a file, cheaply and generously (a redirect, `tee`, `sed -i`,
/// `perl -i`, `cp`/`mv`/`install`, `dd of=`). Redirects to `/dev/null` and fd duplications do not count.
pub fn may_write_files(command: &str) -> bool {
    let text = DEV_NULL.replace_all(command, " ");
    let text = FD_DUP.replace_all(&text, " ");
    WRITERS
        .iter()
        .any(|pattern| pattern.is_match(text.as_ref()).unwrap_or(false))
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

/// The guards of one working directory (the session's, or a worktree subagent's), answering the host's
/// `files.before_write` and shell checks, and checking what each cell changed (`FileHooks`).
#[derive(Clone)]
pub struct FileHooks {
    inner: Arc<Inner>,
}

struct Inner {
    cwd: PathBuf,
    guards: Mutex<Vec<Registered>>,
    observers: Mutex<Vec<Arc<dyn CellObserver>>>,
    on_record: Mutex<Option<OnRecord>>,
    now: Mutex<Clock>,
    snapshot: Option<Snapshotter>,
    /// Checked writes since the last after-cell snapshot: absolute path -> sha256 of the checked content.
    checked: Mutex<HashMap<PathBuf, String>>,
    /// Findings not yet shown, per lane.
    pending: Mutex<HashMap<String, Vec<String>>>,
    /// The last snapshot of the directory (taken before the first watched cell, then after each).
    latest: Mutex<Option<Snapshot>>,
    /// Each lane's picture from the start of its running cell, which the cell's end is compared with: a lane is
    /// told only what changed while its own cell ran, and another lane's end does not absorb its changes. Changed
    /// only by the after-cell worker, in the order the cells started and ended.
    baselines: Mutex<HashMap<String, Snapshot>>,
    /// Changes another lane's cell end was given while this lane's cell ran (relative path -> its picture entry then,
    /// `None` for a deletion): a change goes to the first cell that ends after it, and another lane's running cell
    /// gets it only if that cell changes the file again.
    claimed: Mutex<HashMap<String, HashMap<String, Option<String>>>>,
    /// Lanes whose watched cell runs now.
    live: Mutex<HashSet<String>>,
    /// Files changed outside the cells, left out of their changes ([`FileHooks::ignore_outside`]).
    outside: Mutex<Option<OutsideChanges>>,
    /// Paths no cell may write ([`FileHooks::protect`]), and how a shell command names each.
    protected: Mutex<Vec<Protected>>,
    /// Cell starts and ends queued or being handled, and the worker that handles them one at a time.
    queued: Mutex<usize>,
    idle: Condvar,
    worker: Mutex<Option<Sender<CellEvent>>>,
}

/// A watched cell's start or end, handled by the after-cell worker in order.
enum CellEvent {
    /// Its picture when one was taken as it started (another lane's cell was running, or there was none yet);
    /// otherwise the latest picture is its baseline.
    Started(String, Option<Snapshot>),
    Ended(String),
}

/// A protected file or directory: its absolute path, and the text a shell command names it by (its path from the
/// home directory when it is inside it, which `~/`, `$HOME/` and absolute forms all contain).
struct Protected {
    path: PathBuf,
    named: String,
}

/// Who blocks a write into a protected path, in its reason and guard records.
const PROTECTOR: &str = "Ultron";

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
fn js_cut(text: &str, limit: usize) -> (String, usize) {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= limit {
        return (text.to_string(), units.len());
    }
    (String::from_utf16_lossy(&units[..limit]), units.len())
}

impl FileHooks {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        FileHooks {
            inner: Arc::new(Inner {
                cwd: cwd.into(),
                guards: Mutex::new(Vec::new()),
                observers: Mutex::new(Vec::new()),
                on_record: Mutex::new(None),
                now: Mutex::new({
                    let started = std::time::Instant::now();
                    Arc::new(move || started.elapsed().as_millis() as u64)
                }),
                snapshot: None,
                checked: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                latest: Mutex::new(None),
                baselines: Mutex::new(HashMap::new()),
                claimed: Mutex::new(HashMap::new()),
                live: Mutex::new(HashSet::new()),
                outside: Mutex::new(None),
                protected: Mutex::new(Vec::new()),
                queued: Mutex::new(0),
                idle: Condvar::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    /// Hooks that also check what each cell changed, picturing the directory with `snapshot`.
    pub fn with_snapshots(cwd: impl Into<PathBuf>, snapshot: Snapshotter) -> Self {
        let mut hooks = FileHooks::new(cwd);
        if let Some(inner) = Arc::get_mut(&mut hooks.inner) {
            inner.snapshot = Some(snapshot);
        }
        hooks
    }

    pub fn add(&mut self, guard: Box<dyn FileWriteGuard>) {
        let stats = GuardStats {
            name: guard.name().to_string(),
            ..GuardStats::default()
        };
        sync::lock(&self.inner.guards).push(Registered {
            guard: Arc::from(guard),
            stats: Arc::new(Mutex::new(stats)),
        });
    }

    /// No cell writes `paths` (files, or directories with everything in them) through `write()`/`edit()` or a
    /// shell command that names one: they are the user's to change (the profile's settings, keys and skills).
    /// `home` is the home directory a command may name them from.
    pub fn protect(&self, paths: Vec<PathBuf>, home: &Path) {
        let mut protected = Vec::new();
        for path in paths {
            let named = path.strip_prefix(home).unwrap_or(&path).to_string_lossy().into_owned();
            // Through a symlinked home or profile, too.
            if let Ok(real) = std::fs::canonicalize(&path).or_else(|_| {
                let parent = std::fs::canonicalize(path.parent().unwrap_or(&path))?;
                Ok::<_, std::io::Error>(parent.join(path.file_name().unwrap_or_default()))
            }) && real != path
            {
                protected.push(Protected {
                    path: real,
                    named: named.clone(),
                });
            }
            protected.push(Protected { path, named });
        }
        *sync::lock(&self.inner.protected) = protected;
    }

    /// The protected path `path` is in, if any (as written, and with its existing part's symlinks resolved).
    fn protected_by(&self, path: &Path) -> Option<PathBuf> {
        let real = path
            .ancestors()
            .find_map(|existing| {
                let resolved = std::fs::canonicalize(existing).ok()?;
                Some(resolved.join(path.strip_prefix(existing).ok()?))
            })
            .unwrap_or_else(|| path.to_path_buf());
        sync::lock(&self.inner.protected)
            .iter()
            .find(|p| {
                [path, real.as_path()]
                    .iter()
                    .any(|candidate| candidate.starts_with(&p.path))
            })
            .map(|p| p.path.clone())
    }

    /// The protected path a shell command that may write files names, if any.
    fn protected_in_command(&self, command: &str) -> Option<PathBuf> {
        sync::lock(&self.inner.protected)
            .iter()
            .find(|p| command.contains(&p.named))
            .map(|p| p.path.clone())
    }

    /// `observer` sees what each cell changed (after the guards); its lines go with the lane's next cell result.
    pub fn observe(&self, observer: Arc<dyn CellObserver>) {
        sync::lock(&self.inner.observers).push(observer);
    }

    /// Files `changes` records as changed outside the cells are not a cell's changes (unless it changed them
    /// again), and an after-cell check waits for that work to end first.
    pub fn ignore_outside(&self, changes: OutsideChanges) {
        *sync::lock(&self.inner.outside) = Some(changes);
    }

    /// Every guard outcome goes to `on_record` (ULTRON_LOKI_LOG).
    pub fn on_record(&self, on_record: OnRecord) {
        *sync::lock(&self.inner.on_record) = Some(on_record);
    }

    /// The clock guard times are measured with (tests).
    pub fn set_clock(&self, now: Clock) {
        *sync::lock(&self.inner.now) = now;
    }

    /// A new root turn began: per-turn times start from zero.
    pub fn begin_turn(&self) {
        for registered in sync::lock(&self.inner.guards).iter() {
            sync::lock(&registered.stats).turn_ms = 0;
        }
    }

    /// Each guard's statistics, in registration order.
    pub fn stats(&self) -> Vec<GuardStats> {
        sync::lock(&self.inner.guards)
            .iter()
            .map(|registered| sync::lock(&registered.stats).clone())
            .collect()
    }

    /// True when some guard wants to see changes after each cell (snapshots are taken only then).
    pub fn watches_cells(&self) -> bool {
        self.inner.watches_cells()
    }

    /// A cell is starting on `lane`: the directory as it is now is what its end is compared with. While no other
    /// lane's cell runs that is the latest picture (taken after the previous cell); otherwise a picture is taken now,
    /// so the other lane's changes made before this start are not this lane's.
    pub fn cell_started(&self, lane: &str) {
        let inner = &self.inner;
        if !inner.watches_cells() {
            return;
        }
        let concurrent = {
            let mut live = sync::lock(&inner.live);
            live.insert(lane.to_string());
            live.len() > 1
        };
        let picture = (concurrent || sync::lock(&inner.latest).is_none())
            .then(|| inner.picture())
            .flatten();
        self.send(CellEvent::Started(lane.to_string(), picture));
    }

    /// A cell on `lane` ended: in the background, diff the directory against the lane's picture from the cell's
    /// start and give the guards the files changed by other means than checked writes. Returns at once;
    /// [`Self::settled`] waits.
    pub fn cell_ended(&self, lane: &str) {
        let inner = &self.inner;
        let watched = sync::lock(&inner.live).remove(lane);
        if !inner.watches_cells() {
            return;
        }
        if !watched {
            if sync::lock(&inner.latest).is_none() {
                // No picture yet (a guard became interested mid-session): this cell's changes cannot be told apart;
                // the next cell's are.
                let picture = inner.picture();
                let mut latest = sync::lock(&inner.latest);
                if latest.is_none() {
                    *latest = picture;
                }
                return;
            }
            // Its start was not seen: compared with the latest picture.
            self.send(CellEvent::Started(lane.to_string(), None));
        }
        self.send(CellEvent::Ended(lane.to_string()));
    }

    fn send(&self, event: CellEvent) {
        let inner = &self.inner;
        *sync::lock(&inner.queued) += 1;
        let mut worker = sync::lock(&inner.worker);
        let sender = worker.get_or_insert_with(|| {
            let (sender, events) = channel::<CellEvent>();
            let inner = Arc::downgrade(&self.inner);
            std::thread::spawn(move || {
                for event in events {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    match event {
                        CellEvent::Started(lane, picture) => inner.started(lane, picture),
                        CellEvent::Ended(lane) => inner.after_cell(&lane),
                    }
                    let mut queued = sync::lock(&inner.queued);
                    *queued -= 1;
                    inner.idle.notify_all();
                }
            });
            sender
        });
        if sender.send(event).is_err() {
            *sync::lock(&inner.queued) -= 1;
        }
    }

    /// Wait for the after-cell checks already started.
    pub fn settled(&self) {
        let mut queued = sync::lock(&self.inner.queued);
        while *queued > 0 {
            queued = self.inner.idle.wait(queued).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Findings from after-cell checks not yet shown to `lane`, as one block of text (and forget them).
    pub fn take_pending(&self, lane: &str) -> Option<String> {
        let pending = sync::lock(&self.inner.pending).remove(lane)?;
        if pending.is_empty() {
            return None;
        }
        let text = pending.join("\n");
        let (head, total) = js_cut(&text, MAX_PENDING_CHARS);
        Some(if total > MAX_PENDING_CHARS {
            format!("{head}\n… ({} more characters of findings)", total - MAX_PENDING_CHARS)
        } else {
            text
        })
    }

    /// A path for notes: relative to the session directory when inside it.
    fn display(&self, path: &Path) -> String {
        self.inner.display(path)
    }

    /// The refusal of a write into `protected` (recorded like a guard's block).
    fn refuse_protected(&self, lane: &str, phase: &'static str, protected: &Path) -> String {
        let reason = format!(
            "[{PROTECTOR}] {} is the user's to change (the profile's settings, keys and skills): a cell may not write \
             it. Ask the user to make the change.",
            protected.display()
        );
        self.inner.record(GuardRecord {
            guard: PROTECTOR.into(),
            phase,
            outcome: "blocked",
            ms: 0,
            paths: vec![protected.display().to_string()],
            lane: lane.to_string(),
            detail: Some(reason.clone()),
        });
        reason
    }

    fn guards(&self) -> Vec<Registered> {
        sync::lock(&self.inner.guards).clone()
    }
}

impl Inner {
    fn watching(guard: &dyn FileWriteGuard) -> bool {
        guard.enabled() && guard.watches_cells()
    }

    fn watches_cells(&self) -> bool {
        self.snapshot.is_some()
            && (sync::lock(&self.guards).iter().any(|r| Inner::watching(&*r.guard))
                || sync::lock(&self.observers).iter().any(|observer| observer.enabled()))
    }

    fn now(&self) -> u64 {
        let clock = sync::lock(&self.now).clone();
        clock()
    }

    fn record(&self, record: GuardRecord) {
        let on_record = sync::lock(&self.on_record).clone();
        if let Some(on_record) = on_record {
            on_record(&record);
        }
    }

    /// A path for records: relative to the session directory when inside it.
    fn display(&self, path: &Path) -> String {
        inside(&self.cwd, path).unwrap_or_else(|| path.to_string_lossy().into_owned())
    }

    fn picture(&self) -> Option<Snapshot> {
        self.snapshot.as_ref().and_then(|snapshot| snapshot(&self.cwd).ok())
    }

    /// A lane's cell started: its baseline is the picture taken then, else the latest one.
    fn started(&self, lane: String, picture: Option<Snapshot>) {
        let mut latest = sync::lock(&self.latest);
        let Some(baseline) = picture.or_else(|| latest.clone()) else {
            return;
        };
        latest.get_or_insert_with(|| baseline.clone());
        sync::lock(&self.claimed).remove(&lane);
        sync::lock(&self.baselines).insert(lane, baseline);
    }

    /// `#afterCell`: diff against the lane's picture from its cell's start, then ask every watching guard (each
    /// within its time limit).
    fn after_cell(&self, lane: &str) {
        let before = sync::lock(&self.baselines).remove(lane);
        let outside = sync::lock(&self.outside).clone();
        if let Some(outside) = &outside {
            outside.wait(OUTSIDE_WAIT);
        }
        let after = self.picture();
        if let Some(after) = &after {
            *sync::lock(&self.latest) = Some(after.clone());
        }
        let (Some(before), Some(after)) = (before, after) else {
            return;
        };
        if before.root != after.root {
            return;
        }
        let (mut added, mut modified, mut deleted, complete) = diff_snapshots(&before, &after);
        {
            let mut claimed = sync::lock(&self.claimed);
            if let Some(theirs) = claimed.remove(lane) {
                let unchanged = |relative: &String| theirs.get(relative) == Some(&after.entries.get(relative).cloned());
                added.retain(|relative| !unchanged(relative));
                modified.retain(|relative| !unchanged(relative));
                deleted.retain(|relative| !unchanged(relative));
            }
            for other in sync::lock(&self.baselines).keys() {
                let theirs = claimed.entry(other.clone()).or_default();
                for relative in added.iter().chain(&modified).chain(&deleted) {
                    theirs.insert(relative.clone(), after.entries.get(relative).cloned());
                }
            }
        }
        if let Some(outside) = &outside {
            let theirs = |relative: &String| {
                outside.explains(&resolve_lexical(&after.root, relative), after.entries.get(relative))
            };
            added.retain(|relative| !theirs(relative));
            modified.retain(|relative| !theirs(relative));
            deleted.retain(|relative| !theirs(relative));
        }
        // Checked writes outside the session directory (a sibling worktree) are in no snapshot; they are changes too.
        let elsewhere: Vec<PathBuf> = {
            let mut checked = sync::lock(&self.checked);
            let outside: Vec<PathBuf> = checked
                .keys()
                .filter(|p| inside(&after.root, p).is_none())
                .cloned()
                .collect();
            outside
                .into_iter()
                .filter(|path| {
                    let approved = checked.remove(path);
                    approved.is_some_and(|approved| {
                        std::fs::read(path).is_ok_and(|now| jsrt::hash::sha256_hex(&now) == approved)
                    })
                })
                .collect()
        };
        if added.is_empty() && modified.is_empty() && deleted.is_empty() && elsewhere.is_empty() {
            return;
        }
        let mut changes = CellChanges {
            complete,
            checked: elsewhere,
            ..CellChanges::default()
        };
        for relative in added.iter().chain(&modified) {
            let path = resolve_lexical(&after.root, relative);
            let approved = sync::lock(&self.checked).remove(&path);
            if let Some(approved) = approved
                && std::fs::read(&path).is_ok_and(|current| jsrt::hash::sha256_hex(&current) == approved)
            {
                changes.checked.push(path);
                continue;
            }
            changes.files.push(path);
        }
        for relative in &deleted {
            let path = resolve_lexical(&after.root, relative);
            sync::lock(&self.checked).remove(&path);
            changes.deleted.push(path);
        }
        let guards: Vec<Registered> = sync::lock(&self.guards)
            .iter()
            .filter(|r| Inner::watching(&*r.guard))
            .cloned()
            .collect();
        let paths: Vec<String> = changes
            .files
            .iter()
            .chain(&changes.checked)
            .map(|path| self.display(path))
            .collect();
        let changes = Arc::new(changes);
        // Every watching guard starts at once; each is awaited with its own limit.
        let calls: Vec<_> = guards
            .into_iter()
            .map(|registered| {
                let started = self.now();
                let (task, cwd, lane_name, checked) = (
                    registered.guard.clone(),
                    self.cwd.clone(),
                    lane.to_string(),
                    changes.clone(),
                );
                let limit = registered.guard.after_timeout();
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(Some(limit), move || {
                        task.after_cell_changes(&checked, &cwd, &lane_name)
                    }));
                });
                (registered, started, receiver)
            })
            .collect();
        let mut found = Vec::new();
        for (registered, started, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let (text, outcome) = match answer {
                Answer::TimedOut => (
                    Some(format!("{name} did not finish checking the files this cell changed.")),
                    "unchecked",
                ),
                Answer::Error(error) => (
                    Some(format!("{name} could not check the files this cell changed: {error}")),
                    "unchecked",
                ),
                Answer::Value(None) => (None, "clean"),
                Answer::Value(Some(report)) => (
                    report.message.filter(|m| !m.is_empty()),
                    match report.outcome {
                        AfterOutcome::Clean => "clean",
                        AfterOutcome::Advisory => "advisory",
                        AfterOutcome::Findings => "findings",
                    },
                ),
            };
            {
                let mut stats = sync::lock(&registered.stats);
                stats.total_ms += ms;
                stats.turn_ms += ms;
                stats.after_checks += 1;
                if matches!(outcome, "findings" | "unchecked") {
                    stats.after_findings += 1;
                }
            }
            self.record(GuardRecord {
                guard: name.clone(),
                phase: "after_cell",
                outcome,
                ms,
                paths: paths.clone(),
                lane: lane.to_string(),
                detail: text.clone(),
            });
            if let Some(text) = text {
                found.push(format!("[{name}] {text}"));
            }
        }
        let observers: Vec<Arc<dyn CellObserver>> = sync::lock(&self.observers)
            .iter()
            .filter(|observer| observer.enabled())
            .cloned()
            .collect();
        for observer in observers {
            found.extend(observer.cell_changed(&changes, &self.cwd, lane));
        }
        if !found.is_empty() {
            sync::lock(&self.pending)
                .entry(lane.to_string())
                .or_default()
                .extend(found);
        }
    }
}

impl Guard for FileHooks {
    /// `beforeShell` of a command on `lane`: commands that may write no file, or with no shell guard, are allowed
    /// at once. `Some(reasons)` blocks it. Notes (a guard that could not check, advise-only refusals) are not shown
    /// for shell commands.
    fn before_shell(&self, lane: &str, command: &str) -> Option<String> {
        let observers: Vec<Arc<dyn CellObserver>> = sync::lock(&self.inner.observers)
            .iter()
            .filter(|observer| observer.enabled())
            .cloned()
            .collect();
        if let Some(reason) = observers
            .iter()
            .find_map(|observer| observer.before_shell(command, lane))
        {
            return Some(reason);
        }
        if let Some(path) = may_write_files(command)
            .then(|| self.protected_in_command(command))
            .flatten()
        {
            return Some(self.refuse_protected(lane, "before_shell", &path));
        }
        let guards: Vec<Registered> = self
            .guards()
            .into_iter()
            .filter(|r| r.guard.checks_shell() && r.guard.enabled())
            .collect();
        if guards.is_empty() || !may_write_files(command) {
            return None;
        }
        let display = if command.encode_utf16().count() > 80 {
            format!("{}...", js_cut(command, 77).0)
        } else {
            command.to_string()
        };
        let calls: Vec<(Registered, u64, Pending)> = guards
            .into_iter()
            .map(|registered| {
                let started = self.inner.now();
                let timeout = registered.guard.timeout();
                let (guard, command, cwd) = (registered.guard.clone(), command.to_string(), self.inner.cwd.clone());
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(timeout, move || guard.before_shell(&command, &cwd)));
                });
                (registered, started, receiver)
            })
            .collect();
        let mut reasons = Vec::new();
        for (registered, started, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.inner.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let mut stats = sync::lock(&registered.stats);
            stats.total_ms += ms;
            stats.turn_ms += ms;
            stats.checks += 1;
            let (outcome, reason) = match answer {
                Answer::TimedOut | Answer::Error(_) => {
                    stats.unchecked += 1;
                    ("unchecked", None)
                }
                Answer::Value(Verdict::Block(reason)) => {
                    stats.blocked += 1;
                    ("blocked", Some(format!("[{name}] {reason}")))
                }
                Answer::Value(Verdict::Allow(_)) => ("allowed", None),
            };
            drop(stats);
            self.inner.record(GuardRecord {
                guard: name,
                phase: "before_shell",
                outcome,
                ms,
                paths: vec![display.clone()],
                lane: lane.to_string(),
                detail: reason.clone(),
            });
            reasons.extend(reason);
        }
        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    /// `beforeWrite` of one proposed write on `lane`: every enabled guard, in parallel, each within its time limit.
    fn before_write(&self, lane: &str, path: &str, content: &str) -> Value {
        let path = resolve_lexical(&self.inner.cwd, path);
        if let Some(protected) = self.protected_by(&path) {
            let reason = self.refuse_protected(lane, "before_write", &protected);
            return json!({ "blocked": true, "reason": reason, "notes": [] });
        }
        let display = self.display(&path);
        let calls: Vec<(Registered, u64, Option<Duration>, Pending)> = self
            .guards()
            .into_iter()
            .filter(|r| r.guard.checks_writes() && r.guard.enabled())
            .map(|registered| {
                let started = self.inner.now();
                let timeout = registered.guard.timeout();
                let (guard, path, content, cwd, lane) = (
                    registered.guard.clone(),
                    path.clone(),
                    content.to_string(),
                    self.inner.cwd.clone(),
                    lane.to_string(),
                );
                // Started together; each answer is awaited below with its own limit.
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(timeout, move || guard.before_write(&path, &content, &cwd, &lane)));
                });
                (registered, started, timeout, receiver)
            })
            .collect();
        let mut reasons = Vec::new();
        let mut notes = Vec::new();
        for (registered, started, timeout, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.inner.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let mut stats = sync::lock(&registered.stats);
            stats.total_ms += ms;
            stats.turn_ms += ms;
            stats.checks += 1;
            let (outcome, reason, note) = match answer {
                Answer::TimedOut => {
                    stats.unchecked += 1;
                    let limit = js_seconds(timeout.unwrap_or_default());
                    let note = format!(
                        "[{name}] {name} did not check this write to {display} (no answer within {limit} s); it was written unchecked."
                    );
                    ("unchecked", None, Some(note))
                }
                Answer::Error(error) => {
                    stats.unchecked += 1;
                    let note = format!(
                        "[{name}] {name} did not check this write to {display} ({error}); it was written unchecked."
                    );
                    ("unchecked", None, Some(note))
                }
                Answer::Value(Verdict::Block(reason)) => {
                    stats.blocked += 1;
                    ("blocked", Some(format!("[{name}] {reason}")), None)
                }
                Answer::Value(Verdict::Allow(Some(message))) if !message.is_empty() => {
                    ("allowed", None, Some(format!("[{name}] {message}")))
                }
                Answer::Value(Verdict::Allow(_)) => ("allowed", None, None),
            };
            drop(stats);
            self.inner.record(GuardRecord {
                guard: name,
                phase: "before_write",
                outcome,
                ms,
                paths: vec![display.clone()],
                lane: lane.to_string(),
                detail: reason.clone(),
            });
            reasons.extend(reason);
            notes.extend(note);
        }
        if reasons.is_empty() {
            // The write will happen with this content; an after-cell diff need not report it again.
            sync::lock(&self.inner.checked).insert(path, jsrt::hash::sha256_hex(content.as_bytes()));
            json!({ "blocked": false, "notes": notes })
        } else {
            json!({ "blocked": true, "reason": reasons.join("\n"), "notes": notes })
        }
    }

    fn guard_stats(&self) -> Vec<Value> {
        self.stats().iter().map(GuardStats::to_json).collect()
    }

    fn begin_turn(&self) {
        FileHooks::begin_turn(self);
    }

    fn cell_started(&self, lane: &str) {
        FileHooks::cell_started(self, lane);
    }

    fn cell_ended(&self, lane: &str) {
        FileHooks::cell_ended(self, lane);
    }

    fn take_pending(&self, lane: &str) -> Option<String> {
        FileHooks::take_pending(self, lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_inside_matches_path_relative() {
        let root = Path::new("/r");
        assert_eq!(inside(root, Path::new("/r/a/b.txt")).as_deref(), Some("a/b.txt"));
        assert_eq!(inside(root, Path::new("/r")), None);
        assert_eq!(inside(root, Path::new("/rx/a")), None);
        assert_eq!(inside(root, Path::new("/r/..x")), None);
        assert_eq!(resolve_lexical(root, "a/../b/./c"), Path::new("/r/b/c"));
        assert_eq!(resolve_lexical(root, "/abs/../x"), Path::new("/x"));
    }
}
