//! The failure-mode register (`ultron autoreview register`): the ways recorded reviews went wrong, counted, so a
//! check can be written for what recurs. Its sources:
//!
//! - the fates ledger: posted findings people rejected, by category, kind of pass and how the fate was seen
//!   ([`Fate::how`]);
//! - each review record's result: what the pipeline dropped (`dropped`: rejected by the verifier, generic,
//!   duplicates, refuted by a test, contradicted by the deterministic stage) and what it did not check
//!   (`notChecked`, numbers and paths taken out so one mode is one line);
//! - the check library ([`crate::checks`]): the hits of each check that has caught its planted bug (an unproven
//!   check's hits are not counted).
//!
//! Rules findings keep citing and people keep accepting are proposed as Loki rules ([`loki_proposals`]): a rule a
//! reviewer has to repeat belongs at write time. A proposal is a `rules.rs` tuple for a person to approve; nothing
//! here changes Loki.

use std::collections::BTreeMap;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::checks::Check;
use crate::eval::Proof;
use crate::ledger::{Fate, FateKind};

/// Examples kept per mode.
pub const EXAMPLES: usize = 3;
/// A rule findings cited is recurring from this many accepted fates, when it is accepted at least twice as often
/// as rejected.
pub const MIN_RECURRING: i64 = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mode {
    pub id: String,
    /// `fate`, `dropped`, `notChecked` or `check`.
    pub source: &'static str,
    pub description: String,
    pub count: u64,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckStatus {
    pub id: &'static str,
    pub rule: &'static str,
    pub flags: &'static str,
    /// It caught its planted bug and stayed quiet without it: only then are its hits counted.
    pub proven: bool,
    pub hits: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Register {
    pub reviews: usize,
    pub fates: usize,
    /// Most frequent first.
    pub modes: Vec<Mode>,
    pub checks: Vec<CheckStatus>,
}

#[derive(Default)]
struct Tally {
    description: String,
    count: u64,
    examples: Vec<String>,
}

impl Tally {
    fn add(&mut self, count: u64, example: Option<String>) {
        self.count += count;
        if let Some(example) = example
            && self.examples.len() < EXAMPLES
            && !self.examples.contains(&example)
        {
            self.examples.push(example);
        }
    }
}

const DROPPED: [(&str, &str); 5] = [
    ("rejected", "findings the verifier rejected"),
    (
        "generic",
        "generic findings (no change a test would miss, no problem that exists now)",
    ),
    ("duplicates", "duplicate findings merged"),
    ("refutedByTest", "tests findings a test run refuted"),
    ("contradicted", "findings the deterministic stage contradicted"),
];

/// A `notChecked` line as a mode: numbers and file paths taken out.
pub fn normalize(line: &str) -> String {
    let path = Regex::new(r"[\w.-]*(/[\w.-]+)+|\b[\w-]+\.[a-z]{1,4}\b").expect("static pattern");
    let number = Regex::new(r"\d+").expect("static pattern");
    let line = path.replace_all(line, "<path>");
    let line = number.replace_all(&line, "N");
    line.chars().take(160).collect()
}

/// The register of `fates` and review `records` (each a review record, its engine result under `result`), with the
/// library's checks run where `proofs` say they count.
pub fn build(fates: &[Fate], records: &[Value], library: &[Check], proofs: &[Proof]) -> Register {
    let mut modes: BTreeMap<(String, &'static str), Tally> = BTreeMap::new();
    for fate in fates.iter().filter(|fate| fate.fate == FateKind::Rejected) {
        let id = format!("fate:{}/{}:{}", fate.category, fate.kind, fate.how);
        let tally = modes.entry((id, "fate")).or_default();
        tally.description = format!(
            "{} findings from the {} pass rejected ({})",
            fate.category, fate.kind, fate.how
        );
        tally.add(1, Some(format!("{} {}:{}", fate.pull, fate.file, fate.line)));
    }
    let proven: Vec<&Check> = library
        .iter()
        .filter(|check| proofs.iter().any(|proof| proof.id == check.id && proof.proven()))
        .collect();
    let mut hits: BTreeMap<&str, u64> = BTreeMap::new();
    for record in records {
        let result = record.get("result").unwrap_or(record);
        let pull = record.get("pull").and_then(Value::as_str).unwrap_or("a review");
        for (key, description) in DROPPED {
            let count = result["dropped"][key]
                .as_u64()
                .or_else(|| result["dropped"][key].as_array().map(|items| items.len() as u64))
                .unwrap_or(0);
            if count > 0 {
                let tally = modes.entry((format!("dropped:{key}"), "dropped")).or_default();
                tally.description = description.to_string();
                tally.add(count, Some(pull.to_string()));
            }
        }
        for line in result["notChecked"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let mode = normalize(line);
            let tally = modes.entry((format!("notChecked:{mode}"), "notChecked")).or_default();
            tally.description = mode;
            tally.add(1, Some(pull.to_string()));
        }
        for check in &proven {
            let found = (check.detect)(result);
            if found.is_empty() {
                continue;
            }
            *hits.entry(check.id).or_default() += found.len() as u64;
            let tally = modes.entry((format!("check:{}", check.id), "check")).or_default();
            tally.description = format!("{} ({})", check.flags, check.rule);
            tally.add(found.len() as u64, found.first().map(|hit| format!("{pull}: {hit}")));
        }
    }
    let mut modes: Vec<Mode> = modes
        .into_iter()
        .map(|((id, source), tally)| Mode {
            id,
            source,
            description: tally.description,
            count: tally.count,
            examples: tally.examples,
        })
        .collect();
    modes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.id.cmp(&b.id)));
    Register {
        reviews: records.len(),
        fates: fates.len(),
        modes,
        checks: library
            .iter()
            .map(|check| CheckStatus {
                id: check.id,
                rule: check.rule,
                flags: check.flags,
                proven: proofs.iter().any(|proof| proof.id == check.id && proof.proven()),
                hits: hits.get(check.id).copied().unwrap_or(0),
            })
            .collect(),
    }
}

/// A Loki rule proposed from a recurring review rule, as a `rules.rs` tuple for a person to approve.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    /// `loki/review-r<id>`.
    pub rule: String,
    /// What it would match: the code the rule quotes (between backticks), escaped; None when it quotes none.
    pub pattern: Option<String>,
    /// Files it would apply to (by the extension of the file the rule was left on).
    pub paths: String,
    pub message: String,
    /// `("rule", r#"pattern"#, "message")`, ready for `crates/loki/src/rules.rs`; None without a pattern.
    pub tuple: Option<String>,
    pub accepted: i64,
    pub rejected: i64,
    pub source: String,
}

/// Proposals for the rules of one repository that recur (see [`MIN_RECURRING`]).
pub fn loki_proposals(rules: &[knowledge::RuleCredit]) -> Vec<Proposal> {
    let quoted = Regex::new(r"`([^`\n]{3,80})`").expect("static pattern");
    rules
        .iter()
        .filter(|rule| rule.accepted >= MIN_RECURRING && rule.accepted >= 2 * rule.rejected)
        .map(|rule| {
            let pattern = quoted
                .captures(&rule.body)
                .map(|found| regex::escape(found[1].trim()))
                .filter(|pattern| !pattern.is_empty());
            let first = rule.body.split(['\n', '.']).find(|part| !part.trim().is_empty());
            let message: String = first.unwrap_or(&rule.body).trim().chars().take(160).collect();
            let name = format!("loki/review-r{}", rule.id);
            let tuple = pattern
                .as_ref()
                .map(|pattern| format!("({name:?}, r#\"{pattern}\"#, {message:?})"));
            Proposal {
                paths: match rule.path.rsplit_once('.') {
                    Some((_, ext)) if !ext.contains('/') => format!("*.{ext}"),
                    _ => "*".into(),
                },
                rule: name,
                pattern,
                message,
                tuple,
                accepted: rule.accepted,
                rejected: rule.rejected,
                source: rule.url.clone(),
            }
        })
        .collect()
}

/// Where the proposals are written, under the autoreview directory.
pub const PROPOSALS_FILE: &str = "loki-proposals.json";

/// `host/owner/name` (lowercased) of `--repo owner/name` or `host/owner/name`.
pub fn repo_key(repo: &str) -> String {
    let repo = repo.trim().trim_matches('/').to_lowercase();
    if repo.matches('/').count() == 1 {
        format!("github.com/{repo}")
    } else {
        repo
    }
}

/// The register and the Loki proposals from what `ultron autoreview` keeps on disk: the state file's fates, the
/// review records (`<dir>/reviews/*.json`) and the knowledge store's rule credits; one repository's (`repo`, a
/// [`repo_key`]) or every repository's. The library's checks are proven first.
pub fn from_disk(paths: &crate::settings::AutoreviewPaths, repo: Option<&str>) -> (Register, Vec<Proposal>) {
    let state = crate::state::StateStore::new(&paths.state).read();
    let lessons = state.lessons.unwrap_or_default();
    let repos: Vec<String> = match repo {
        Some(repo) => vec![repo.to_string()],
        None => lessons.keys().cloned().collect(),
    };
    let fates: Vec<Fate> = repos
        .iter()
        .flat_map(|repo| lessons.get(repo).cloned().unwrap_or_default())
        .collect();
    let mut records: Vec<Value> = std::fs::read_dir(std::path::Path::new(&paths.dir).join("reviews"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|entry| serde_json::from_str::<Value>(&std::fs::read_to_string(entry.path()).ok()?).ok())
        .filter(|record| {
            repo.is_none_or(|repo| {
                record["pull"]
                    .as_str()
                    .is_some_and(|pull| pull.to_lowercase().starts_with(&format!("{repo}#")))
            })
        })
        .collect();
    records.sort_by(|a, b| a["pull"].as_str().cmp(&b["pull"].as_str()));
    let library = &crate::checks::LIBRARY;
    let proofs = crate::eval::prove(library, &crate::eval::planted());
    let register = build(&fates, &records, library, &proofs);
    let db = std::path::Path::new(&paths.knowledge);
    let proposals = if db.exists() {
        knowledge::Store::open(db)
            .map(|store| {
                repos
                    .iter()
                    .flat_map(|repo| loki_proposals(&store.credited_rules(repo).unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    (register, proposals)
}
