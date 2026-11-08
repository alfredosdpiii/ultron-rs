//! The autoreview fates ledger (`autoreview/lessons.ts`): what happened to each posted finding, and the rules a
//! repository's recent fates support. Pure functions; the state file and Hindsight retention live elsewhere.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const MIN_SAMPLES: u32 = 5;
pub const LOWER_BELOW: f64 = 0.5;
pub const FATE_DAYS: i64 = 180;
pub const MAX_FATES_PER_REPO: usize = 400;
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FateKind {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fate {
    /// The posted finding's id within its pull request.
    pub id: String,
    /// `host/owner/repo#N`.
    pub pull: String,
    pub file: String,
    pub line: i64,
    pub category: String,
    /// The kind of pass that raised it: `fast`, `deep:<lens>`, `structure:<shape>`, `compiled` or `hybrid`.
    pub kind: String,
    pub level: String,
    pub claim: String,
    pub fate: FateKind,
    /// How the fate was observed.
    pub how: String,
    /// ISO time of the observation.
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lesson {
    pub category: String,
    pub kind: String,
    pub accepted: u32,
    pub rejected: u32,
    /// Fewer than half accepted on a full sample: such findings are posted at `low`.
    pub lowered: bool,
}

/// What a review thread shows about a posted finding.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadState {
    pub id: String,
    pub resolved: bool,
    #[serde(default)]
    pub thumbs_up: u32,
    #[serde(default)]
    pub thumbs_down: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostedFinding {
    pub id: String,
    pub file: String,
    pub line: i64,
    pub claim: String,
    pub severity: String,
    pub category: Option<String>,
    pub kind: Option<String>,
    pub comment_id: Option<i64>,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct FateInput {
    pub pull: String,
    pub now: String,
    pub findings: Vec<PostedFinding>,
    /// The re-review's status of each earlier finding, by id (absent: the state's own status).
    pub statuses: HashMap<String, String>,
    /// Threads by the database id of their first comment.
    pub threads: HashMap<i64, ThreadState>,
    /// Threads this account resolved itself (a fixed finding), never a human's dismissal.
    pub resolved_by_us: HashSet<String>,
    /// The pull request merged: a finding still open then was passed over.
    pub merged: bool,
}

/// The fates the current observation settles. A finding with nothing new about it yields none. A finding never posted
/// as a review comment (listed only in the review body: over the comment budget, or off the diff) is not rejected by
/// a merge: nobody could react to it (fisher-ki-saas#2945: 199 of 200 findings "merged with the finding open").
pub fn fates_of(input: &FateInput) -> Vec<Fate> {
    let mut out = Vec::new();
    for finding in &input.findings {
        let status = input.statuses.get(&finding.id).unwrap_or(&finding.status).as_str();
        let thread = finding.comment_id.and_then(|id| input.threads.get(&id));
        let (up, down) = thread.map_or((0, 0), |thread| (thread.thumbs_up, thread.thumbs_down));
        let resolved_by_human =
            thread.is_some_and(|thread| thread.resolved && !input.resolved_by_us.contains(&thread.id));
        let decided = if down > 0 && up == 0 {
            Some((FateKind::Rejected, "thumbs down on the comment"))
        } else if up > 0 && down == 0 {
            Some((FateKind::Accepted, "thumbs up on the comment"))
        } else if status == "fixed" {
            Some((FateKind::Accepted, "fixed in a later commit"))
        // A nit is optional: waving it off is not a rejection the repository's rules should learn from.
        } else if resolved_by_human && status != "not_applicable" && finding.severity != "nit" {
            Some((FateKind::Rejected, "the thread was resolved without a fix"))
        } else if input.merged
            && finding.comment_id.is_some()
            && finding.severity != "nit"
            && (status == "open" || status == "still_present")
        {
            Some((FateKind::Rejected, MERGED_OPEN))
        } else {
            None
        };
        if let Some((fate, how)) = decided {
            out.push(Fate {
                id: finding.id.clone(),
                pull: input.pull.clone(),
                file: finding.file.clone(),
                line: finding.line,
                category: finding.category.clone().unwrap_or_else(|| "unknown".to_string()),
                kind: finding.kind.clone().unwrap_or_else(|| "unknown".to_string()),
                level: finding.severity.clone(),
                claim: finding.claim.clone(),
                fate,
                how: how.to_string(),
                at: input.now.clone(),
            });
        }
    }
    out
}

/// How a merge with a posted finding still open is recorded.
pub const MERGED_OPEN: &str = "merged with the finding open";

/// The ledger without the merge rejections of findings that were never posted as review comments (`unposted`:
/// `(pull, id)`): fates recorded before [`fates_of`] stopped making them. Rust only; the TypeScript ledger has none.
pub fn without_unposted_merges(ledger: &[Fate], unposted: &HashSet<(String, String)>) -> Vec<Fate> {
    ledger
        .iter()
        .filter(|fate| fate.how != MERGED_OPEN || !unposted.contains(&(fate.pull.clone(), fate.id.clone())))
        .cloned()
        .collect()
}

/// The ledger with these fates added; a finding's later fate replaces its earlier one, bounded per repository.
pub fn merge_fates(ledger: &[Fate], fates: &[Fate]) -> Vec<Fate> {
    let mut by_key: IndexMap<String, Fate> = IndexMap::new();
    for fate in ledger.iter().chain(fates) {
        by_key.insert(format!("{}:{}", fate.pull, fate.id), fate.clone());
    }
    let mut merged: Vec<Fate> = by_key.into_values().collect();
    merged.sort_by(|a, b| jsrt::collate::locale_compare(&a.at, &b.at));
    let excess = merged.len().saturating_sub(MAX_FATES_PER_REPO);
    merged.split_off(excess)
}

/// The rules a repository's recent fates support.
pub fn lessons_for(ledger: &[Fate], now: &str) -> Vec<Lesson> {
    // JS: Date.parse(now) - window; an unparsable `now` (NaN) admits nothing.
    let since = jsrt::date::parse_date(now).map(|now| now - FATE_DAYS * DAY_MS);
    let mut groups: IndexMap<(String, String), (u32, u32)> = IndexMap::new();
    for fate in ledger {
        let Some(at) = jsrt::date::parse_date(&fate.at) else {
            continue;
        };
        if since.is_none_or(|since| at < since) {
            continue;
        }
        let group = groups.entry((fate.category.clone(), fate.kind.clone())).or_default();
        match fate.fate {
            FateKind::Accepted => group.0 += 1,
            FateKind::Rejected => group.1 += 1,
        }
    }
    let mut lessons: Vec<Lesson> = groups
        .into_iter()
        .filter(|(_, (accepted, rejected))| accepted + rejected >= MIN_SAMPLES)
        .map(|((category, kind), (accepted, rejected))| Lesson {
            category,
            kind,
            accepted,
            rejected,
            lowered: f64::from(accepted) / f64::from(accepted + rejected) < LOWER_BELOW,
        })
        .collect();
    lessons.sort_by(|a, b| {
        jsrt::collate::locale_compare(&a.category, &b.category)
            .then_with(|| jsrt::collate::locale_compare(&a.kind, &b.kind))
    });
    lessons
}
