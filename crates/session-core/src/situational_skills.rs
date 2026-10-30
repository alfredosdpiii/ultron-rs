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

use host::file_hooks::{CellChanges, CellObserver, inside};
use kernel_apis::memory_records::Glob;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// Skills added to one cell's result at most.
pub const PER_CELL: usize = 1;
/// A skill longer than this is named with its file, not added whole (the after-cell findings of a cell are cut at
/// 6,000 characters).
pub const MAX_TEXT_CHARS: usize = 4000;

/// The rules when the settings name none: (skill, situation, paths).
pub const DEFAULT_RULES: &[(&str, &str, &[&str])] = &[
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

/// The situational skills of one session.
pub struct SituationalSkills {
    rules: Vec<Rule>,
    resolve: Box<Resolve>,
    engineering: Box<Engineering>,
    /// (lane, skill) already given.
    given: Mutex<HashSet<(String, String)>>,
}

impl SituationalSkills {
    pub fn new(rules: Vec<Rule>, resolve: Box<Resolve>, engineering: Box<Engineering>) -> SituationalSkills {
        SituationalSkills {
            rules,
            resolve,
            engineering,
            given: Mutex::new(HashSet::new()),
        }
    }

    /// The text for a cell on `lane` that changed `paths` (relative to the session directory, with `/`).
    pub fn text_for(&self, lane: &str, paths: &[String]) -> Vec<String> {
        let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        for rule in &self.rules {
            if out.len() == PER_CELL {
                break;
            }
            if given.contains(&(lane.to_string(), rule.skill.clone())) {
                continue;
            }
            let Some(path) = paths
                .iter()
                .find(|path| rule.paths.iter().any(|glob| glob.matches(path)))
            else {
                continue;
            };
            // A skill that is not loaded (renamed, `--no-skills`) is given to nobody, and asked for again next time.
            let Some((file, text)) = (self.resolve)(&rule.skill) else {
                continue;
            };
            given.insert((lane.to_string(), rule.skill.clone()));
            let head = format!(
                "[skill] {} ({path}): {} applies from here on.",
                rule.situation, rule.skill
            );
            out.push(if text.chars().count() > MAX_TEXT_CHARS {
                format!("{head} Read {file} and follow it.")
            } else {
                format!("{head}\n{}", text.trim())
            });
        }
        out
    }
}

impl CellObserver for SituationalSkills {
    fn enabled(&self) -> bool {
        !self.rules.is_empty() && (self.engineering)()
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
}

/// The session's observer, or `None` when no rule applies.
pub fn session_observer(
    settings: &Value,
    resolve: Box<Resolve>,
    engineering: Box<Engineering>,
) -> Option<Arc<dyn CellObserver>> {
    let rules = rules(settings);
    (!rules.is_empty()).then(|| Arc::new(SituationalSkills::new(rules, resolve, engineering)) as Arc<dyn CellObserver>)
}
