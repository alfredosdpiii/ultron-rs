//! Port of `worktrees.ts`: Git worktrees for subagents. `rlm.spawn(brief, name=..., worktree=True)` gives a child a
//! private checkout of its parent's repository, and `rlm.merge(children)` brings each child's work back into the
//! parent's working tree.
//!
//! Creating ([`create_child_worktree`]): the parent's working tree is pictured as a commit from a copy of its index
//! (`git add -A`, `write-tree`, `commit-tree` on top of HEAD; HEAD itself when nothing is dirty), and the child's
//! branch `ultron/<session>/<name>` starts there, checked out in `<git common dir>/ultron-worktrees/<id>`. Gitignored
//! dependency directories are linked in and small env files copied ([`WorktreeSetup`]).
//!
//! Finishing ([`commit_child_worktree`]): what the child changed is committed on its branch (no hooks, no signing).
//!
//! Merging ([`merge_child_worktree`]): a three-way merge of trees (base = the child's snapshot, ours = a fresh
//! snapshot of the parent's tree, theirs = the child's commit) by `git merge-tree --write-tree`, written into the
//! parent's working tree through a temporary index after checking none of the written files changed meanwhile. A
//! conflict writes nothing unless conflict markers were asked for. Nothing is committed on the parent's branch.
//!
//! Crashes: each worktree has a record (`<id>.json`) naming its owner process; [`prune_worktrees`] removes those of
//! gone owners, committing uncommitted work first and keeping branches that hold work.

use jsrt::utf16;
use kernel::Signal;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Branches of subagent worktrees: `ultron/<session>/<name>`.
pub const BRANCH_PREFIX: &str = "ultron";
/// Keep worktrees and branches after merges, on session end and at startup pruning (debugging).
pub const KEEP_WORKTREES_ENV: &str = "ULTRON_KEEP_WORKTREES";
/// Gitignored directories linked into a worktree by default.
pub const DEFAULT_LINK_NAMES: [&str; 3] = ["node_modules", ".venv", "venv"];
const MAX_COPY_BYTES: u64 = 256 * 1024;
const MAX_SETUP_DEPTH: usize = 3;
const MAX_SETUP_ENTRIES: usize = 64;
const DEFAULT_SETUP_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// A stopped Git, jj or setup command gets SIGTERM, and SIGKILL when it is still running this long after.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(5);
const MAX_HUNKS_PER_FILE: usize = 5;
const MAX_CONFLICT_FILES: usize = 20;
const HUNK_TEXT_CHARS: usize = 400;
const ZERO_MODE: &str = "000000";

/// A worktree operation failed. `fallback`: a shared working directory is a fine substitute (`worktree="auto"`):
/// no Git, no repository, no commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeError {
    pub message: String,
    pub fallback: bool,
}

impl WorktreeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: false,
        }
    }
    fn fallback(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: true,
        }
    }
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<std::io::Error> for WorktreeError {
    fn from(error: std::io::Error) -> Self {
        WorktreeError::new(error.to_string())
    }
}

type Result<T> = std::result::Result<T, WorktreeError>;

/// `ULTRON_KEEP_WORKTREES` set to anything but empty, 0, false, no or off.
pub fn keep_worktrees(value: Option<&str>) -> bool {
    value
        .map(|v| v.trim().to_lowercase())
        .is_some_and(|v| !v.is_empty() && !["0", "false", "no", "off"].contains(&v.as_str()))
}

/// How a worktree is prepared for its child after checkout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorktreeSetup {
    /// Gitignored paths to link from the parent's checkout; default every gitignored node_modules/.venv/venv a few
    /// levels deep. `Some(vec![])` links nothing.
    pub link: Option<Vec<String>>,
    /// Gitignored files to copy; default small `.env` / `.env.*` files.
    pub copy: Option<Vec<String>>,
    /// A shell command run in the child's working directory before it starts.
    pub command: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    pub host: String,
}

impl Owner {
    /// This worker process on this host.
    pub fn of_this_process() -> Owner {
        Owner {
            pid: std::process::id(),
            host: hostname(),
        }
    }
}

/// A subagent worktree, as recorded next to it (`<common dir>/ultron-worktrees/<id>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    pub version: u32,
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub owner: Owner,
    pub repo: String,
    pub common_dir: String,
    pub path: String,
    pub cwd: String,
    pub branch: String,
    pub head: String,
    pub base: String,
    pub dirty: bool,
    pub linked: Vec<String>,
    pub copied: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command_timeout_ms: Option<u64>,
    pub created_at: f64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub task_id: Option<String>,
    /// The branch's final commit once committed: `Some(None)` when the child changed nothing.
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "present")]
    pub commit: Option<Option<String>>,
}

/// `commit` is three-state (absent, null, a commit), as in the TS record: a present null is `Some(None)`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::deserialize(deserializer)?))
}

#[derive(Debug, Clone, Default)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Default)]
pub(crate) struct GitOptions<'a> {
    pub(crate) env: Vec<(&'a str, String)>,
    pub(crate) input: Option<Vec<u8>>,
}

pub(crate) fn opts<'a>(env: &[(&'a str, &str)]) -> GitOptions<'a> {
    GitOptions {
        env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        input: None,
    }
}

/// Runs `git` without a shell or terminal; stdout and stderr as text. Fails (fallback) when Git cannot start.
fn git_with(cwd: &Path, args: &[&str], options: GitOptions<'_>) -> Result<GitResult> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in &options.env {
        command.env(key, value);
    }
    let bound = signals::child::Bound {
        timeout: Some(GIT_TIMEOUT),
        stop: signals::child::Stop {
            group: false,
            term_grace: Some(KILL_GRACE),
        },
        keep: None,
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = signals::child::run(&mut command, options.input.as_deref(), bound).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorktreeError::fallback("worktree=True needs Git, and `git` is not installed or not on PATH")
        } else {
            WorktreeError::new(error.to_string())
        }
    })?;
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    stderr.truncate(stderr.floor_char_boundary(100_000.min(stderr.len())));
    Ok(GitResult {
        code: output.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
    })
}

fn git(cwd: &Path, args: &[&str]) -> Result<GitResult> {
    git_with(cwd, args, GitOptions::default())
}

fn first_lines(text: &str, count: usize) -> String {
    text.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(count)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `git` that must succeed; its stderr names what failed.
pub(crate) fn git_ok_with(cwd: &Path, args: &[&str], options: GitOptions<'_>) -> Result<String> {
    let result = git_with(cwd, args, options)?;
    if result.code != 0 {
        let shown: Vec<&str> = args
            .iter()
            .copied()
            .filter(|arg| !arg.starts_with("-c") && !arg.contains('='))
            .collect();
        let why = first_lines(
            if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            },
            3,
        );
        let why = if why.is_empty() {
            format!("exit {}", result.code)
        } else {
            why
        };
        return Err(WorktreeError::new(format!("git {} failed: {why}", shown.join(" "))));
    }
    Ok(result.stdout)
}

pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    git_ok_with(cwd, args, GitOptions::default())
}

/// No user hooks, no signing prompts: Ultron's own bookkeeping commits and checkouts.
pub(crate) fn quiet<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "tag.gpgsign=false",
    ];
    out.extend_from_slice(args);
    out
}

type Identity = Vec<(&'static str, String)>;

static IDENTITIES: LazyLock<Mutex<HashMap<PathBuf, Identity>>> = LazyLock::new(Mutex::default);

/// Author/committer for Ultron's commits: the repository's configured identity, else "Ultron".
pub(crate) fn identity_env(cwd: &Path) -> Result<Identity> {
    if let Some(cached) = jsrt::sync::lock(&IDENTITIES).get(cwd) {
        return Ok(cached.clone());
    }
    let name = git(cwd, &["config", "user.name"])?;
    let email = git(cwd, &["config", "user.email"])?;
    let mut env = Vec::new();
    if name.stdout.trim().is_empty() && std::env::var_os("GIT_AUTHOR_NAME").is_none() {
        env.push(("GIT_AUTHOR_NAME", "Ultron".to_string()));
        env.push(("GIT_COMMITTER_NAME", "Ultron".to_string()));
    }
    if email.stdout.trim().is_empty() && std::env::var_os("GIT_AUTHOR_EMAIL").is_none() {
        env.push(("GIT_AUTHOR_EMAIL", "ultron@localhost".to_string()));
        env.push(("GIT_COMMITTER_EMAIL", "ultron@localhost".to_string()));
    }
    jsrt::sync::lock(&IDENTITIES).insert(cwd.to_path_buf(), env.clone());
    Ok(env)
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub top: PathBuf,
    pub common_dir: PathBuf,
    /// HEAD's commit; `None` in a repository without commits.
    pub head: Option<String>,
}

/// The repository `cwd` is in. Fails (fallback) outside Git or a working tree.
pub fn repo_info(cwd: &Path) -> Result<RepoInfo> {
    let result = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--is-bare-repository",
        ],
    )?;
    if result.code != 0 {
        if result.stderr.to_lowercase().contains("not a git repository") {
            return Err(WorktreeError::fallback(format!(
                "worktree=True needs a Git repository, and {} is not inside one",
                text(cwd)
            )));
        }
        return Err(WorktreeError::fallback(format!(
            "git could not read the repository at {}: {}",
            text(cwd),
            first_lines(&result.stderr, 3)
        )));
    }
    let lines: Vec<&str> = result.stdout.trim().split('\n').collect();
    let (top, common, bare) = (
        lines.first().copied().unwrap_or(""),
        lines.get(1).copied().unwrap_or(""),
        lines.get(2).copied().unwrap_or(""),
    );
    if bare == "true" || top.is_empty() {
        return Err(WorktreeError::fallback(format!(
            "{} is not inside a Git working tree",
            text(cwd)
        )));
    }
    let head = git(cwd, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])?;
    Ok(RepoInfo {
        top: jsrt::path::absolute(Path::new(top)),
        common_dir: jsrt::path::absolute(Path::new(common)),
        head: (head.code == 0).then(|| head.stdout.trim().to_string()),
    })
}

pub(crate) fn temporary_index(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ultron-{kind}-{}-{}",
        std::process::id(),
        jsrt::random::random_hex(6)
    ))
}

pub(crate) struct TempIndex(pub(crate) PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Copy the index with its modification time. Git trusts an entry whose cached stat matches the file (seconds
/// only) unless the entry is racily clean: not older than the index file itself. A plain copy is newer than every
/// entry, so a file rewritten with the same size in the second it was staged (an edit right after a commit) looked
/// unchanged once the copy landed a second later, and the snapshot left the edit out: under load the fixture's
/// parent edit went missing and a conflicting merge came back as "changed during the merge".
pub(crate) fn copy_index(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options().write(true).open(to)?.set_modified(modified)
}

/// A commit of `top`'s working tree as it is now, made from a copy of its index (the working tree and the real index
/// stay untouched); HEAD itself (`dirty: false`) when nothing differs from it.
pub fn snapshot_working_tree(top: &Path, head: &str, message: &str) -> Result<(String, bool)> {
    let index_path = git_ok(top, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let temporary = TempIndex(temporary_index("index"));
    let index = text(&temporary.0);
    let env = [("GIT_INDEX_FILE", index.as_str()), ("GIT_OPTIONAL_LOCKS", "0")];
    if copy_index(Path::new(index_path.trim()), &temporary.0).is_err() {
        git_ok_with(top, &["read-tree", head], opts(&env))?;
    }
    git_ok_with(top, &quiet(&["add", "-A", "--", "."]), opts(&env))?;
    let tree = git_ok_with(top, &["write-tree"], opts(&env))?.trim().to_string();
    let head_tree = git_ok(top, &["rev-parse", &format!("{head}^{{tree}}")])?
        .trim()
        .to_string();
    if tree == head_tree {
        return Ok((head.to_string(), false));
    }
    let identity = identity_env(top)?;
    let commit = git_ok_with(
        top,
        &quiet(&["commit-tree", &tree, "-p", head, "-m", message]),
        GitOptions {
            env: identity,
            input: None,
        },
    )?
    .trim()
    .to_string();
    Ok((commit, true))
}

fn records_dir(common_dir: &Path) -> PathBuf {
    common_dir.join(kernel::env::WORKTREES_DIR)
}

fn record_path(record: &WorktreeRecord) -> PathBuf {
    records_dir(Path::new(&record.common_dir)).join(format!("{}.json", record.id))
}

/// The record as `JSON.stringify(record, null, "\t")` writes it.
pub fn record_json(record: &WorktreeRecord) -> String {
    let value = serde_json::to_value(record).unwrap_or_default();
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    let _ = serde::Serialize::serialize(&jsrt::json::normalize_numbers(value), &mut serializer);
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes the record beside its final path and renames it there, so a crash never leaves a truncated record.
pub fn save_record(record: &WorktreeRecord) -> Result<()> {
    let path = record_path(record);
    jsrt::fs::atomic_write_mode(&path, format!("{}\n", record_json(record)).as_bytes(), 0o600)?;
    Ok(())
}

fn delete_record(record: &WorktreeRecord) {
    let _ = std::fs::remove_file(record_path(record));
}

/// Every recorded worktree of the repository `cwd` is in (any session).
pub fn list_records(cwd: &Path) -> Vec<WorktreeRecord> {
    match repo_info(cwd) {
        Ok(info) => read_records(&info.common_dir),
        Err(_) => Vec::new(),
    }
}

fn read_records(common_dir: &Path) -> Vec<WorktreeRecord> {
    let dir = records_dir(common_dir);
    let mut names: Vec<String> = std::fs::read_dir(&dir).map_or_default(|entries| {
        entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect()
    });
    names.sort();
    names
        .iter()
        .filter_map(|name| std::fs::read_to_string(dir.join(name)).ok())
        .filter_map(|text| serde_json::from_str::<WorktreeRecord>(&text).ok())
        .filter(|record| record.version == 1)
        .collect()
}

pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Whether a record's owning process still runs (on this machine).
pub fn owner_alive(owner: &Owner) -> bool {
    if owner.host != hostname() || owner.pid == std::process::id() {
        return true;
    }
    signals::pid_alive(owner.pid)
}

static SLUG_RULES: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        Regex::new(r"[^a-z0-9._-]+").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"\.{2,}").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"-{2,}").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"^[-.]+|[-.]+$").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"[-.]+$").unwrap_or_else(|_| unreachable!()),
    ]
});

/// JavaScript `toLowerCase` then `slice(0, n)` counts UTF-16 units; the slug is ASCII by then.
pub fn slug(name: &str) -> String {
    let rules = &*SLUG_RULES;
    let text = name.to_lowercase();
    let text = rules[0].replace_all(&text, "-");
    let text = rules[1].replace_all(&text, ".");
    let text = rules[2].replace_all(&text, "-");
    let text = rules[3].replace_all(&text, "");
    let text: String = text.chars().take(40).collect();
    let text = rules[4].replace_all(&text, "").into_owned();
    if text.is_empty() { "child".into() } else { text }
}

fn branch_exists(top: &Path, branch: &str) -> Result<bool> {
    Ok(git(
        top,
        &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )?
    .code
        == 0)
}

/// A new branch for a child: `ultron/<session>/<name>`, suffixed when taken (never reused).
fn unique_branch(top: &Path, session_id: &str, name: &str) -> Result<String> {
    let session: String = slug(session_id).chars().take(8).collect();
    let stem = format!("{BRANCH_PREFIX}/{session}/{}", slug(name));
    for attempt in 1..1000 {
        let candidate = if attempt == 1 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        };
        if git(top, &["check-ref-format", "--branch", &candidate])?.code != 0 {
            return Err(WorktreeError::new(format!(
                "cannot make a branch name from {}",
                serde_json::Value::from(name)
            )));
        }
        if !branch_exists(top, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(WorktreeError::new(format!("no free branch name for {name}")))
}

/// `path` relative to `root` with "/" separators, "" for the root itself, `None` outside it.
pub fn relative_inside(root: &Path, path: &Path) -> Option<String> {
    let rel = jsrt::path::absolute(path)
        .strip_prefix(jsrt::path::absolute(root))
        .ok()?
        .to_path_buf();
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn safe_relative(path: &str, what: &str) -> Result<String> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() || normalized.starts_with('/') || normalized.split('/').any(|p| p == ".." || p.is_empty())
    {
        return Err(WorktreeError::new(format!(
            "worktree setup {what} path {} must be relative to the repository root",
            serde_json::Value::from(path)
        )));
    }
    Ok(normalized.to_string())
}

/// Gitignored entries of the parent's checkout: directories end with "/".
fn ignored_entries(top: &Path) -> Result<Vec<String>> {
    let result = git_with(
        top,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
        opts(&[("GIT_OPTIONAL_LOCKS", "0")]),
    )?;
    if result.code != 0 {
        return Ok(Vec::new());
    }
    Ok(result
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

fn sorted_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).map_or_default(|entries| {
        entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    });
    names.sort();
    names
}

/// Link a node_modules directory entry by entry; links back into the repository (workspace packages) point to the
/// same place in the worktree.
fn link_node_modules(source: &Path, target: &Path, repo: &Path, worktree: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in sorted_entries(source) {
        let from = source.join(&entry);
        let to = target.join(&entry);
        let Ok(info) = std::fs::symlink_metadata(&from) else {
            continue;
        };
        if entry.starts_with('@') && info.is_dir() {
            link_node_modules(&from, &to, repo, worktree)?;
            continue;
        }
        let mut destination = from.clone();
        if info.file_type().is_symlink()
            && let Ok(resolved) = std::fs::canonicalize(&from)
            && let Some(inside) = relative_inside(repo, &resolved)
            && !inside.split('/').any(|p| p == "node_modules")
        {
            destination = worktree.join(inside);
        }
        let _ = symlink(&destination, &to);
    }
    Ok(())
}

fn is_env_name(name: &str) -> bool {
    name == ".env" || name.strip_prefix(".env.").is_some_and(|rest| !rest.is_empty())
}

fn setup_path_depth(path: &str) -> usize {
    path.trim_end_matches('/').split('/').count()
}

fn resolve_setup_links(setup: &WorktreeSetup, ignored: &[String]) -> Result<Vec<String>> {
    match &setup.link {
        Some(paths) => paths.iter().map(|p| safe_relative(p, "link")).collect(),
        None => Ok(ignored
            .iter()
            .filter(|e| e.ends_with('/') && setup_path_depth(e) <= MAX_SETUP_DEPTH)
            .map(|e| e[..e.len() - 1].to_string())
            .filter(|p| DEFAULT_LINK_NAMES.contains(&p.rsplit('/').next().unwrap_or("")))
            .take(MAX_SETUP_ENTRIES)
            .collect()),
    }
}

fn resolve_setup_copies(setup: &WorktreeSetup, ignored: &[String], links: &[String]) -> Result<Vec<String>> {
    let in_linked = |path: &str| {
        links
            .iter()
            .any(|link| path == link || path.starts_with(&format!("{link}/")))
    };
    match &setup.copy {
        Some(paths) => paths.iter().map(|p| safe_relative(p, "copy")).collect(),
        None => Ok(ignored
            .iter()
            .filter(|e| {
                !e.ends_with('/')
                    && setup_path_depth(e) <= MAX_SETUP_DEPTH
                    && is_env_name(e.rsplit('/').next().unwrap_or(""))
                    && !in_linked(e)
            })
            .take(MAX_SETUP_ENTRIES)
            .cloned()
            .collect()),
    }
}

fn link_source_entry(from: &Path, to: &Path, top: &Path, worktree: &Path, path: &str, is_dir: bool) -> Result<()> {
    if path.rsplit('/').next() == Some("node_modules") && is_dir {
        link_node_modules(from, to, top, worktree)?;
    } else if is_dir {
        std::fs::create_dir_all(to)?;
        for entry in sorted_entries(from) {
            let _ = symlink(from.join(&entry), to.join(&entry));
        }
    } else {
        symlink(from, to)?;
    }
    Ok(())
}

fn apply_one_link(top: &Path, worktree: &Path, path: &str, explicit: bool) -> Result<bool> {
    let (from, to) = (top.join(path), worktree.join(path));
    let Ok(source) = std::fs::metadata(&from) else {
        if explicit {
            return Err(WorktreeError::new(format!(
                "worktree setup link {path} does not exist in {}",
                text(top)
            )));
        }
        return Ok(false);
    };
    if std::fs::symlink_metadata(&to).is_ok() {
        return Ok(false);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    link_source_entry(&from, &to, top, worktree, path, source.is_dir())?;
    Ok(true)
}

fn apply_one_copy(top: &Path, worktree: &Path, path: &str, explicit: bool) -> Result<bool> {
    let (from, to) = (top.join(path), worktree.join(path));
    let source = std::fs::metadata(&from).ok().filter(std::fs::Metadata::is_file);
    let Some(source) = source else {
        if explicit {
            return Err(WorktreeError::new(format!(
                "worktree setup copy {path} is not a file in {}",
                text(top)
            )));
        }
        return Ok(false);
    };
    if (!explicit && source.len() > MAX_COPY_BYTES) || std::fs::symlink_metadata(&to).is_ok() {
        return Ok(false);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&from, &to)?;
    Ok(true)
}

fn apply_setup_entries(
    top: &Path,
    worktree: &Path,
    paths: &[String],
    explicit: bool,
    apply: fn(&Path, &Path, &str, bool) -> Result<bool>,
) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for path in paths {
        if apply(top, worktree, path, explicit)? {
            done.push(path.clone());
        }
    }
    Ok(done)
}

fn apply_setup(top: &Path, worktree: &Path, setup: &WorktreeSetup) -> Result<(Vec<String>, Vec<String>)> {
    let ignored = if setup.link.is_none() || setup.copy.is_none() {
        ignored_entries(top)?
    } else {
        Vec::new()
    };
    let links = resolve_setup_links(setup, &ignored)?;
    let copies = resolve_setup_copies(setup, &ignored, &links)?;
    let linked = apply_setup_entries(top, worktree, &links, setup.link.is_some(), apply_one_link)?;
    let copied = apply_setup_entries(top, worktree, &copies, setup.copy.is_some(), apply_one_copy)?;
    Ok((linked, copied))
}

pub struct CreateWorktreeOptions<'a> {
    /// The parent's working directory: the worktree branches from its repository's working tree.
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub name: &'a str,
    pub setup: Option<&'a WorktreeSetup>,
    /// Milliseconds since the epoch for `createdAt`.
    pub now: f64,
}

/// Creations per repository run one at a time, so branch names are picked and taken without a race.
static CREATING: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);

pub fn create_child_worktree(options: &CreateWorktreeOptions<'_>) -> Result<WorktreeRecord> {
    let info = repo_info(options.cwd)?;
    let _serial = jsrt::sync::lock(&CREATING);
    create_in(&info, options)
}

fn create_in(info: &RepoInfo, options: &CreateWorktreeOptions<'_>) -> Result<WorktreeRecord> {
    let Some(head) = info.head.clone() else {
        return Err(WorktreeError::fallback(format!(
            "worktree=True needs a commit to branch from, and the repository at {} has none yet",
            text(&info.top)
        )));
    };
    let (base, dirty) = snapshot_working_tree(
        &info.top,
        &head,
        &format!("ultron: snapshot of the working tree for sub-agent {}", options.name),
    )?;
    let branch = unique_branch(&info.top, options.session_id, options.name)?;
    let id = format!(
        "{}-{}-{}",
        slug(options.session_id).chars().take(8).collect::<String>(),
        slug(options.name).chars().take(24).collect::<String>(),
        jsrt::random::random_hex(3)
    );
    let path = records_dir(&info.common_dir).join(&id);
    let real = std::fs::canonicalize(options.cwd).unwrap_or_else(|_| jsrt::path::absolute(options.cwd));
    let at = relative_inside(&info.top, &real).unwrap_or_default();
    let setup = options.setup.cloned().unwrap_or_default();
    let mut record = WorktreeRecord {
        version: 1,
        id,
        session_id: options.session_id.to_string(),
        name: options.name.to_string(),
        owner: Owner::of_this_process(),
        repo: text(&info.top),
        common_dir: text(&info.common_dir),
        path: text(&path),
        cwd: if at.is_empty() {
            text(&path)
        } else {
            text(&path.join(&at))
        },
        branch: branch.clone(),
        head,
        base: base.clone(),
        dirty,
        linked: Vec::new(),
        copied: Vec::new(),
        command: setup
            .command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(String::from),
        command_timeout_ms: setup.timeout_ms,
        created_at: options.now,
        state: "creating".into(),
        task_id: None,
        commit: None,
    };
    save_record(&record)?;
    let made = (|| -> Result<()> {
        git_ok(
            &info.top,
            &quiet(&["worktree", "add", "--quiet", "-b", &branch, &record.path, &base]),
        )?;
        std::fs::create_dir_all(&record.cwd)?;
        let (linked, copied) = apply_setup(&info.top, &path, &setup)?;
        record.linked.extend(linked);
        record.copied.extend(copied);
        record.state = "active".into();
        save_record(&record)
    })();
    match made {
        Ok(()) => Ok(record),
        Err(error) => {
            remove_child_worktree(&record, true);
            Err(error)
        }
    }
}

/// Environment of every process working in a child's worktree.
pub fn worktree_env(record: &WorktreeRecord) -> Vec<(String, String)> {
    vec![
        (kernel::env::WORKTREE_ENV.into(), record.path.clone()),
        (kernel::env::PARENT_REPO_ENV.into(), record.repo.clone()),
        (kernel::env::WORKTREE_BRANCH_ENV.into(), record.branch.clone()),
        ("PWD".into(), record.cwd.clone()),
    ]
}

fn spawn_setup_output_drain(
    mut pipe: std::io::PipeReader,
    output: std::sync::Arc<Mutex<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut out = jsrt::sync::lock(&output);
            out.push_str(&String::from_utf8_lossy(&chunk[..n]));
            let excess = out.chars().count().saturating_sub(4000);
            if excess > 0 {
                *out = out.chars().skip(excess).collect();
            }
        }
    })
}

fn setup_command_error(command: &str, output: &Mutex<String>, code: Option<i32>, timed_out: bool) -> WorktreeError {
    let output = jsrt::sync::lock(output).trim().to_string();
    let chars: Vec<char> = output.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(1500)..].iter().collect();
    let how = if timed_out {
        "timed out".to_string()
    } else {
        format!("failed (exit {})", code.map_or("?".to_string(), |c| c.to_string()))
    };
    WorktreeError::new(format!(
        "worktree setup command {} {how}: {tail}",
        serde_json::Value::from(command)
    ))
}

/// Run the setup command (`worktree_setup={"command": ...}`) in the child's working directory, in its own process
/// group (killed on timeout or when `signal` is aborted).
pub fn run_setup_command(record: &WorktreeRecord, signal: Option<&Signal>) -> Result<()> {
    let Some(command) = record.command.as_deref() else {
        return Ok(());
    };
    let (reader, writer) = std::io::pipe()?;
    let mut sh = Command::new("/bin/sh");
    sh.args(["-c", command])
        .current_dir(&record.cwd)
        .envs(worktree_env(record))
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let bound = signals::child::Bound {
        timeout: Some(Duration::from_millis(
            record.command_timeout_ms.unwrap_or(DEFAULT_SETUP_TIMEOUT_MS),
        )),
        stop: signals::child::Stop {
            group: true,
            term_grace: Some(KILL_GRACE),
        },
        keep: None,
    };
    let output = std::sync::Arc::new(Mutex::new(String::new()));
    let drain = spawn_setup_output_drain(reader, output.clone());
    let ran = match signal {
        Some(signal) => signal.run_child(&mut sh, None, bound),
        None => signals::child::run(&mut sh, None, bound),
    };
    drop(sh);
    let (code, timed_out) = match ran {
        Err(error) => {
            *jsrt::sync::lock(&output) = error.to_string();
            (None, false)
        }
        Ok(out) => {
            let _ = drain.join();
            (out.code(), out.end == signals::child::End::TimedOut)
        }
    };
    if let Some(reason) = signal.and_then(Signal::reason) {
        return Err(WorktreeError::new(reason));
    }
    if code != Some(0) {
        return Err(setup_command_error(command, &output, code, timed_out));
    }
    Ok(())
}

/// The note added to a worktree child's brief.
pub fn worktree_brief_note(record: &WorktreeRecord) -> String {
    format!(
        "[Worktree] You work in your own Git worktree {} (branch {}): a private copy of your parent's checkout, uncommitted changes included. Use paths relative to it; read/edit/write map absolute paths under {} into it, but shell commands do not, so never cd there. Your changes are committed on your branch when you finish and your parent merges them: do not commit, switch branches or push yourself, even if your brief asks you to commit.",
        record.cwd, record.branch, record.repo
    )
}

/// An absolute path into the parent's checkout, as the same path in a child's worktree; any other path as it is.
pub fn in_worktree(path: &str, record: &WorktreeRecord) -> String {
    if !path.starts_with('/') {
        return path.to_string();
    }
    if relative_inside(Path::new(&record.path), Path::new(path)).is_some() {
        return path.to_string();
    }
    match relative_inside(Path::new(&record.repo), Path::new(path)) {
        Some(rel) if rel.is_empty() => record.path.clone(),
        Some(rel) => text(&Path::new(&record.path).join(rel)),
        None => path.to_string(),
    }
}

/// Paths (relative to the root) that differ from HEAD in a working tree, untracked ones included; `None` on error.
pub fn list_dirty(root: &Path) -> Option<Vec<String>> {
    let status = git_with(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ],
        opts(&[("GIT_OPTIONAL_LOCKS", "0")]),
    )
    .ok()?;
    if status.code != 0 {
        return None;
    }
    Some(
        status
            .stdout
            .split('\0')
            .filter(|item| item.len() > 3)
            .map(|item| item[3..].to_string())
            .collect(),
    )
}

fn outside_setup(record: &WorktreeRecord, path: &str) -> bool {
    !record
        .linked
        .iter()
        .chain(&record.copied)
        .any(|entry| path == entry || path.starts_with(&format!("{entry}/")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub commit: Option<String>,
    pub changed_files: Vec<String>,
    pub diffstat: String,
}

fn stage_and_commit_paths(root: &Path, wanted: &[String], message: &str) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let literal = [("GIT_LITERAL_PATHSPECS", "1")];
    let mut input = wanted.join("\0").into_bytes();
    input.push(0);
    let added = git_ok_with(
        root,
        &quiet(&[
            "add",
            "-A",
            "--ignore-errors",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]),
        GitOptions {
            env: literal.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            input: Some(input),
        },
    );
    if added.is_err() {
        for path in wanted {
            git_with(root, &quiet(&["add", "-A", "--", path]), opts(&literal))?;
        }
    }
    if git(root, &["diff", "--cached", "--quiet"])?.code == 1 {
        git_ok_with(
            root,
            &quiet(&["commit", "-q", "--no-verify", "-m", message]),
            GitOptions {
                env: identity_env(root)?,
                input: None,
            },
        )?;
    }
    Ok(())
}

/// Commit what the child changed on its branch; `paths` (relative to the worktree root) limits the commit, by default
/// every change Git sees minus the setup links and copies.
pub fn commit_child_worktree(
    record: &WorktreeRecord,
    message: &str,
    paths: Option<&[String]>,
) -> Result<CommitOutcome> {
    let root = Path::new(&record.path);
    let candidates = match paths {
        Some(paths) => paths.to_vec(),
        None => list_dirty(root).unwrap_or_default(),
    };
    let wanted: Vec<String> = candidates.into_iter().filter(|p| outside_setup(record, p)).collect();
    stage_and_commit_paths(root, &wanted, message)?;
    let head = git_ok(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let same = git(root, &["diff", "--quiet", &record.base, &head])?;
    if head == record.base || same.code == 0 {
        return Ok(CommitOutcome {
            commit: None,
            changed_files: Vec::new(),
            diffstat: String::new(),
        });
    }
    let names = git_ok(
        root,
        &["diff", "--name-only", "-z", "--no-renames", &record.base, &head],
    )?;
    let stat = git_ok(root, &["diff", "--shortstat", &record.base, &head])?;
    Ok(CommitOutcome {
        commit: Some(head),
        changed_files: names.split('\0').filter(|s| !s.is_empty()).map(String::from).collect(),
        diffstat: stat.trim().to_string(),
    })
}

/// A commit message from a child's summary, with its name and task (no trailers).
pub fn commit_message(summary: &str, name: &str, task_id: Option<&str>, status: &str) -> String {
    let trimmed = summary.trim();
    let text = if trimmed.is_empty() {
        format!("Sub-agent {name} ({status})")
    } else {
        trimmed.to_string()
    };
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("");
    let rest: Vec<&str> = lines.collect();
    let long = utf16::len(first) > 72;
    let subject = if long {
        format!("{}…", utf16::head(first, 71))
    } else {
        first.to_string()
    };
    let mut body_lines = vec![if long { first } else { "" }];
    body_lines.extend(rest);
    let body = body_lines.join("\n").trim().to_string();
    let mut out = vec![subject];
    if !body.is_empty() {
        out.push(String::new());
        out.push(body);
    }
    out.push(String::new());
    out.push(format!(
        "Ultron sub-agent \"{name}\"{}, {status}.",
        task_id.map_or(String::new(), |id| format!(" (task {id})"))
    ));
    out.join("\n")
}

mod merge_prune;
pub use merge_prune::{
    ConflictFile, ConflictHunk, MergeOutcome, MergeSource, PruneOptions, PruneReport, conflict_hunks,
    merge_child_worktree, prune_orphan_worktrees, prune_worktrees, remove_child_worktree,
};
