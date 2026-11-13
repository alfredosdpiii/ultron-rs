//! Situational skills (Rust-only, engineering mode): the skill that fits what a cell just did is put in front of the
//! model, instead of trusting the model to read it.
//!
//! Problem: the prompt says when to read each situational principle (the `situational-principles` always-on skill,
//! the bundled-skills note), and how often a model does depends on the model. Over two weeks of sessions, one model
//! read a fitting skill in 6 of 8 long sessions and another in none of 3, and the "any change beyond a quick edit"
//! playbook was read once.
//!
//! Fix: a cell observer (host::file_hooks::CellObserver, beside kernel_apis::memory_pointers). When a cell on a lane
//! changes a file matching a rule's paths, that lane's cell result ends with the rule's skill, its text whole:
//!
//! ```text
//! [skill] You changed a test file (tests/test_totals.py): principle-test-behavior-not-implementation applies from
//! here on.
//! <the skill's text>
//! ```
//!
//! Each skill reaches each lane once per session, at most [`PER_CELL`] per cell (another matching rule waits for its
//! next matching change), and a skill longer than [`MAX_TEXT_CHARS`] is named with its file instead. Rules are the
//! `situationalSkills` setting, a list of `{"skill", "paths", "situation"}` ([`DEFAULT_RULES`] when it is absent,
//! none when it is `false` or empty). A rule whose skill is always on (`alwaysSkills`) is skipped: its text is in the
//! prompt already. The observer works only while engineering mode is on, read from the settings at each cell, so
//! `/engineering off` stops it at once.
//!
//! Two skills follow the shape of the work, not a path: [`MANY_FILES_SKILL`] once a lane has changed [`MANY_FILES`]
//! files, and [`SHIP_SKILL`] as the refusal of a lane's first command that commits, pushes or opens a pull request.
//!
//! That refusal is a reminder: the same command goes through the second time, reviewed or not. So opening a pull
//! request also needs a receipt ([`SelfReview::Require`], the default of the `selfReview` setting): `/review` writes
//! `<git dir>/ultron-review/receipt.json` naming the tree it reviewed, and `gh pr create` is refused until that
//! tree is the one committed, with no confirmed finding left unwaived. A new commit is a new tree, so it needs a
//! new review. Its body must then be the description `spec_api.description` wrote for that tree
//! ([`description_refusal`]); what makes a description writable is decided in spec_api.py.

use host::file_hooks::{CellChanges, CellObserver, inside};
use kernel_apis::memory_records::Glob;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// Skills added to one cell's result at most.
pub const PER_CELL: usize = 1;
/// A skill longer than this is named with its file, not added whole (the after-cell findings of a cell are cut at
/// 6,000 characters).
pub const MAX_TEXT_CHARS: usize = 4000;

/// The rules when the settings name none: (skill, situation, paths).
const DEFAULT_RULES: &[(&str, &str, &[&str])] = &[
    (
        "principle-test-behavior-not-implementation",
        "You changed a test file",
        &[
            "**/tests/**",
            "**/test/**",
            "**/__tests__/**",
            "**/test_*.py",
            "**/*_test.*",
            "**/*.test.*",
            "**/*.spec.*",
        ],
    ),
    (
        "principle-minimize-reader-load",
        "You changed documentation",
        &["**/*.md", "**/docs/**"],
    ),
    (
        "typescript-best-practices",
        "You changed TypeScript",
        &["**/*.ts", "**/*.tsx"],
    ),
    (
        "principle-make-operations-idempotent",
        "You changed a migration or a deploy step",
        &[
            "**/migrations/**",
            "**/migration/**",
            "**/alembic/**",
            "**/*.sql",
            "**/*.service",
            "**/Dockerfile",
            "**/.github/workflows/**",
        ],
    ),
    (
        "principle-explain-the-number",
        "You changed a benchmark",
        &["**/benches/**", "**/bench/**", "**/benchmarks/**", "**/*bench*.*"],
    ),
    (
        "principle-boundary-discipline",
        "You changed an API or CLI boundary",
        &["**/api/**", "**/routes/**", "**/handlers/**", "**/cli/src/**"],
    ),
    (
        "principle-type-system-discipline",
        "You changed typed code",
        &[
            "**/*.rs",
            "**/*.go",
            "**/*.java",
            "**/*.kt",
            "**/*.swift",
            "**/*.cs",
            "**/*.scala",
        ],
    ),
];

/// Edited files on one lane from which [`MANY_FILES_SKILL`] applies.
pub const MANY_FILES: usize = 3;
/// The playbook for a change across several files.
pub const MANY_FILES_SKILL: &str = "rigor";
/// The part of [`MANY_FILES_SKILL`] it brings: the routes, not its opening (an opening and a pointer were not followed).
pub const MANY_FILES_PART: &str = "Playbooks";
/// The check before a commit, push or pull request.
pub const SHIP_SKILL: &str = "blast-radius";

/// What shipping asks of a lane (the `selfReview` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfReview {
    /// Nothing is refused.
    Off,
    /// The first commit, push or pull request of a lane is refused once, with [`SHIP_SKILL`].
    Advise,
    /// As `Advise`, and a pull request needs a review receipt for the committed tree.
    Require,
}

/// The `selfReview` setting: `"off"`, `"advise"`, else `Require`.
pub fn self_review(settings: &Value) -> SelfReview {
    match settings["selfReview"].as_str() {
        Some("off") => SelfReview::Off,
        Some("advise") => SelfReview::Advise,
        _ => SelfReview::Require,
    }
}

/// `command` without its quoted text: a search for the words "gh pr create" ships nothing.
// ponytail: a command inside quotes (`bash -c "git push"`) is not seen; parse the shell if that gets used to dodge.
fn unquoted(command: &str) -> String {
    static QUOTED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"'[^']*'|"[^"]*""#).expect("valid"));
    QUOTED.replace_all(command, "''").into_owned()
}

/// Whether `command` (unquoted) commits, pushes or opens a pull request.
fn ships(command: &str) -> bool {
    static SHIP: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\bgit\s+(-C\s+\S+\s+)?(commit|push)\b|\bgh\s+pr\s+create\b").expect("valid")
    });
    SHIP.is_match(command)
}

/// Where `command` (unquoted) opens its pull request: the last `cd <dir>` before `gh pr create`, else `cwd`. `None`
/// when it opens none.
// ponytail: a quoted or `~` directory is not followed and falls back to `cwd`; parse the shell if that matters.
fn pr_directory(command: &str, cwd: &Path) -> Option<PathBuf> {
    static CREATE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\bgh\s+pr\s+create\b").expect("valid"));
    directory_of(command, cwd, &CREATE)
}

/// Where `command` (unquoted) pushes from, by the same rule. `None` when it pushes nothing.
fn push_directory(command: &str, cwd: &Path) -> Option<PathBuf> {
    static PUSH: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\bgit\s+push\b").expect("valid"));
    directory_of(command, cwd, &PUSH)
}

fn directory_of(command: &str, cwd: &Path, what: &regex::Regex) -> Option<PathBuf> {
    static CD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:^|[;&|(\n])\s*cd\s+([^\s;&|)']+)").expect("valid"));
    let before = &command[..what.find(command)?.start()];
    Some(match CD.captures_iter(before).last() {
        Some(found) => cwd.join(&found[1]),
        None => cwd.to_path_buf(),
    })
}

/// A git query's output in `dir`, `None` when it fails or prints nothing.
fn git_value(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// Why `command` (as typed, quotes kept) may not open its pull request from `dir` yet: `None` when its body is a
/// file `spec_api.description` wrote for the tree committed there (the file's first line names that tree, so a
/// commit after it makes it stale). The description is where the spec's acceptance items, the recorded checks and
/// the change's purposes are held against the diff (spec_api.py); this only checks that it was done for this tree.
fn description_refusal(command: &str, dir: &Path) -> Option<String> {
    static BODY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?:--body-file|\s-F)(?:\s+|=)(?:"([^"]*)"|'([^']*)'|([^\s;&|)]+))"#).expect("valid")
    });
    let tree = git_value(dir, &["rev-parse", "HEAD^{tree}"])?;
    let path = BODY
        .captures(command)
        .and_then(|found| found.iter().skip(1).flatten().next())
        .map(|path| dir.join(path.as_str()));
    let described = path
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|text| text.lines().next() == Some(format!("<!-- ultron-description tree={tree} -->").as_str()));
    (!described).then(|| {
        format!(
            "the description comes from the diff: run `import spec_api; path = spec_api.description(cwd={:?})`              (it names what is still open, if anything), then open the pull request with `--body-file <path>` in              place of --fill or --body. A commit after it needs a new one.",
            dir.display().to_string()
        )
    })
}

/// The tree the description kept in `dir`'s repository was written for, when it is not the one committed there now:
/// (described, committed). A push of the second makes a pull request's body describe a change it no longer carries.
fn stale_description(dir: &Path) -> Option<(String, String)> {
    let tree = git_value(dir, &["rev-parse", "HEAD^{tree}"])?;
    let git_dir = git_value(dir, &["rev-parse", "--absolute-git-dir"])?;
    let text = std::fs::read_to_string(Path::new(&git_dir).join("ultron-review/description.md")).ok()?;
    let described = text
        .lines()
        .next()?
        .strip_prefix("<!-- ultron-description tree=")?
        .strip_suffix(" -->")?;
    (described != tree).then(|| (described.to_string(), tree))
}

/// Why a pull request may not be opened from `dir` yet: `None` when `/review` wrote a receipt for the tree committed
/// there and no confirmed finding is left unwaived. Unreadable state refuses.
fn receipt_refusal(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| git_value(dir, args);
    let how = format!(
        "commit the change, run `import review_api; review = await review_api.run(rlm, \"\", cwd={:?}); \
         print(review.report)`, then run the command again",
        dir.display().to_string()
    );
    let Some(tree) = git(&["rev-parse", "HEAD^{tree}"]) else {
        return Some(format!(
            "self-review comes first: no commit to review in {}; {how}.",
            dir.display()
        ));
    };
    let receipt = git(&["rev-parse", "--absolute-git-dir"])
        .and_then(|git_dir| std::fs::read_to_string(Path::new(&git_dir).join("ultron-review/receipt.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|receipt| receipt["tree"] == tree.as_str());
    let Some(receipt) = receipt else {
        return Some(format!(
            "self-review comes first: the committed tree {} has not been reviewed (a commit after a review needs a \
             new one); {how}.",
            &tree[..tree.len().min(8)]
        ));
    };
    let confirmed = receipt["confirmed"].as_u64().unwrap_or(u64::MAX);
    let incomplete = receipt["incomplete"] != false;
    let waived = receipt["waived"]
        .as_str()
        .is_some_and(|reason| !reason.trim().is_empty());
    ((confirmed > 0 || incomplete) && !waived).then(|| {
        let unfinished = if incomplete {
            " and did not finish (see its report)"
        } else {
            ""
        };
        format!(
            "self-review comes first: the review of this tree confirmed {confirmed} finding(s){unfinished}. Fix \
             them, commit and review again (raise --budget for an unfinished one); or, for findings that are wrong \
             or belong in a follow-up, record why with \
             `review_api.waive(\"<reason>\", cwd={:?})` and say so in the pull request's description.",
            dir.display().to_string()
        )
    })
}

/// `text` whole when it fits [`MAX_TEXT_CHARS`]; otherwise its paragraphs up to the bound, the headings of the
/// sections left out, and where to read them (a pointer alone was not followed).
pub fn excerpt(text: &str, file: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text.to_string();
    }
    let bound = text.char_indices().nth(MAX_TEXT_CHARS).map_or(text.len(), |(at, _)| at);
    let mut cut = text[..bound].rfind("\n\n").unwrap_or(bound);
    while let Some(at) = text[..cut].rfind("\n\n")
        && text[at + 2..cut].starts_with('#')
    {
        cut = at;
    }
    let rest: Vec<&str> = text[cut..]
        .lines()
        .filter(|line| line.starts_with("## "))
        .map(|line| line.trim_start_matches("## "))
        .collect();
    format!(
        "{}\n\n[The rest, {}, is in {file}: read it before you act on it.]",
        text[..cut].trim_end(),
        rest.join(" · ")
    )
}

/// The `## heading` section of `text`, heading included, up to the next `## ` heading.
pub fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let start = text.find(&format!("\n## {heading}\n"))? + 1;
    let end = text[start + 3..].find("\n## ").map_or(text.len(), |at| start + 3 + at);
    Some(text[start..end].trim_end())
}

/// One rule: a change to a file matching `paths` brings `skill`.
pub struct Rule {
    pub skill: String,
    pub situation: String,
    pub paths: Vec<Glob>,
}

/// The rules of `settings` (`situationalSkills`), without those whose skill is always on. A rule with no skill or
/// no valid path is left out.
pub fn rules(settings: &Value) -> Vec<Rule> {
    let always: HashSet<&str> = settings["alwaysSkills"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let rule = |skill: &str, situation: &str, paths: &mut dyn Iterator<Item = &str>| {
        let paths: Vec<Glob> = paths.filter_map(|pattern| Glob::new(pattern).ok()).collect();
        (!skill.is_empty() && !paths.is_empty() && !always.contains(skill)).then(|| Rule {
            skill: skill.to_string(),
            situation: if situation.is_empty() {
                "You changed a matching file".to_string()
            } else {
                situation.to_string()
            },
            paths,
        })
    };
    match &settings["situationalSkills"] {
        Value::Null => DEFAULT_RULES
            .iter()
            .filter_map(|(skill, situation, paths)| rule(skill, situation, &mut paths.iter().copied()))
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let paths = item["paths"].as_array()?;
                rule(
                    item["skill"].as_str()?,
                    item["situation"].as_str().unwrap_or_default(),
                    &mut paths.iter().filter_map(Value::as_str),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A skill's file and its text without the frontmatter, by name.
pub type Resolve = dyn Fn(&str) -> Option<(String, String)> + Send + Sync;
/// Whether engineering mode is on now.
pub type Engineering = dyn Fn() -> bool + Send + Sync;

pub struct SituationalSkills {
    rules: Vec<Rule>,
    resolve: Box<Resolve>,
    engineering: Box<Engineering>,
    /// (lane, skill) already given.
    given: Mutex<HashSet<(String, String)>>,
    /// (lane, file) changed so far, for [`MANY_FILES`].
    changed: Mutex<HashSet<(String, String)>>,
    self_review: SelfReview,
    /// Where the session's commands run, for the receipt of a pull request.
    cwd: PathBuf,
}

impl SituationalSkills {
    pub fn new(rules: Vec<Rule>, resolve: Box<Resolve>, engineering: Box<Engineering>) -> SituationalSkills {
        SituationalSkills {
            rules,
            resolve,
            engineering,
            given: Mutex::new(HashSet::new()),
            changed: Mutex::new(HashSet::new()),
            self_review: SelfReview::Advise,
            cwd: PathBuf::new(),
        }
    }

    /// What shipping asks, for commands that run in `cwd`.
    pub fn with_self_review(mut self, self_review: SelfReview, cwd: &Path) -> SituationalSkills {
        (self.self_review, self.cwd) = (self_review, cwd.to_path_buf());
        self
    }

    /// `skill` for `lane` once, introduced by `head`: its `## part` section when it has one (the part to act on,
    /// with where the rest lives), else the skill whole when short or excerpted when long.
    fn give_part(&self, lane: &str, skill: &str, head: String, part: &str) -> Option<String> {
        let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
        if given.contains(&(lane.to_string(), skill.to_string())) {
            return None;
        }
        let (file, text) = (self.resolve)(skill)?;
        given.insert((lane.to_string(), skill.to_string()));
        let dir = Path::new(&file)
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        Some(match section(text.trim(), part) {
            Some(body) => format!(
                "{head}\n{body}\n\n[Paths are relative to {dir}. Pick the playbook now and read it before the next edit; \
                 the whole skill is {file}.]"
            ),
            None => format!("{head}\n{}", excerpt(text.trim(), &file)),
        })
    }

    /// `skill` for `lane` once, introduced by `head`; whole when short, excerpted when long.
    fn give(&self, lane: &str, skill: &str, head: String) -> Option<String> {
        let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
        if given.contains(&(lane.to_string(), skill.to_string())) {
            return None;
        }
        // A skill that is not loaded (renamed, `--no-skills`) is given to nobody, and asked for again next time.
        let (file, text) = (self.resolve)(skill)?;
        given.insert((lane.to_string(), skill.to_string()));
        Some(format!("{head}\n{}", excerpt(text.trim(), &file)))
    }

    /// [`MANY_FILES_SKILL`] once `lane` has changed [`MANY_FILES`] files; it does not count toward [`PER_CELL`].
    pub fn many_files(&self, lane: &str, paths: &[String]) -> Option<String> {
        let count = {
            let mut changed = self.changed.lock().unwrap_or_else(PoisonError::into_inner);
            changed.extend(paths.iter().map(|path| (lane.to_string(), path.clone())));
            changed.iter().filter(|(l, _)| l == lane).count()
        };
        (count >= MANY_FILES).then(|| {
            self.give_part(
                lane,
                MANY_FILES_SKILL,
                format!("[skill] You changed {count} files: {MANY_FILES_SKILL} applies from here on."),
                MANY_FILES_PART,
            )
        })?
    }

    /// The text for a cell on `lane` that changed `paths` (relative to the session directory, with `/`).
    pub fn text_for(&self, lane: &str, paths: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for rule in &self.rules {
            if out.len() == PER_CELL {
                break;
            }
            let Some(path) = paths
                .iter()
                .find(|path| rule.paths.iter().any(|glob| glob.matches(path)))
            else {
                continue;
            };
            let head = format!(
                "[skill] {} ({path}): {} applies from here on.",
                rule.situation, rule.skill
            );
            out.extend(self.give(lane, &rule.skill, head));
        }
        out.extend(self.many_files(lane, paths));
        out
    }
}

impl CellObserver for SituationalSkills {
    fn enabled(&self) -> bool {
        (!self.rules.is_empty() || self.self_review != SelfReview::Off) && (self.engineering)()
    }

    fn cell_changed(&self, changes: &CellChanges, cwd: &Path, lane: &str) -> Vec<String> {
        let paths: Vec<String> = changes
            .files
            .iter()
            .chain(&changes.checked)
            // A file in another repository (a sibling worktree) matches by its path in that repository.
            .filter_map(|path| {
                inside(cwd, path).or_else(|| {
                    let dir = path.ancestors().skip(1).find(|dir| dir.is_dir())?;
                    inside(&host::loki_guard::git_root(dir)?, path)
                })
            })
            .map(|relative| relative.replace('\\', "/"))
            .collect();
        self.text_for(lane, &paths)
    }

    /// The lane's first commit or push is refused once, with [`SHIP_SKILL`]: given after it ran, the check came
    /// too late to shape it. Running the same command again goes through, except a pull request without its
    /// receipt ([`SelfReview::Require`]).
    fn before_shell(&self, typed: &str, lane: &str) -> Option<String> {
        let command = unquoted(typed);
        if self.self_review == SelfReview::Off || !ships(&command) {
            return None;
        }
        let head = format!(
            "{SHIP_SKILL} comes first: check what this change could break outside its diff, then run the command \
             again (it is refused once per lane)."
        );
        self.give(lane, SHIP_SKILL, head).or_else(|| {
            if self.self_review != SelfReview::Require {
                return None;
            }
            if let Some(dir) = pr_directory(&command, &self.cwd) {
                return receipt_refusal(&dir).or_else(|| description_refusal(typed, &dir));
            }
            // A push stays advisory: said once per tree, then the same command goes through.
            let dir = push_directory(&command, &self.cwd)?;
            let (described, tree) = stale_description(&dir)?;
            let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
            given
                .insert((lane.to_string(), format!("description {tree}")))
                .then(|| {
                    format!(
                        "the description was written for tree {}, and this push carries {}: an open pull request would \
                     describe a change it no longer carries. Run the push again (it is refused once per tree); then \
                     review the new tree, run `spec_api.description(cwd={:?})` and update the pull request with \
                     `gh pr edit --body-file <path>`.",
                        &described[..described.len().min(8)],
                        &tree[..tree.len().min(8)],
                        dir.display().to_string()
                    )
                })
        })
    }
}

/// The session's observer, or `None` when no rule applies.
pub fn session_observer(
    settings: &Value,
    cwd: &Path,
    resolve: Box<Resolve>,
    engineering: Box<Engineering>,
) -> Option<Arc<dyn CellObserver>> {
    let (rules, self_review) = (rules(settings), self_review(settings));
    (!rules.is_empty() || self_review != SelfReview::Off).then(|| {
        Arc::new(SituationalSkills::new(rules, resolve, engineering).with_self_review(self_review, cwd))
            as Arc<dyn CellObserver>
    })
}
