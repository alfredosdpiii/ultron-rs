//! Head-pinned, blinded scoring of recorded reviews against adjudicated labels (`ultron autoreview eval --labels`).
//! Nothing here reviews or calls a model: it re-reads review records (`<agent dir>/autoreview/reviews/*.json`, one
//! engine result at one head each) and a labels file a person wrote.
//!
//! - **Head-pinned**: each case names the head commit; a record of another head is refused. With a repository,
//!   every defect's fixing commit must descend from the head, so a defect is a fact learnt later, not one the
//!   reviewed code already showed fixed.
//! - **Blinded**: [`packet`] gives the adjudicator each finding's claim, place and fix only, under an id derived from
//!   the head and the claim, in id order: no level, verification, confidence, source, reasoning, rank or posting.
//! - **Discovery and posting apart**: discovery is every finding the engine returned (confirmed or not); posting is
//!   what the record says went out on the pull request. A defect counts as caught only when a finding about it was
//!   labelled right on both its trigger and its consequence; one right on only one is `partial`, not a catch.
//! - Findings outside the defect list are `novel` (a real problem the labels did not hold) or `unsupported`; the
//!   suggested fix is judged on its own (`correct`, `partial`, `wrong`), never folded into the finding's verdict.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::plan::{EngineFinding, EngineResult};
use crate::runner::{Env, RunOptions, Runner, argv};
use crate::state::claim_hash;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Labels {
    pub cases: Vec<LabelCase>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelCase {
    /// The full head commit the review read.
    pub head: String,
    /// The review record's path, relative to the labels file.
    pub record: String,
    #[serde(default)]
    pub defects: Vec<Defect>,
    /// Verdicts by blinded finding id ([`packet`]).
    #[serde(default)]
    pub findings: BTreeMap<String, FindingLabel>,
}

/// A defect the head carried, as a later fix shows it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Defect {
    pub id: String,
    pub trigger: String,
    pub consequence: String,
    /// The commit that fixed it.
    pub fixed_by: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingLabel {
    /// `defect` (it is about one of the case's defects), `novel` (a real problem the defects do not hold) or
    /// `unsupported`.
    pub verdict: String,
    /// `defect`: which one, and whether the finding names its trigger and its consequence.
    pub defect: Option<String>,
    #[serde(default)]
    pub trigger: bool,
    #[serde(default)]
    pub consequence: bool,
    /// `correct`, `partial` or `wrong`; absent when the finding offered no fix or it was not judged.
    pub fix: Option<String>,
}

const VERDICTS: [&str; 3] = ["defect", "novel", "unsupported"];
const FIXES: [&str; 3] = ["correct", "partial", "wrong"];

/// One recorded review at its head: the engine result and the claim hashes that were posted.
pub struct Recorded {
    pub sha: String,
    pub result: EngineResult,
    pub posted: HashSet<String>,
}

/// A review record (`reviews/*.json`): `sha`, `result` and `posted[].claimHash`.
pub fn parse_record(record: &Value) -> Result<Recorded, String> {
    let sha = record["sha"]
        .as_str()
        .ok_or("the record names no head (sha)")?
        .to_string();
    let result = EngineResult::parse(&record["result"])?;
    let posted = record["posted"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["claimHash"].as_str().map(str::to_string))
        .collect();
    Ok(Recorded { sha, result, posted })
}

/// The blinded id of each finding, in the result's order: the head and the claim hashed, a suffix on a repeat.
pub fn blind_ids(head: &str, findings: &[EngineFinding]) -> Vec<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    findings
        .iter()
        .map(|finding| {
            let base = claim_hash(&format!("{head}\n{}:{}\n{}", finding.file, finding.line, finding.claim));
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 { base } else { format!("{base}-{count}") }
        })
        .collect()
}

/// What the adjudicator sees of one case: its head and each finding's place, claim and fix, in blinded-id order.
pub fn packet(recorded: &Recorded) -> Value {
    let ids = blind_ids(&recorded.sha, &recorded.result.findings);
    let mut items: Vec<(String, Value)> = ids
        .into_iter()
        .zip(&recorded.result.findings)
        .map(|(id, finding)| {
            let fix = finding.replacement.clone().or_else(|| finding.suggested_fix.clone());
            let item = json!({"id": id, "file": finding.file, "line": finding.line, "endLine": finding.end_line,
                "claim": finding.claim, "fix": fix});
            (id, item)
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    json!({"head": recorded.sha, "findings": items.into_iter().map(|(_, item)| item).collect::<Vec<_>>()})
}

/// The labels' own mistakes: an unknown verdict or fix, a match with no known defect, a label for no finding.
fn check_case(case: &LabelCase, ids: &[String]) -> Vec<String> {
    let defects: HashSet<&str> = case.defects.iter().map(|d| d.id.as_str()).collect();
    let known: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut problems = Vec::new();
    for (id, label) in &case.findings {
        if !known.contains(id.as_str()) {
            problems.push(format!("{id}: no finding of head {} has this id", case.head));
        }
        if !VERDICTS.contains(&label.verdict.as_str()) {
            problems.push(format!(
                "{id}: verdict {} is not one of {}",
                label.verdict,
                VERDICTS.join(", ")
            ));
        }
        if (label.verdict == "defect") != label.defect.as_deref().is_some_and(|d| defects.contains(d)) {
            problems.push(format!(
                "{id}: only a defect verdict names a defect, and it must be one of the case's"
            ));
        }
        if let Some(fix) = &label.fix
            && !FIXES.contains(&fix.as_str())
        {
            problems.push(format!("{id}: fix {fix} is not one of {}", FIXES.join(", ")));
        }
    }
    problems
}

/// The pin: the record's head is the case's head, and with a repository each fix descends from it.
pub fn check_pin(case: &LabelCase, recorded: &Recorded, repo: Option<(&dyn Runner, &str)>) -> Vec<String> {
    let mut problems = Vec::new();
    if recorded.sha != case.head {
        problems.push(format!("the record is of head {}, not {}", recorded.sha, case.head));
    }
    let Some((runner, repo)) = repo else {
        return problems;
    };
    let mut env = Env::new();
    env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    env.insert("GIT_NO_LAZY_FETCH".into(), "1".into());
    let options = RunOptions {
        env,
        timeout_ms: Some(60_000),
        ..RunOptions::default()
    };
    let ancestor = |older: &str, newer: &str| {
        let parts = ["git", "-C", repo, "merge-base", "--is-ancestor", older, newer];
        runner.run(&argv(&parts), &options).code == 0
    };
    for defect in &case.defects {
        if !ancestor(&case.head, &defect.fixed_by) || ancestor(&defect.fixed_by, &case.head) {
            problems.push(format!(
                "{}: fix {} does not come after head {}",
                defect.id, defect.fixed_by, case.head
            ));
        }
    }
    problems
}

#[derive(Default)]
struct Tally {
    findings: usize,
    labelled: usize,
    matched: usize,
    partial: usize,
    novel: usize,
    unsupported: usize,
    caught: HashSet<String>,
}

impl Tally {
    fn add(&mut self, case: usize, label: Option<&FindingLabel>) {
        self.findings += 1;
        let Some(label) = label else { return };
        self.labelled += 1;
        match label.verdict.as_str() {
            "defect" if label.trigger && label.consequence => {
                self.matched += 1;
                self.caught
                    .insert(format!("{case}/{}", label.defect.clone().unwrap_or_default()));
            }
            "defect" => self.partial += 1,
            "novel" => self.novel += 1,
            _ => self.unsupported += 1,
        }
    }

    fn json(&self, defects: usize) -> Value {
        let valid = self.matched + self.novel;
        json!({"findings": self.findings, "labelled": self.labelled, "matched": self.matched,
            "partial": self.partial, "novel": self.novel, "unsupported": self.unsupported,
            "defectsCaught": self.caught.len(), "defects": defects,
            "recall": share(self.caught.len(), defects), "validShare": share(valid, self.labelled)})
    }
}

fn share(part: usize, whole: usize) -> Value {
    if whole == 0 {
        Value::Null
    } else {
        json!(((part as f64 / whole as f64) * 1000.0).round() / 1000.0)
    }
}

/// Scores every case: discovery (every finding) and posting (the posted ones) apart, fixes on their own.
/// `cases` pairs each labelled case with its record; any problem with the pin or the labels is an error.
pub fn score(cases: &[(LabelCase, Recorded)], repo: Option<(&dyn Runner, &str)>) -> Result<Value, Vec<String>> {
    let mut problems = Vec::new();
    let (mut discovery, mut posting) = (Tally::default(), Tally::default());
    let mut fixes: BTreeMap<&str, usize> = BTreeMap::new();
    let mut defects = 0;
    let mut rows = Vec::new();
    for (index, (case, recorded)) in cases.iter().enumerate() {
        let ids = blind_ids(&recorded.sha, &recorded.result.findings);
        let mut case_problems = check_pin(case, recorded, repo);
        case_problems.extend(check_case(case, &ids));
        problems.extend(
            case_problems
                .into_iter()
                .map(|problem| format!("{}: {problem}", case.record)),
        );
        defects += case.defects.len();
        let (mut found, mut sent) = (Tally::default(), Tally::default());
        for (id, finding) in ids.iter().zip(&recorded.result.findings) {
            let label = case.findings.get(id);
            discovery.add(index, label);
            found.add(index, label);
            if recorded.posted.contains(&claim_hash(&finding.claim)) {
                posting.add(index, label);
                sent.add(index, label);
            }
            if let Some(fix) = label.and_then(|label| label.fix.as_deref()) {
                *fixes.entry(fix).or_default() += 1;
            }
        }
        rows.push(json!({"head": case.head, "record": case.record,
            "discovery": found.json(case.defects.len()), "posting": sent.json(case.defects.len())}));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(
        json!({"cases": rows, "discovery": discovery.json(defects), "posting": posting.json(defects),
        "fixes": {"correct": fixes.get("correct").copied().unwrap_or(0),
            "partial": fixes.get("partial").copied().unwrap_or(0), "wrong": fixes.get("wrong").copied().unwrap_or(0)}}),
    )
}

pub fn summary_lines(scored: &Value) -> Vec<String> {
    let line = |name: &str, part: &Value| {
        let n = |key: &str| part[key].as_u64().unwrap_or(0);
        format!(
            "{name}: {} of {} defects caught; of {} findings ({} labelled), {} match, {} partial, {} novel, {} unsupported.",
            n("defectsCaught"),
            n("defects"),
            n("findings"),
            n("labelled"),
            n("matched"),
            n("partial"),
            n("novel"),
            n("unsupported")
        )
    };
    let fixes = &scored["fixes"];
    vec![
        format!(
            "{} case(s), head-pinned.",
            scored["cases"].as_array().map_or(0, Vec::len)
        ),
        line("Discovery (every finding)", &scored["discovery"]),
        line("Posting (posted findings)", &scored["posting"]),
        format!(
            "Fixes judged: {} correct, {} partial, {} wrong.",
            fixes["correct"].as_u64().unwrap_or(0),
            fixes["partial"].as_u64().unwrap_or(0),
            fixes["wrong"].as_u64().unwrap_or(0)
        ),
    ]
}
