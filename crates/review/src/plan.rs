//! From an engine result to what is posted (`autoreview/plan.ts`, levels from `types.ts`): the verdict, a few inline
//! comments, and the body. Pure functions; nothing here talks to GitHub.
//!
//! - Verdict: REQUEST_CHANGES when a confirmed finding is at or above `blockAt`, or an earlier one is still present
//!   at that level; APPROVE only when coverage was complete; otherwise COMMENT. The account's own pull request, and
//!   a closed or merged one, always get COMMENT. On a re-review, a new finding on lines that did not change since
//!   the last review blocks only when critical; one at `blockAt` or above makes it COMMENT, not APPROVE.
//! - Few, short comments, ranked by level and evidence strength; a nit takes a slot only when one is left.
//! - An inline comment sits on its line or the nearest diff line within three; else the body names it.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::settings::SIGNATURE;

pub const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
pub const DEFAULT_BLOCK_AT: &str = "medium";
pub const DEFAULT_MAX_COMMENTS: i64 = crate::settings::DEFAULT_MAX_COMMENTS as i64;
pub const RELOCATE_WITHIN: i64 = 3;
/// An inline comment's sentences (before any code block) aim to stay under this.
const MAX_BLOCKING_LISTED: usize = 8;
const MAX_FINDINGS_LISTED: usize = 12;
const MAX_NOT_CHECKED: usize = 3;
const MAX_BODY_LINES: usize = 80;
/// `notChecked` lines that account for internal pass skipping/bookkeeping: kept in the record, not posted.
const PIPELINE_ACCOUNTING: [&str; 8] = [
    "Not traced by the deep pass",
    "Checks the deep pass",
    "AI and LLM integration reviewer skipped",
    "Correctness reviewer skipped",
    "Architecture and maintainability reviewer skipped",
    "Tests and QA reviewer skipped",
    "Conventions: exceeded",
    "Conventions: CLAUDE.md changed",
];
const MAX_SOURCES_LISTED: usize = 6;
const MAX_MISSING_LISTED: usize = 6;
const STRENGTHS: [&str; 3] = ["test", "outside", "diff"];

/// A finding's level: its `level`, or its old-scale `severity` (blocker, major, minor) mapped to one; else `low`.
pub fn level_of(level: Option<&str>, severity: Option<&str>) -> &'static str {
    for name in [level, severity].into_iter().flatten() {
        if let Some(level) = LEVELS.iter().find(|item| **item == name) {
            return level;
        }
        match name {
            "blocker" => return "critical",
            "major" => return "high",
            "minor" => return "low",
            _ => {}
        }
    }
    "low"
}

/// A level on the old scale; medium has no equal there and is written as `minor`.
pub fn severity_of(level: &str) -> &'static str {
    match level {
        "critical" => "blocker",
        "high" => "major",
        "nit" => "nit",
        _ => "minor",
    }
}

fn rank(level: &str) -> usize {
    LEVELS.iter().position(|item| *item == level).unwrap_or(LEVELS.len())
}

fn at_or_above(level: &str, threshold: &str) -> bool {
    rank(level) <= rank(threshold)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub file: String,
    pub line: i64,
}

/// The fields of an engine finding the plan reads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineFinding {
    pub file: String,
    pub line: i64,
    pub end_line: Option<i64>,
    pub level: Option<String>,
    pub severity: Option<String>,
    /// `test`, `outside` or `diff` (absent: `diff`).
    pub strength: Option<String>,
    pub also_at: Option<Vec<Place>>,
    pub scenario: Option<String>,
    #[serde(default)]
    pub category: String,
    pub claim: String,
    #[serde(default)]
    pub why: String,
    pub suggested_fix: Option<String>,
    /// The exact new text of lines `line..endLine`, when the fix is a drop-in replacement.
    pub replacement: Option<String>,
    pub source: Option<String>,
    pub lesson: Option<String>,
    /// The verifier's sentence on why the finding has this level and not a neighbouring one.
    pub level_reason: Option<String>,
    /// `confirmed` or `uncertain`.
    pub verification: String,
}

impl EngineFinding {
    pub fn level(&self) -> &'static str {
        level_of(self.level.as_deref(), self.severity.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EarlierStatus {
    pub file: String,
    pub line: i64,
    pub level: Option<String>,
    pub severity: Option<String>,
    /// `fixed`, `still_present`, `not_applicable` or `unknown`.
    pub status: String,
    #[serde(default)]
    pub claim: String,
    /// What the re-check saw (the quoted line).
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AlsoRaised {
    pub by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Dropped {
    #[serde(default)]
    pub rejected: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    pub total_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub cost_usd: f64,
    pub frames: f64,
    pub tokens: f64,
}

/// The fields of an engine result the plan reads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineResult {
    pub complete: bool,
    pub files: i64,
    pub added: i64,
    pub removed: i64,
    pub findings: Vec<EngineFinding>,
    /// Real problems the verifier found in code the change did not alter (Rust-only): named in the body, never
    /// inline, never blocking.
    #[serde(default)]
    pub preexisting: Vec<EngineFinding>,
    #[serde(default)]
    pub also_raised: Vec<AlsoRaised>,
    #[serde(default)]
    pub earlier: Vec<EarlierStatus>,
    #[serde(default)]
    pub dropped: Dropped,
    pub timing: Timing,
    pub usage: Usage,
    pub assurance: Option<Vec<String>>,
    #[serde(default)]
    pub not_checked: Vec<String>,
    #[serde(default)]
    pub incomplete: Vec<String>,
    /// New-file line ranges of the diff's hunks, by path.
    #[serde(default)]
    pub diff_lines: IndexMap<String, Vec<(i64, i64)>>,
}

impl EngineResult {
    /// The plan's view of an engine's raw result; an unreadable result is an error, never an empty review.
    pub fn parse(raw: &serde_json::Value) -> Result<EngineResult, String> {
        EngineResult::deserialize(raw).map_err(|error| format!("the engine returned an unreadable result: {error}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanOptions {
    /// The reviewing account wrote the pull request.
    pub self_authored: bool,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub head_sha: String,
    /// A re-review of the changes since this commit.
    pub since_sha: Option<String>,
    pub signature: bool,
    pub block_at: Option<String>,
    #[serde(default)]
    pub never_request_changes: bool,
    pub max_comments: Option<i64>,
    /// A cherry-pick or promotion: only what differs from the merged pull requests it carries was read.
    #[serde(default)]
    pub promotion: Option<PromotionNote>,
    /// A re-review: the new-file line ranges, by path, that changed since this account's last review (empty when
    /// the head did not move). `None` (a first review, a force-push) puts every line in play.
    #[serde(default)]
    pub fresh_lines: Option<IndexMap<String, Vec<(i64, i64)>>>,
}

/// What a cherry-pick or promotion carries (`promotion.rs`): its sources and what matched them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionNote {
    pub sources: Vec<String>,
    pub carried_lines: u64,
    pub carried_files: u64,
    #[serde(default)]
    pub context_files: u64,
    pub missing: Vec<crate::promotion::Missing>,
    /// Every changed line was carried: nothing was left to read.
    #[serde(default)]
    pub entire: bool,
}

fn counted_noun(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// `a`, `a and b`, `a, b and c`; past `max`, `a, b, ... and N more`.
fn listed(items: &[String], max: usize) -> String {
    let shown: Vec<&str> = items.iter().take(max).map(String::as_str).collect();
    let rest = items.len().saturating_sub(max);
    match (shown.as_slice(), rest) {
        ([], _) => String::new(),
        ([one], 0) => (*one).to_string(),
        (_, 0) => format!("{} and {}", shown[..shown.len() - 1].join(", "), shown[shown.len() - 1]),
        (_, rest) => format!("{} and {rest} more", shown.join(", ")),
    }
}

/// The lead of the body for a cherry-pick: what it carries and that the rest was read, or that there was no rest.
fn promotion_lead(note: &PromotionNote) -> String {
    let names = listed(&note.sources, MAX_SOURCES_LISTED);
    let matches = if note.carried_lines == 1 { "matches" } else { "match" };
    let them = if note.sources.len() == 1 { "it" } else { "them" };
    if note.entire {
        let what = if note.carried_lines == 1 {
            "its one changed line matches".to_string()
        } else {
            format!("all {} changed lines match", note.carried_lines)
        };
        let was = if note.carried_lines == 1 { "was" } else { "were" };
        return format!(
            "Cherry-pick of {names}: {what} {them} line for line and {was} reviewed there; nothing else changed."
        );
    }
    let files = if note.carried_files > 0 {
        format!(" ({} entirely)", counted_noun(note.carried_files, "file"))
    } else {
        String::new()
    };
    let context = if note.context_files > 0 {
        format!(
            ", and in full {} where this branch's code differs from what {} reviewed against",
            counted_noun(note.context_files, "file"),
            if note.sources.len() == 1 { "it was" } else { "they were" }
        )
    } else {
        String::new()
    };
    format!(
        "Cherry-pick of {names}: {} {matches} {them} line for line and {} reviewed there{files}; this review reads the \
         rest{context}.",
        counted_noun(note.carried_lines, "changed line"),
        if note.carried_lines == 1 { "was" } else { "were" }
    )
}

/// The lines the sources add that the head does not have, for the author to confirm.
fn promotion_missing(note: &PromotionNote) -> Option<String> {
    if note.missing.is_empty() {
        return None;
    }
    let named: Vec<String> = note
        .missing
        .iter()
        .take(MAX_MISSING_LISTED)
        .map(|missing| format!("`{}` ({})", missing.path, counted_noun(missing.lines, "line")))
        .collect();
    let more = note.missing.len().saturating_sub(MAX_MISSING_LISTED);
    let more = if more > 0 {
        format!(", and {} more", counted_noun(more as u64, "file"))
    } else {
        String::new()
    };
    let verb = if note.sources.len() == 1 { "adds" } else { "add" };
    Some(format!(
        "Not carried over: lines {} {verb} are not in this head, in {}{more}. Worth confirming they are left out on purpose.",
        listed(&note.sources, MAX_SOURCES_LISTED),
        named.join(", ")
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    /// The verdict as the state and the outcome name it (its serialized form).
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request_changes",
            Verdict::Comment => "comment",
        }
    }

    pub fn event(self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Comment => "COMMENT",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Verdict::Approve => "Approve",
            Verdict::RequestChanges => "Request changes",
            Verdict::Comment => "Comment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Placed {
    pub line: i64,
    pub relocated: bool,
    pub range: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedComment {
    /// Index into the result's findings.
    pub finding: usize,
    /// The marker in the body that identifies the comment when the review is read back.
    pub marker: String,
    pub relocated: bool,
    pub suggestion: bool,
    pub path: String,
    pub line: i64,
    pub side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<&'static str>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPlan {
    pub verdict: Verdict,
    pub event: &'static str,
    pub body: String,
    pub comments: Vec<PlannedComment>,
    /// Confirmed findings named in the body instead of inline, by index.
    pub in_summary: Vec<usize>,
    /// Confirmed findings neither inline nor named: past the comment limit, or nits. By index.
    pub over_cap: Vec<usize>,
}

/// Where an inline comment for `line` can go: the line itself, a diff line within three lines, or nowhere.
pub fn place_line(ranges: Option<&[(i64, i64)]>, line: i64) -> Option<Placed> {
    let mut best: Option<(i64, i64, (i64, i64))> = None;
    for &(start, end) in ranges.unwrap_or_default() {
        let nearest = line.max(start).min(end);
        let distance = (nearest - line).abs();
        if best.is_none_or(|(_, best_distance, _)| distance < best_distance) {
            best = Some((nearest, distance, (start, end)));
        }
    }
    let (line, distance, range) = best?;
    (distance <= RELOCATE_WITHIN).then_some(Placed {
        line,
        relocated: distance > 0,
        range,
    })
}

fn fence(code: &str, info: &str) -> String {
    let mut longest = 2;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let ticks = "`".repeat(longest + 1);
    format!("{ticks}{info}\n{code}\n{ticks}")
}

pub fn comment_marker(key: &str) -> String {
    format!("<!-- ultron-autoreview:{key} -->")
}

/// `text` ending as a sentence.
fn sentence(text: &str) -> String {
    let flat = jsrt::string::flatten(text);
    if flat.is_empty() || flat.ends_with(['.', '!', '?', '…', ':']) {
        flat
    } else {
        format!("{flat}.")
    }
}

fn place(file: &str, line: i64) -> String {
    format!("`{file}:{line}`")
}

fn where_(finding: &EngineFinding) -> String {
    place(&finding.file, finding.line)
}

fn non_empty(text: &Option<String>) -> Option<&str> {
    text.as_deref().filter(|text| !text.is_empty())
}

/// `[level] what is wrong, with the evidence. The fix.`: the sentences of an inline comment.
pub fn comment_text(finding: &EngineFinding) -> String {
    let head = format!("[{}] {}", finding.level(), sentence(&finding.claim));
    let fix = match (&finding.replacement, non_empty(&finding.suggested_fix)) {
        (None, Some(fix)) => sentence(fix),
        _ => String::new(),
    };
    let evidence = match &finding.scenario {
        Some(scenario) if !jsrt::string::trim(scenario).is_empty() => scenario.as_str(),
        _ => finding.why.as_str(),
    };
    let middle = sentence(evidence);
    let rated = non_empty(&finding.level_reason)
        .map_or_default(|reason| format!("Rated {}: {}", finding.level(), sentence(reason)));
    [head, middle, rated, fix, certainty(finding)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// How sure the review is, when it is more than the verifier's reading: a run the host made, or a fact read from the
/// files' structure; and when the repository's history lowered it.
pub fn certainty(finding: &EngineFinding) -> String {
    let mut parts = Vec::new();
    if finding.strength.as_deref() == Some("test") {
        parts.push("Proven by a test run the reviewer made.".to_string());
    } else if finding.source.as_deref() == Some("deep:structure") {
        parts
            .push("Stated by the reviewer from the files' structure; the verifier agreed it matters here.".to_string());
    }
    if let Some(lesson) = non_empty(&finding.lesson) {
        parts.push(format!("Posted as a note: this repository {lesson}."));
    }
    parts.join(" ")
}

/// The inline comment of one finding, or none when its line cannot be placed on the diff.
pub fn plan_comment(
    finding: &EngineFinding,
    index: usize,
    ranges: Option<&[(i64, i64)]>,
    key: &str,
) -> Option<PlannedComment> {
    let placed = place_line(ranges, finding.line)?;
    let end = finding
        .end_line
        .filter(|end| *end > finding.line)
        .unwrap_or(finding.line);
    let span_in_hunk = !placed.relocated && end >= placed.range.0 && end <= placed.range.1;
    let multi = span_in_hunk && end > finding.line;
    let suggestion = finding.replacement.is_some() && span_in_hunk;
    let mut parts = vec![comment_text(finding)];
    if placed.relocated {
        parts.push(format!(
            "This is about line {}, which is not part of the diff.",
            finding.line
        ));
    }
    if let Some(also) = finding.also_at.as_ref().filter(|also| !also.is_empty()) {
        let named: Vec<String> = also.iter().take(6).map(|item| place(&item.file, item.line)).collect();
        parts.push(format!("Same at {}.", named.join(", ")));
    }
    if let Some(replacement) = &finding.replacement {
        if suggestion {
            parts.push(fence(replacement, "suggestion"));
        } else {
            let lines = if end > finding.line {
                format!("lines {}-{end}", finding.line)
            } else {
                format!("line {}", finding.line)
            };
            parts.push(format!(
                "Suggested replacement for {lines}:\n\n{}",
                fence(replacement, "")
            ));
        }
    }
    let marker = comment_marker(key);
    parts.push(marker.clone());
    Some(PlannedComment {
        finding: index,
        marker,
        relocated: placed.relocated,
        suggestion,
        path: finding.file.clone(),
        line: if multi { end } else { placed.line },
        side: "RIGHT",
        start_line: multi.then_some(finding.line),
        start_side: multi.then_some("RIGHT"),
        body: parts.join("\n\n"),
    })
}

fn confirmed(finding: &EngineFinding) -> bool {
    finding.verification == "confirmed"
}

pub type FreshLines = IndexMap<String, Vec<(i64, i64)>>;

/// Whether a finding (or another place of its root cause) is on lines that changed since the last review, or within
/// [`RELOCATE_WITHIN`] lines of them; always, when `fresh` is `None`.
pub fn is_fresh(finding: &EngineFinding, fresh: Option<&FreshLines>) -> bool {
    let Some(fresh) = fresh else { return true };
    let changed = |file: &str, line: i64| place_line(fresh.get(file).map(Vec::as_slice), line).is_some();
    changed(&finding.file, finding.line)
        || finding
            .also_at
            .iter()
            .flatten()
            .any(|place| changed(&place.file, place.line))
}

/// A finding at `block_at` or above blocks, unless a re-review found it on code the last review already read: a new
/// finding there moves the goalposts, so it blocks only when critical (boost-capital/aqua-parser#90 and #91 had 22
/// change requests, most for medium findings in lines no commit had touched since the review before).
fn blocks(finding: &EngineFinding, block_at: &str, fresh: Option<&FreshLines>) -> bool {
    at_or_above(finding.level(), block_at) && (finding.level() == "critical" || is_fresh(finding, fresh))
}

/// The verdict of a first review of an open pull request the reviewer did not author.
pub fn decide_verdict(result: &EngineResult, block_at: Option<&str>) -> Decision {
    decide_verdict_in(result, false, "open", block_at, None)
}

/// [`decide_verdict`] of a re-review: findings outside `fresh` block only when critical.
pub fn decide_verdict_in(
    result: &EngineResult,
    self_authored: bool,
    state: &str,
    block_at: Option<&str>,
    fresh: Option<&FreshLines>,
) -> Decision {
    let block_at = block_at.unwrap_or(DEFAULT_BLOCK_AT);
    let blocking = result
        .findings
        .iter()
        .filter(|finding| confirmed(finding) && blocks(finding, block_at, fresh))
        .count();
    let late = result
        .findings
        .iter()
        .filter(|finding| confirmed(finding) && at_or_above(finding.level(), block_at))
        .count()
        - blocking;
    let still_open = result
        .earlier
        .iter()
        .filter(|item| {
            item.status == "still_present"
                && at_or_above(level_of(item.level.as_deref(), item.severity.as_deref()), block_at)
        })
        .count();
    let decision = |verdict, reason: String| Decision { verdict, reason };
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    if self_authored {
        decision(Verdict::Comment, "this account opened the pull request".into())
    } else if state != "open" {
        decision(Verdict::Comment, format!("the pull request is {state}"))
    } else if blocking > 0 {
        decision(
            Verdict::RequestChanges,
            format!(
                "{blocking} confirmed finding{} at {block_at} or above",
                plural(blocking)
            ),
        )
    } else if still_open > 0 {
        decision(
            Verdict::RequestChanges,
            format!(
                "{still_open} finding{} from an earlier review still present",
                plural(still_open)
            ),
        )
    } else if !result.complete {
        decision(Verdict::Comment, "the review did not cover everything".into())
    } else if late > 0 {
        decision(
            Verdict::Comment,
            "nothing that changed since the last review blocks the merge".into(),
        )
    } else {
        decision(Verdict::Approve, format!("no confirmed finding at {block_at} or above"))
    }
}

/// Confirmed findings, the ones to post first: those that are serious (critical, high) or proven beyond the diff,
/// then the rest; within each by level, then by evidence strength. Indices into `result.findings`.
pub fn rank_findings(result: &EngineResult) -> Vec<usize> {
    let strength = |finding: &EngineFinding| {
        let name = finding.strength.as_deref().unwrap_or("diff");
        STRENGTHS
            .iter()
            .position(|item| *item == name)
            .map_or(-1, |position| position as i64)
    };
    let tier = |finding: &EngineFinding| {
        let level = finding.level();
        i32::from(
            !(level != "nit"
                && (at_or_above(level, "high") || finding.strength.as_deref().unwrap_or("diff") != "diff")),
        )
    };
    let mut ranked: Vec<usize> = (0..result.findings.len())
        .filter(|index| confirmed(&result.findings[*index]))
        .collect();
    ranked.sort_by_key(|index| {
        let finding = &result.findings[*index];
        (tier(finding), rank(finding.level()), strength(finding), *index)
    });
    ranked
}

fn counted(indices: &[usize], result: &EngineResult) -> String {
    LEVELS
        .iter()
        .filter_map(|level| {
            let count = indices
                .iter()
                .filter(|index| result.findings[**index].level() == *level)
                .count();
            (count > 0).then(|| format!("{count} {level}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map_or_default(|first| format!("{}{}", first.to_uppercase(), chars.as_str()))
}

pub fn plan_review(result: &EngineResult, options: &PlanOptions) -> ReviewPlan {
    let block_at = options.block_at.as_deref().unwrap_or(DEFAULT_BLOCK_AT);
    let max_comments = options.max_comments.unwrap_or(DEFAULT_MAX_COMMENTS).max(0) as usize;
    let fresh = options.fresh_lines.as_ref();
    let Decision {
        verdict: raw_verdict,
        reason,
    } = decide_verdict_in(
        result,
        options.self_authored,
        &options.state,
        options.block_at.as_deref(),
        fresh,
    );
    let verdict = if options.never_request_changes && raw_verdict == Verdict::RequestChanges {
        Verdict::Comment
    } else {
        raw_verdict
    };
    let mut comments: Vec<PlannedComment> = Vec::new();
    let mut in_summary = Vec::new();
    let mut over_cap = Vec::new();
    let short_sha = jsrt::utf16::head(&options.head_sha, 7);
    let ranked = rank_findings(result);
    let ranges = |file: &str| result.diff_lines.get(file).map(Vec::as_slice);
    for &index in &ranked {
        let finding = &result.findings[index];
        let level = finding.level();
        let blocking = blocks(finding, block_at, fresh);
        if verdict == Verdict::RequestChanges && level == "nit" {
            over_cap.push(index);
            continue;
        }
        let key = format!("{short_sha}-{}", index + 1);
        let mut comment = plan_comment(finding, index, ranges(&finding.file), &key);
        if comment.is_none() {
            let also = finding.also_at.clone().unwrap_or_default();
            for (position, other) in also.iter().enumerate() {
                let mut also_at = vec![Place {
                    file: finding.file.clone(),
                    line: finding.line,
                }];
                also_at.extend(
                    also.iter()
                        .enumerate()
                        .filter(|(i, _)| *i != position)
                        .map(|(_, item)| item.clone()),
                );
                let moved = EngineFinding {
                    file: other.file.clone(),
                    line: other.line,
                    end_line: None,
                    replacement: None,
                    also_at: Some(also_at),
                    ..finding.clone()
                };
                comment = plan_comment(&moved, index, ranges(&other.file), &key);
                if comment.is_some() {
                    break;
                }
            }
        }
        match comment {
            Some(comment) if comments.len() < max_comments => comments.push(comment),
            ref other if blocking || (other.is_none() && level != "nit") => in_summary.push(index),
            _ => over_cap.push(index),
        }
    }
    let inline: Vec<usize> = comments.iter().map(|comment| comment.finding).collect();
    let blockers: Vec<usize> = ranked
        .iter()
        .copied()
        .filter(|index| blocks(&result.findings[*index], block_at, fresh))
        .collect();
    let late = ranked
        .iter()
        .filter(|index| {
            let finding = &result.findings[**index];
            at_or_above(finding.level(), block_at) && !blocks(finding, block_at, fresh)
        })
        .count();
    let uncertain = result
        .findings
        .iter()
        .filter(|finding| finding.verification == "uncertain")
        .count();
    let since = options.since_sha.as_deref().map(|sha| jsrt::utf16::head(sha, 7));

    let scope = format!(
        "{} file{}, +{} / -{}",
        result.files,
        if result.files == 1 { "" } else { "s" },
        result.added,
        result.removed
    );
    let rev_span = match since {
        None => format!("`{short_sha}`"),
        Some(sha) => format!("`{sha}`..`{short_sha}`"),
    };
    let promotion = options.promotion.as_ref().filter(|note| !note.sources.is_empty());

    let mut sections: Vec<String> = Vec::new();
    let mut header_lines = vec![format!("**{}** — {rev_span} ({scope})", verdict.title())];
    if let Some(note) = promotion {
        header_lines.push(promotion_lead(note));
    }
    if blockers.is_empty() {
        header_lines.push(format!("{}.", capitalized(&reason)));
    } else if verdict == Verdict::RequestChanges {
        header_lines.push(format!(
            "{} blocking finding{} to resolve before merge.",
            blockers.len(),
            if blockers.len() == 1 { "" } else { "s" }
        ));
    } else {
        header_lines.push(format!("{}.", capitalized(&reason)));
    }
    sections.push(header_lines.join("\n"));

    let mut listed_findings: Vec<String> = Vec::new();
    let mut seen_indices: Vec<usize> = Vec::new();
    for &index in &blockers {
        if listed_findings.len() >= MAX_FINDINGS_LISTED {
            break;
        }
        let finding = &result.findings[index];
        let tag = if inline.contains(&index) { " (inline)" } else { "" };
        listed_findings.push(format!(
            "{}. **[{}]** {}{} — {}",
            listed_findings.len() + 1,
            capitalized(finding.level()),
            where_(finding),
            tag,
            sentence(&finding.claim)
        ));
        seen_indices.push(index);
    }
    for comment in &comments {
        if seen_indices.contains(&comment.finding) || listed_findings.len() >= MAX_FINDINGS_LISTED {
            continue;
        }
        let finding = &result.findings[comment.finding];
        listed_findings.push(format!(
            "{}. **[{}]** {} (inline) — {}",
            listed_findings.len() + 1,
            capitalized(finding.level()),
            where_(finding),
            sentence(&finding.claim)
        ));
        seen_indices.push(comment.finding);
    }
    for &index in &in_summary {
        if seen_indices.contains(&index) || listed_findings.len() >= MAX_FINDINGS_LISTED {
            continue;
        }
        let finding = &result.findings[index];
        listed_findings.push(format!(
            "{}. **[{}]** {} (outside diff) — {}",
            listed_findings.len() + 1,
            capitalized(finding.level()),
            where_(finding),
            sentence(&finding.claim)
        ));
        seen_indices.push(index);
    }
    for finding in &result.preexisting {
        if listed_findings.len() >= MAX_FINDINGS_LISTED {
            break;
        }
        listed_findings.push(format!(
            "{}. **[{}]** {} (pre-existing) — {}",
            listed_findings.len() + 1,
            capitalized(finding.level()),
            where_(finding),
            sentence(&finding.claim)
        ));
    }
    let total_listed_pool = blockers.len()
        + comments.iter().filter(|c| !blockers.contains(&c.finding)).count()
        + in_summary.iter().filter(|i| !blockers.contains(i)).count()
        + result.preexisting.len();
    if !listed_findings.is_empty() {
        let mut block = vec!["## Findings".to_string()];
        block.extend(listed_findings);
        if total_listed_pool > MAX_FINDINGS_LISTED {
            block.push(format!(
                "- *…and {} more finding(s).*",
                total_listed_pool - MAX_FINDINGS_LISTED
            ));
        }
        sections.push(block.join("\n"));
    }

    if !result.earlier.is_empty() {
        let mut prev = vec!["## Previous findings".to_string()];
        for item in result.earlier.iter().take(MAX_BLOCKING_LISTED) {
            let status_label = match item.status.as_str() {
                "fixed" => "fixed",
                "still_present" => "still present",
                "not_applicable" => "not applicable",
                _ => "not re-checked",
            };
            let detail = if item.status == "still_present" && !item.evidence.is_empty() {
                format!(" — {} ({})", sentence(&item.claim), sentence(&item.evidence))
            } else if !item.claim.is_empty() {
                format!(" — {}", sentence(&item.claim))
            } else {
                String::new()
            };
            prev.push(format!(
                "- {} **[{status_label}]**{detail}",
                place(&item.file, item.line)
            ));
        }
        if result.earlier.len() > MAX_BLOCKING_LISTED {
            prev.push(format!(
                "- *…and {} more from earlier reviews.*",
                result.earlier.len() - MAX_BLOCKING_LISTED
            ));
        }
        sections.push(prev.join("\n"));
    }

    let assurance: Vec<String> = result
        .assurance
        .iter()
        .flatten()
        .take(4)
        .map(|item| jsrt::string::flatten(item))
        .collect();
    if !assurance.is_empty() {
        let mut checked = vec!["## Checked".to_string()];
        for item in &assurance {
            checked.push(format!("- {item}"));
        }
        sections.push(checked.join("\n"));
    }

    let mut notes = Vec::new();
    if late > 0 {
        let (plural, verb, pronoun) = if late == 1 {
            ("", "is", "it does")
        } else {
            ("s", "are", "they do")
        };
        notes.push(format!(
            "{late} finding{plural} at {block_at} or above {verb} on unchanged lines, \
             so {pronoun} not block."
        ));
    }
    if !over_cap.is_empty() {
        notes.push(format!(
            "Not posted: {} lower-ranked finding{} ({}).",
            over_cap.len(),
            if over_cap.len() == 1 { "" } else { "s" },
            counted(&over_cap, result)
        ));
    }
    let lowered = result
        .findings
        .iter()
        .filter(|finding| non_empty(&finding.lesson).is_some())
        .count();
    if lowered > 0 {
        notes.push(format!(
            "{lowered} finding{} notes because this repository rarely accepted their kind.",
            if lowered == 1 { " is" } else { "s are" }
        ));
    }
    let raised = result.also_raised.len();
    if raised > 0 {
        let mut by: Vec<&str> = Vec::new();
        for name in result.also_raised.iter().flat_map(|item| &item.by) {
            if !by.contains(&name.as_str()) {
                by.push(name);
            }
        }
        let by: Vec<String> = by.iter().take(4).map(|name| format!("@{name}")).collect();
        notes.push(format!(
            "{raised} finding{} already raised by {} and {} not repeated.",
            if raised == 1 { " was" } else { "s were" },
            by.join(", "),
            if raised == 1 { "is" } else { "are" }
        ));
    }
    let _ = uncertain;
    if !notes.is_empty() {
        sections.push(notes.join(" "));
    }
    if let Some(missing) = promotion.and_then(promotion_missing) {
        sections.push(missing);
    }

    let mut gaps: Vec<&String> = result.incomplete.iter().collect();
    for item in &result.not_checked {
        let is_accounting = PIPELINE_ACCOUNTING.iter().any(|lead| item.starts_with(lead));
        if !result.incomplete.contains(item) && !is_accounting {
            gaps.push(item);
        }
    }
    if !gaps.is_empty() {
        let listed: Vec<String> = gaps
            .iter()
            .take(MAX_NOT_CHECKED)
            .map(|gap| {
                let gap = jsrt::string::flatten(gap);
                gap.strip_suffix('.').map(str::to_string).unwrap_or(gap)
            })
            .collect();
        let more = if gaps.len() > MAX_NOT_CHECKED {
            format!("; and {} more", gaps.len() - MAX_NOT_CHECKED)
        } else {
            String::new()
        };
        sections.push(format!("Not checked: {}{more}.", listed.join("; ")));
    }

    let mut lines: Vec<String> = Vec::new();
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        lines.extend(section.lines().map(str::to_string));
    }
    let tail: Vec<String> = if options.signature {
        vec![String::new(), SIGNATURE.into()]
    } else {
        Vec::new()
    };
    lines.truncate(MAX_BODY_LINES - tail.len());
    lines.extend(tail);
    ReviewPlan {
        verdict,
        event: verdict.event(),
        body: lines.join("\n"),
        comments,
        in_summary,
        over_cap,
    }
}

/// The plan with every inline comment named in the body instead (when GitHub rejects the inline comments).
pub fn without_inline(plan: &ReviewPlan, result: &EngineResult) -> ReviewPlan {
    if plan.comments.is_empty() {
        return plan.clone();
    }
    let named: Vec<String> = plan
        .comments
        .iter()
        .map(|comment| {
            let finding = &result.findings[comment.finding];
            format!("{} {}", where_(finding), comment_text(finding))
        })
        .collect();
    let extra = format!("These could not be attached to the diff: {}", named.join(" "));
    let lines: Vec<&str> = plan.body.split('\n').collect();
    let signed = lines.last() == Some(&SIGNATURE);
    let head = if signed {
        &lines[..lines.len().saturating_sub(2)]
    } else {
        &lines[..]
    };
    let mut body: Vec<&str> = head.to_vec();
    body.extend(["", &extra]);
    if signed {
        body.extend(["", SIGNATURE]);
    }
    let mut in_summary = plan.in_summary.clone();
    in_summary.extend(plan.comments.iter().map(|comment| comment.finding));
    ReviewPlan {
        body: body.join("\n"),
        comments: Vec::new(),
        in_summary,
        ..plan.clone()
    }
}
