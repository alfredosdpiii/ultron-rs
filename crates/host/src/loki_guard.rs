//! Ultron's built-in Loki guardrails (ultron/loki.ts `LokiGuard`), running the Rust Loki engine in-process instead of
//! `python loki.py`:
//! - before `edit()`/`write()` change a file: `protect --file <path> --preview ultron` on the proposed content; a
//!   finding blocks the write (with `advise`, it is only reported);
//! - before a shell command that may write files: `shell-writes`, which refuses a write into source or configuration
//!   that would skip the before-write check;
//! - after a cell, on the files it changed by other means (`hook --file ... --format json`): new findings to fix,
//!   findings shown earlier and still open as one line, advisory lines once, at most 40 files per check.
//!
//! The policy is the repository's `.loki/loki.json` under the root (the Git work tree, else the session directory).

use crate::file_hooks::{AfterCellReport, AfterOutcome, CellChanges, FileWriteGuard, Verdict, inside};
use loki::CheckError;
use loki::run::Report;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files one after-cell check takes; the rest are named as not checked.
const MAX_HOOK_FILES: usize = 40;

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// A `.loki/` file whose content is the committed one: a `git checkout`, `switch` or `commit` in the cell brought
/// it there, so it is no change to the policy and not reported as one (a branch switch otherwise reported
/// `protected file: .loki/loki.json` on the cell). Untracked or modified `.loki/` files are still checked.
fn loki_file_as_committed(root: &Path, relative: &str) -> bool {
    if !relative.starts_with(".loki/") {
        return false;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    git(&["ls-files", "--error-unmatch", "--", relative]) && git(&["diff", "--quiet", "HEAD", "--", relative])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LokiMode {
    On,
    Advise,
    Off,
}

/// `lokiTimeoutMs`: ULTRON_LOKI_TIMEOUT_MS, how long a before-write (or before-shell) check may take before the write
/// proceeds unchecked, when it is a positive safe integer. Unset or invalid: None, no limit (TypeScript: 5 s), so a
/// check slowed by machine load still decides the write instead of letting it through unchecked.
pub fn loki_timeout(value: Option<&str>) -> Option<std::time::Duration> {
    let n: f64 = value?.trim().parse().ok()?;
    (jsrt::number::is_safe_integer(n) && n > 0.0).then(|| std::time::Duration::from_millis(n as u64))
}

/// `lokiMode`: ULTRON_LOKI (off | advise | on), else the `loki.mode` setting, else on.
pub fn loki_mode(env: Option<&str>, settings: &Map<String, Value>) -> LokiMode {
    let raw = env.map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
    match raw.as_deref() {
        Some(v) if jsrt::env::flag_off_or_none(Some(v)) => LokiMode::Off,
        Some("advise") => LokiMode::Advise,
        Some(_) => LokiMode::On,
        None => match settings.get("mode").and_then(Value::as_str) {
            Some("off") => LokiMode::Off,
            Some("advise") => LokiMode::Advise,
            _ => LokiMode::On,
        },
    }
}

/// `gitRoot`: the Git work tree containing `cwd`, or None outside one.
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !root.is_empty()).then(|| PathBuf::from(root))
}

pub struct LokiGuard {
    root: PathBuf,
    mode: LokiMode,
    /// NOT CHECKED lines are shown once per session, not after every write.
    shown_notices: Mutex<Vec<String>>,
    /// Findings already shown and not fixed since, per tier and file, so a later cell is not told again in full.
    open: Mutex<HashMap<&'static str, HashMap<String, HashSet<String>>>>,
    paused_shown: AtomicBool,
    /// Runs `hook --file ...` in `root` (default: the Rust engine in-process).
    hook: HookRunner,
    /// Post-write checks run (`postWrite`): they compare against a base revision, so only in a repository.
    post_write: bool,
    /// How long a before-write check may take (`timeoutMs`); None: no limit.
    timeout: Option<std::time::Duration>,
    /// Which hygiene notes follow a post-write check (`loki::hygiene`); off unless the session sets it.
    hygiene: loki::hygiene::Level,
}

/// Runs Loki's post-write `hook` on repository-relative files.
pub type HookRunner = std::sync::Arc<dyn Fn(&Path, &[String]) -> Result<Report, CheckError> + Send + Sync>;

/// One tier's lines not yet shown for their file, how many shown earlier are still there, and their files.
struct Unseen {
    fresh: Vec<String>,
    earlier: usize,
    paths: Vec<String>,
}

impl LokiGuard {
    /// `root`: the repository (or working directory) Loki checks; `mode` is on or advise.
    pub fn new(root: impl Into<PathBuf>, mode: LokiMode) -> Self {
        LokiGuard {
            root: root.into(),
            mode,
            shown_notices: Mutex::new(Vec::new()),
            open: Mutex::new(HashMap::new()),
            paused_shown: AtomicBool::new(false),
            hook: std::sync::Arc::new(|root: &Path, files: &[String]| loki::run::hook(root, files, None)),
            post_write: true,
            timeout: None,
            hygiene: loki::hygiene::Level::Off,
        }
    }

    /// Hygiene advisories for the lines a cell added, after a post-write check that found nothing to fix.
    pub fn with_hygiene(mut self, level: loki::hygiene::Level) -> Self {
        self.hygiene = level;
        self
    }

    /// The limit on each before-write and before-shell check (see [`loki_timeout`]); None: no limit.
    pub fn with_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether the files each cell changed are checked after it (`postWrite`; default on). Outside a Git
    /// repository only before-write checks run.
    pub fn with_post_write(mut self, post_write: bool) -> Self {
        self.post_write = post_write;
        self
    }

    /// Post-write checks ask `python` (the project's interpreter, `findProjectPython`; loki.ts passes it to the
    /// engine as `LOKI_PYTHON`) whether newly imported modules exist.
    pub fn with_project_python(mut self, python: Option<String>) -> Self {
        self.hook =
            std::sync::Arc::new(move |root: &Path, files: &[String]| loki::run::hook(root, files, python.as_deref()));
        self
    }

    /// The same guard with another post-write engine (tests: a stand-in that answers scripted reports).
    pub fn with_hook(mut self, hook: HookRunner) -> Self {
        self.hook = hook;
        self
    }

    /// `#unseen`: the lines of one tier not yet shown for their file; a finding gone from a file checked now was
    /// fixed, and is new again if it comes back.
    fn unseen(&self, tier: &'static str, lines: &[String], checked: &[String]) -> Unseen {
        let mut open = jsrt::sync::lock(&self.open);
        let memory = open.entry(tier).or_default();
        let mut now: Vec<(String, HashSet<String>)> = Vec::new();
        let mut fresh = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for line in lines {
            let (path, key) = loki::finding_key(&self.root, line);
            if !path.is_empty() && memory.get(&path).is_some_and(|keys| keys.contains(&key)) {
                if !stale.contains(&path) {
                    stale.push(path.clone());
                }
            } else {
                fresh.push(line.clone());
            }
            if !path.is_empty() {
                match now.iter_mut().find(|(p, _)| *p == path) {
                    Some((_, keys)) => {
                        keys.insert(key);
                    }
                    None => now.push((path, HashSet::from([key]))),
                }
            }
        }
        for path in checked {
            memory.remove(path);
        }
        for (path, keys) in now {
            memory.insert(path, keys);
        }
        Unseen {
            earlier: lines.len() - fresh.len(),
            fresh,
            paths: stale,
        }
    }

    /// NOT CHECKED notes not yet shown this session, as one short line.
    fn notice(&self, lines: &[String]) -> Option<String> {
        let mut shown = jsrt::sync::lock(&self.shown_notices);
        let mut fresh = Vec::new();
        for line in lines {
            if !shown.contains(line) {
                shown.push(line.clone());
                fresh.push(
                    line.strip_prefix("NOT CHECKED")
                        .unwrap_or(line)
                        .trim_start()
                        .to_string(),
                );
            }
        }
        (!fresh.is_empty()).then(|| format!("not checked in this session (said once): {}", fresh.join("; ")))
    }

    fn refused(&self, reason: String, advise: &str) -> Verdict {
        if self.mode == LokiMode::Advise {
            Verdict::Allow(Some(format!("{advise}:\n{reason}")))
        } else {
            Verdict::Block(reason)
        }
    }
}

impl FileWriteGuard for LokiGuard {
    fn name(&self) -> &str {
        "Loki"
    }

    /// ULTRON_LOKI_TIMEOUT_MS when set (see [`loki_timeout`]); otherwise the in-process engine has no per-call
    /// limit (its tool runs have their own).
    fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout
    }

    fn before_write(&self, path: &Path, content: &str, _cwd: &Path, _lane: &str) -> Result<Verdict, String> {
        let (root, relative) = match inside(&self.root, path) {
            Some(relative) => (self.root.clone(), relative),
            None => {
                let Some((root, relative)) = path
                    .ancestors()
                    .skip(1)
                    .find(|dir| dir.is_dir())
                    .and_then(git_root)
                    .filter(|root| root.join(".loki").is_dir())
                    .and_then(|root| inside(&root, path).map(|relative| (root, relative)))
                else {
                    return Ok(Verdict::Allow(None));
                };
                (root, relative)
            }
        };
        let report = match loki::run::protect_write(&root, &relative, content) {
            Ok(report) => report,
            Err(error) => {
                return Ok(self.refused(
                    format!("loki: {}", error.describe()),
                    "would block this write (advise-only mode)",
                ));
            }
        };
        if report.blocked() {
            return Ok(self.refused(
                report.refusal_lines().join("\n"),
                "would block this write (advise-only mode)",
            ));
        }
        let not_checked: Vec<String> = report.not_checked().cloned().collect();
        let mut lines: Vec<String> = report.advisory().cloned().collect();
        lines.extend(self.notice(&not_checked));
        Ok(Verdict::Allow((!lines.is_empty()).then(|| lines.join("\n"))))
    }

    fn watches_cells(&self) -> bool {
        self.post_write && self.mode != LokiMode::Off
    }

    /// `afterCellChanges`: `hook` on the files a cell changed inside the repository (at most 40).
    fn after_cell_changes(
        &self,
        changes: &CellChanges,
        _cwd: &Path,
        _lane: &str,
    ) -> Result<Option<AfterCellReport>, String> {
        let targets = self.collect_targets(changes);
        if targets.is_empty() {
            return Ok(None);
        }
        let checked: Vec<String> = targets.iter().take(MAX_HOOK_FILES).cloned().collect();
        let report = (self.hook)(&self.root, &checked).map_err(|error| error.describe())?;
        if report.blocked_by_policy_changes() {
            return Ok(self.paused_report());
        }
        let (hygiene_gaps, hygiene): (Vec<String>, Vec<String>) =
            loki::hygiene::written(&self.root, &checked, self.hygiene)
                .into_iter()
                .partition(|line| line.starts_with(loki::run::NOT_CHECKED));
        let advisory: Vec<String> = report.advisory().cloned().collect();
        let not_checked: Vec<String> = report.not_checked().cloned().chain(hygiene_gaps).collect();
        let blocking = self.unseen("blocking", &report.blocking, &checked);
        let advisory = self.unseen("advisory", &advisory, &checked);
        let hygiene = self.unseen("hygiene", &hygiene, &checked);
        let sections = self.format_after_cell_sections(
            &blocking,
            &advisory,
            &hygiene,
            &not_checked,
            targets.len() - checked.len(),
        );
        let outcome = after_cell_outcome(!report.blocking.is_empty(), &hygiene, &advisory);
        Ok((!sections.is_empty()).then(|| AfterCellReport {
            outcome,
            message: Some(sections.join("\n")),
        }))
    }

    fn before_shell(&self, command: &str, cwd: &Path) -> Result<Verdict, String> {
        Ok(match loki::shell::command_refusal(&self.root, command, cwd) {
            None => Verdict::Allow(None),
            Some(reason) => self.refused(
                reason.trim().to_string(),
                "would refuse this command (advise-only mode)",
            ),
        })
    }
}

impl LokiGuard {
    fn collect_targets(&self, changes: &CellChanges) -> Vec<String> {
        let mut targets = Vec::new();
        for path in changes.files.iter().chain(&changes.checked) {
            if let Some(relative) = inside(&self.root, path)
                && !targets.contains(&relative)
                && !loki_file_as_committed(&self.root, &relative)
            {
                targets.push(relative);
            }
        }
        targets
    }

    fn paused_report(&self) -> Option<AfterCellReport> {
        if self.paused_shown.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(AfterCellReport {
            outcome: AfterOutcome::Clean,
            message: Some("post-write checks are paused: .loki/ has uncommitted changes, and Loki checks against the committed policy. Before-write checks of edit() and write() still run. Commit .loki/ to resume them (git add .loki && git commit -m 'Add Loki guardrails' -- .loki).".into()),
        })
    }

    fn format_after_cell_sections(
        &self,
        blocking: &Unseen,
        advisory: &Unseen,
        hygiene: &Unseen,
        not_checked: &[String],
        unchecked_count: usize,
    ) -> Vec<String> {
        let mut sections = Vec::new();
        if !blocking.fresh.is_empty() {
            sections.push(format!(
                "{} from this cell's changes; fix these:\n{}",
                count(blocking.fresh.len(), "new finding"),
                blocking.fresh.join("\n")
            ));
        }
        if blocking.earlier > 0 {
            sections.push(format_earlier_line(blocking.earlier, "finding", &blocking.paths, "."));
        }
        if !advisory.fresh.is_empty() {
            sections.push(format!(
                "FYI (advisory, not blocking; no change required):\n{}",
                advisory.fresh.join("\n")
            ));
        }
        if !hygiene.fresh.is_empty() {
            sections.push(format!(
                "{} on lines this cell added; fix these:\n{}",
                count(hygiene.fresh.len(), "hygiene note"),
                hygiene.fresh.join("\n")
            ));
        }
        if hygiene.earlier > 0 {
            sections.push(format_earlier_line(
                hygiene.earlier,
                "hygiene note",
                &hygiene.paths,
                "; fix these.",
            ));
        }
        sections.extend(self.notice(not_checked));
        if unchecked_count > 0 {
            sections.push(format!("({unchecked_count} more changed files were not checked)"));
        }
        sections
    }
}

fn format_earlier_line(earlier: usize, noun: &str, paths: &[String], suffix: &str) -> String {
    let verb = if earlier == 1 { "is" } else { "are" };
    format!(
        "{} reported earlier {verb} still open in {}{suffix}",
        count(earlier, noun),
        paths.join(", ")
    )
}

fn after_cell_outcome(has_blocking: bool, hygiene: &Unseen, advisory: &Unseen) -> AfterOutcome {
    if has_blocking || !hygiene.fresh.is_empty() || hygiene.earlier > 0 {
        AfterOutcome::Findings
    } else if !advisory.fresh.is_empty() {
        AfterOutcome::Advisory
    } else {
        AfterOutcome::Clean
    }
}
