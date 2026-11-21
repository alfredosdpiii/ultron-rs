//! Automatic instruction refinements (Rust-only): the session learns from its own compaction summaries and session
//! tails, with evidence it cannot invent, and persists durable project-wide refinements in memorantado.
//!
//! - Propose: after a completed compaction, after an idle stretch, or on session close, one side request with the
//!   source text, the active refinements (session + project) and the recent ledger history asks for at most
//!   [`MAX_PROPOSALS`] small instructions. [`plan`] keeps only proposals whose `quote` is a verbatim substring of the
//!   source (no invented failures); each kept one is proposed and activated in the session's refinements ledger (kind
//!   `instruction`, target `instruction:main.<slug>` or `instruction:main.r<n>`, branch-anchored, superseding any
//!   active refinement on the same target, checked by the ledger's validator). Proposals marked `durable` are also
//!   persisted as keyed project claims in memorantado (when configured), where `memory.dream` checks their file quotes
//!   and contradictions across sessions. Listed rollbacks of active refinements (session or `project:<id>`) are rolled
//!   back, and `used` project refinements are re-confirmed in memorantado. Outcomes are logged to
//!   `<agent_dir>/memory/auto-refine.log`.
//! - Deliver: before a root run of a user message, when the active `instruction:main.*` refinements (session + always-on
//!   project, ordered newest / most-recently-confirmed first, capped at [`MESSAGE_LIMIT`]) differ from what the context
//!   shows since its last compaction, the run opens with one `ultron.refinements` custom message listing them (on the
//!   branch, like recalled memory, so the prompt cache holds). Situational project refinements with `pathGlobs` stay
//!   out of the always-on list and surface via `auto_memory` recall instead. The summarizer never sees
//!   `ultron.refinements` (compaction `convert_for_summary`).
//! - Both run only while the `refine.auto` setting is on (default on) and `ULTRON_AUTO_REFINE` is not `off` (tests
//!   that count requests turn it off); off, the session runs the unmodified harness.

use kernel::Signal;
use kernel_apis::MemoryService;
use kernel_apis::memory::PROJECT_REFINEMENT_PREFIX;
use serde_json::{Value, json};
use services::{Branch, BranchSource, LocalServices};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use protocol::custom_types::REFINEMENTS;
/// Targets of main-lane refinements: `instruction:main.<slug>` (the ledger keeps one active record per target).
const TARGET_PREFIX: &str = "instruction:main.";
/// The least time between two background proposal passes.
pub const COOLDOWN: Duration = Duration::from_secs(20 * 60);
/// The most proposals one pass keeps.
pub const MAX_PROPOSALS: usize = 3;
/// The longest refinement text, in bytes.
pub const MAX_TEXT: usize = 1024;
/// The shortest quote accepted as evidence: a word or two of the source proves nothing.
const MIN_QUOTE_CHARS: usize = 16;
/// The most always-on refinements one message lists (newest / most-recently-confirmed first).
pub const MESSAGE_LIMIT: usize = 8;
/// Ledger history the proposal request shows.
const HISTORY_LIMIT: usize = 10;

const SYSTEM: &str = "You review a coding agent's compaction summary or session tail and propose small, durable \
instructions the agent should follow. Answer with JSON only.";

/// The session's refinements ledger, the branch its changes follow, and the optional memorantado memory service for
/// project-scoped persistence (compose builds it).
#[derive(Clone)]
pub struct Ledger {
    pub services: Arc<LocalServices>,
    pub branch: Option<BranchSource>,
    pub memory: Option<Arc<MemoryService>>,
}

impl Ledger {
    fn branch(&self) -> Option<Branch> {
        self.branch.as_ref().map(|source| source())
    }

    /// Every main-lane instruction record on the current session branch, oldest first.
    fn session_records(&self, branch: Option<&Branch>) -> Vec<Value> {
        let listed = self.services.handle("refinements.list", &json!({}), branch);
        let records = listed
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        records
            .into_iter()
            .filter(|record| record["kind"] == "instruction")
            .filter(|record| record["target"].as_str().is_some_and(|t| t.starts_with(TARGET_PREFIX)))
            .collect()
    }

    /// Active always-on main-lane refinements across session (branch-anchored, newest first) and project memorantado
    /// (most-recently-confirmed first, excluding situational `pathGlobs` and stale claims, shadowed by session
    /// overrides or rollbacks): `{id, version, text, scope, target}`.
    pub fn active(&self) -> Vec<Value> {
        let branch = self.branch();
        let records = self.session_records(branch.as_ref());
        let mut out = active_newest_first(&records);
        let shadowed: HashSet<String> = records
            .iter()
            .filter(|r| matches!(r["state"].as_str(), Some("active" | "rolled_back")))
            .filter_map(|r| r["target"].as_str().map(str::to_string))
            .collect();
        if let Some(memory) = &self.memory {
            for project in services::safe_project_refinements(memory) {
                let Some(target) = project["target"].as_str() else {
                    continue;
                };
                if project["kind"] != "instruction"
                    || !target.starts_with(TARGET_PREFIX)
                    || shadowed.contains(target)
                    || project["pathGlobs"].as_array().is_some_and(|g| !g.is_empty())
                    || project["stale"].is_string()
                {
                    continue;
                }
                let mut view = services::refinement_view(&project);
                view["scope"] = "project".into();
                view["target"] = target.into();
                out.push(view);
            }
        }
        out
    }

    /// Active `skill:*` refinements from session + memorantado, re-validated on read: `(target, skill_md_text)`.
    pub fn active_skills(&self) -> Vec<(String, String)> {
        let branch = self.branch();
        let listed = self
            .services
            .handle("refinements.list", &json!({}), branch.as_ref())
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let mut out = Vec::new();
        let mut shadowed = HashSet::new();
        for record in listed.iter().rev() {
            if record["kind"] != "skill" {
                continue;
            }
            let Some(target) = record["target"].as_str() else {
                continue;
            };
            if matches!(record["state"].as_str(), Some("active" | "rolled_back")) {
                shadowed.insert(target.to_string());
            }
            if record["state"] == "active"
                && let Some(text) = record["content"].as_str()
                && services::check_refinement_safety("skill", target, &record["content"]).is_ok()
            {
                out.push((target.to_string(), text.to_string()));
            }
        }
        if let Some(memory) = &self.memory {
            for project in services::safe_project_refinements(memory) {
                if project["kind"] != "skill" || project["stale"].is_string() {
                    continue;
                }
                let Some(target) = project["target"].as_str() else {
                    continue;
                };
                if shadowed.contains(target) {
                    continue;
                }
                if let Some(text) = project["content"].as_str() {
                    out.push((target.to_string(), text.to_string()));
                }
            }
        }
        out
    }
}

fn active_newest_first(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .rev()
        .filter(|record| record["state"] == "active")
        .map(|record| {
            let mut view = services::refinement_view(record);
            view["scope"] = record["scope"].clone();
            view["target"] = record["target"].clone();
            view
        })
        .collect()
}

/// Whether automatic refinement is on for a session (`refine.auto`, default on), when it last proposed, and which
/// source entry it last covered.
pub struct AutoRefine {
    pub on: bool,
    last_pass: Mutex<Option<Instant>>,
    last_through: Mutex<Option<String>>,
}

impl AutoRefine {
    /// From the merged `settings` (`refine.auto`) and `ULTRON_AUTO_REFINE` (`off`, `0`, `false`: off).
    pub fn new(settings: &Value, env: Option<&str>) -> AutoRefine {
        let env_off = crate::flag_off(env);
        AutoRefine {
            on: !env_off && settings["refine"]["auto"].as_bool().unwrap_or(true),
            last_pass: Mutex::new(None),
            last_through: Mutex::new(None),
        }
    }

    /// Whether a proposal pass may start now (on, and past the cooldown); starting one restarts the cooldown.
    pub fn start_pass(&self) -> bool {
        let mut last = self.last_pass.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.on || last.is_some_and(|at| at.elapsed() < COOLDOWN) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// Whether a pass for `through` may start now: skips if off, already refined up to `through`, or within
    /// [`COOLDOWN`] unless `bypass_cooldown` is true (used on session close).
    pub fn start_pass_for(&self, through: &str, bypass_cooldown: bool) -> bool {
        if !self.on {
            return false;
        }
        let mut seen = self.last_through.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.as_deref() == Some(through) {
            return false;
        }
        let mut last = self.last_pass.lock().unwrap_or_else(PoisonError::into_inner);
        if !bypass_cooldown && last.is_some_and(|at| at.elapsed() < COOLDOWN) {
            return false;
        }
        *seen = Some(through.to_string());
        *last = Some(Instant::now());
        true
    }
}

/// The refinement ids the context shows since its last compaction (`None`: no refinements message since).
pub fn shown(entries: &[Value]) -> Option<Vec<String>> {
    let details = *session_journal::context::custom_details_since_compaction(entries, REFINEMENTS).last()?;
    let ids = details["ids"].as_array().into_iter().flatten();
    Some(ids.filter_map(|id| id.as_str().map(str::to_string)).collect())
}

/// The message that tells the context which refinements are active, when that differs from what it `shown`.
pub fn message(active: &[Value], shown: Option<&[String]>, timestamp: u64) -> Option<Value> {
    let active = &active[..active.len().min(MESSAGE_LIMIT)];
    let ids: Vec<String> = active
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    if ids.as_slice() == shown.unwrap_or_default() {
        return None;
    }
    let mut content = if active.is_empty() {
        "[refinements] No instruction refinements are active now; disregard the ones listed earlier.".to_string()
    } else {
        "[refinements] Instructions learned from this session and project, each backed by a verbatim quote. Follow \
         them unless the user says otherwise; `refinements.rollback` with an id retires a wrong one."
            .to_string()
    };
    for refinement in active {
        let text = refinement["text"].as_str().unwrap_or_default();
        let cut = text.char_indices().take_while(|(at, _)| *at < MAX_TEXT).map(|(_, c)| c);
        let scope = refinement["scope"].as_str().filter(|s| *s == "project");
        match scope {
            Some("project") => content.push_str(&format!(
                "\n- [{} project] {}",
                refinement["id"].as_str().unwrap_or_default(),
                cut.collect::<String>()
            )),
            _ => content.push_str(&format!(
                "\n- [{} v{}] {}",
                refinement["id"].as_str().unwrap_or_default(),
                refinement["version"],
                cut.collect::<String>()
            )),
        }
    }
    Some(json!({
        "role": "custom",
        "customType": REFINEMENTS,
        "content": content,
        "display": true,
        "details": {"ids": ids},
        "timestamp": timestamp,
    }))
}

/// One proposed refinement, its quote already found verbatim in the source.
#[derive(Debug, PartialEq)]
pub struct Proposal {
    pub text: String,
    pub quote: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub key: Option<String>,
    pub situation: Option<String>,
    pub path_globs: Vec<String>,
    pub durable: bool,
}

/// What a proposal reply asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    pub rollback: Vec<String>,
    pub used: Vec<String>,
    pub dropped: usize,
}

/// Sanitizes an optional semantic key into a valid kebab-case slug (`[a-z0-9]+(?:-[a-z0-9]+)*`).
fn sanitize_slug(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if matches!(ch, '-' | '_' | ' ' | '.') {
            dash = true;
        }
    }
    (!out.is_empty() && out.len() <= 48).then_some(out)
}

/// The plan in a proposal `reply` (`{"proposals": [{text, quote, rationale, expectedOutcome, key?, situation?,
/// pathGlobs?, durable?}], "rollback": [ids], "used": [ids]}`, fenced or not): at most [`MAX_PROPOSALS`], each with
/// text of at most [`MAX_TEXT`] bytes and a quote that is a verbatim substring of `source`; the rest are dropped.
pub fn plan(reply: &str, source: &str) -> Plan {
    let json = match (reply.find('{'), reply.rfind('}')) {
        (Some(start), Some(end)) if start < end => &reply[start..=end],
        _ => return Plan::default(),
    };
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return Plan::default();
    };
    let field = |item: &Value, key: &str| {
        item[key]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let raw_proposals = value["proposals"].as_array().into_iter().flatten();
    let mut kept = Vec::new();
    let mut dropped = 0;
    for item in raw_proposals.take(MAX_PROPOSALS) {
        let Some(text) = field(item, "text") else {
            dropped += 1;
            continue;
        };
        let Some(quote) = item["quote"].as_str().map(str::to_string) else {
            dropped += 1;
            continue;
        };
        let Some(rationale) = field(item, "rationale") else {
            dropped += 1;
            continue;
        };
        let Some(expected_outcome) = field(item, "expectedOutcome") else {
            dropped += 1;
            continue;
        };
        if text.len() > MAX_TEXT || quote.trim().chars().count() < MIN_QUOTE_CHARS || !source.contains(&quote) {
            dropped += 1;
            continue;
        }
        let path_globs = item["pathGlobs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        kept.push(Proposal {
            text,
            quote,
            rationale,
            expected_outcome,
            key: field(item, "key").and_then(|k| sanitize_slug(&k)),
            situation: field(item, "situation"),
            path_globs,
            durable: item["durable"].as_bool().unwrap_or(false),
        });
    }
    let string_list = |key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect()
    };
    Plan {
        proposals: kept,
        rollback: string_list("rollback"),
        used: string_list("used"),
        dropped,
    }
}

fn prompt(source: &str, active: &[Value], records: &[Value]) -> String {
    let active_json = serde_json::to_string_pretty(active).unwrap_or_default();
    let history: Vec<Value> = records[records.len().saturating_sub(HISTORY_LIMIT)..]
        .iter()
        .map(|r| {
            let view = services::refinement_view(r);
            json!({"id": view["id"], "target": r["target"], "state": r["state"], "text": view["text"]})
        })
        .collect();
    let history = serde_json::to_string_pretty(&history).unwrap_or_default();
    format!(
        "<summary>\n{source}\n</summary>\n\n<active-refinements>\n{active_json}\n</active-refinements>\n\n\
         <refinement-history>\n{history}\n</refinement-history>\n\n\
         Propose at most {MAX_PROPOSALS} short instructions (each under {MAX_TEXT} bytes) the agent should follow. \
         Prefer none over speculative ones: only repeated failures, user corrections or reusable tactics the source \
         shows. Each needs `quote`, an excerpt copied character for character from the source that shows why; a \
         proposal whose quote is not in the source is discarded. Give each a short kebab-case `key` (e.g. \
         `test-runner`) so revisions update the same instruction instead of duplicating it; set `durable: true` only \
         for repo-wide conventions that should persist into future sessions (with optional `situation` and `pathGlobs` \
         when it only applies to specific files). Do not repeat an active refinement. List in `rollback` the ids of \
         active refinements the source shows to be wrong or harmful, and in `used` the ids of active refinements the \
         agent relied on.\n\n\
         Answer with JSON only: {{\"proposals\": [{{\"key\": \"...\", \"text\": \"...\", \"quote\": \"...\", \
         \"rationale\": \"...\", \"expectedOutcome\": \"...\", \"durable\": false, \"situation\": \"...\", \
         \"pathGlobs\": []}}], \"rollback\": [], \"used\": []}}"
    )
}

/// Summary of one automatic refinement pass, for `<agent_dir>/memory/auto-refine.log`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    pub source_ref: String,
    pub proposed: usize,
    pub promoted: usize,
    pub rolled_back: usize,
    pub confirmed: usize,
    pub dropped: usize,
    pub error: Option<String>,
}

/// Appends one concise line for `outcome` to `<agent_dir>/memory/auto-refine.log`.
pub fn log_outcome(agent_dir: &Path, outcome: &Outcome) {
    let dir = agent_dir.join("memory");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("auto-refine.log"))
    else {
        return;
    };
    let at = jsrt::date::iso_string(jsrt::date::now_ms());
    let detail = match &outcome.error {
        Some(err) => format!("error={err}"),
        None => format!(
            "proposed={} promoted={} rolled_back={} confirmed={} dropped={}",
            outcome.proposed, outcome.promoted, outcome.rolled_back, outcome.confirmed, outcome.dropped
        ),
    };
    let _ = writeln!(file, "{at} source={} {detail}", outcome.source_ref);
}

/// Runs one proposal pass synchronously over `source_text` (backed by `source_ref`, e.g. `compaction:<id>` or
/// `tail:<id>`). Logs the outcome when `agent_dir` is provided.
pub fn propose_sync(
    ledger: &Ledger,
    source_ref: &str,
    source_text: &str,
    agent_dir: Option<&Path>,
    ask: impl FnOnce(&str, &str) -> Result<String, String>,
) -> Outcome {
    let records = ledger.session_records(ledger.branch().as_ref());
    let active = ledger.active();
    let reply = match ask(SYSTEM, &prompt(source_text, &active, &records)) {
        Ok(reply) => reply,
        Err(err) => {
            let outcome = Outcome {
                source_ref: source_ref.to_string(),
                error: Some(err),
                ..Outcome::default()
            };
            if let Some(dir) = agent_dir {
                log_outcome(dir, &outcome);
            }
            return outcome;
        }
    };
    let planned = plan(&reply, source_text);
    let outcome = apply(ledger, &planned, source_ref);
    if let Some(dir) = agent_dir {
        log_outcome(dir, &outcome);
    }
    outcome
}

/// One proposal pass over `source_ref`'s `source_text`, in the background: `ask(system, prompt)` is the side request;
/// its plan is applied to `ledger` and logged to `<agent_dir>/memory/auto-refine.log`.
pub fn propose(
    ledger: Ledger,
    source_ref: String,
    source_text: String,
    agent_dir: Option<PathBuf>,
    ask: impl FnOnce(&str, &str) -> Result<String, String> + Send + 'static,
) {
    std::thread::spawn(move || {
        propose_sync(&ledger, &source_ref, &source_text, agent_dir.as_deref(), ask);
    });
}

/// Next unused `r<n>` suffix across `records`, avoiding collisions even when targets were proposed manually or on
/// other numbering schemes.
fn next_fallback_index(records: &[Value]) -> usize {
    records
        .iter()
        .filter_map(|r| r["target"].as_str()?.strip_prefix(TARGET_PREFIX)?.strip_prefix('r'))
        .filter_map(|n| n.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

/// Rolls back the plan's active rollbacks, confirms `used` project refinements, then proposes and activates its
/// proposals (superseding an existing active target when the key matches, and promoting durable ones to memorantado).
fn apply(ledger: &Ledger, plan: &Plan, source_ref: &str) -> Outcome {
    let mut outcome = Outcome {
        source_ref: source_ref.to_string(),
        dropped: plan.dropped,
        ..Outcome::default()
    };
    let branch = ledger.branch();
    let signal = Signal::new();
    // 1. Confirm project refinements the model relied on.
    if let Some(memory) = &ledger.memory {
        let project_used: Vec<String> = plan
            .used
            .iter()
            .filter(|id| id.starts_with(PROJECT_REFINEMENT_PREFIX))
            .cloned()
            .collect();
        if !project_used.is_empty() {
            memory.confirm_project_refinements(&project_used, &signal);
            outcome.confirmed = project_used.len();
        }
    }
    // 2. Roll back requested session or project refinements.
    let records = ledger.session_records(branch.as_ref());
    let current = active_newest_first(&records);
    for id in &plan.rollback {
        if id.starts_with(PROJECT_REFINEMENT_PREFIX) {
            if let Some(memory) = &ledger.memory
                && memory
                    .retire_project_refinement(id, "auto_refine rollback", &signal)
                    .is_ok()
            {
                outcome.rolled_back += 1;
            }
        } else if current.iter().any(|r| r["id"] == id.as_str())
            && ledger
                .services
                .handle("refinements.rollback", &json!({"id": id}), branch.as_ref())
                .is_ok()
        {
            outcome.rolled_back += 1;
        }
    }
    // Refresh after rollbacks so baseVersion lookups reflect the current state.
    let mut records = ledger.session_records(branch.as_ref());
    let mut next_r = next_fallback_index(&records);
    let can_promote = ledger.memory.as_ref().is_some_and(|m| m.supports_project_refinements());
    for proposal in &plan.proposals {
        let target = match &proposal.key {
            Some(slug) => {
                let candidate = format!("{TARGET_PREFIX}{slug}");
                if services::check_refinement_safety("instruction", &candidate, &json!(proposal.text)).is_ok() {
                    candidate
                } else {
                    let fallback = format!("{TARGET_PREFIX}r{next_r}");
                    next_r += 1;
                    fallback
                }
            }
            None => {
                let fallback = format!("{TARGET_PREFIX}r{next_r}");
                next_r += 1;
                fallback
            }
        };
        let existing_active = records
            .iter()
            .find(|r| r["state"] == "active" && r["target"] == target.as_str());
        if existing_active.is_some_and(|r| r["content"] == proposal.text.as_str()) {
            continue;
        }
        // Also avoid exact-text duplication across different targets.
        if records
            .iter()
            .any(|r| r["state"] == "active" && r["content"] == proposal.text.as_str())
        {
            continue;
        }
        let base_version = existing_active.and_then(|r| r["version"].as_i64()).unwrap_or(0);
        let scope = if proposal.durable && can_promote {
            "project"
        } else {
            "session"
        };
        let evidence = json!([{
            "ref": source_ref,
            "quote": proposal.quote,
            "rationale": proposal.rationale,
            "expectedOutcome": proposal.expected_outcome,
        }]);
        let request = json!({
            "kind": "instruction",
            "target": target,
            "baseVersion": base_version,
            "content": proposal.text,
            "evidence": evidence,
            "scope": scope,
        });
        if let Ok(record) = ledger.services.handle("refinements.propose", &request, branch.as_ref())
            && let Ok(activated) =
                ledger
                    .services
                    .handle("refinements.activate", &json!({"id": record["id"]}), branch.as_ref())
        {
            outcome.proposed += 1;
            records = ledger.session_records(branch.as_ref());
            if proposal.durable
                && let Some(memory) = &ledger.memory
                && memory.supports_project_refinements()
                && services::check_refinement_safety("instruction", &target, &activated["content"]).is_ok()
            {
                let mut project_record = json!({
                    "kind": "instruction",
                    "target": target,
                    "text": proposal.text,
                    "evidence": evidence,
                    "source": "auto",
                });
                if let Some(situation) = &proposal.situation {
                    project_record["situation"] = situation.clone().into();
                }
                if !proposal.path_globs.is_empty() {
                    project_record["pathGlobs"] = json!(proposal.path_globs);
                }
                if memory.save_project_refinement(&project_record, &signal).is_ok() {
                    outcome.promoted += 1;
                }
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str =
        "## Progress\nThe user corrected the agent twice: run tests with scripts/nextest.sh, not cargo test.";

    fn reply(proposals: Value) -> String {
        json!({"proposals": proposals, "rollback": ["r-old"], "used": ["project:42"]}).to_string()
    }

    fn proposal(text: &str, quote: &str) -> Value {
        json!({
            "key": "test-runner",
            "text": text,
            "quote": quote,
            "rationale": "user corrected it",
            "expectedOutcome": "no reruns",
            "durable": true,
            "situation": "running tests",
            "pathGlobs": ["crates/**/*.rs"],
        })
    }

    #[test]
    fn auto_refine_plan_keeps_proposals_quoted_from_the_summary() {
        let fenced = format!(
            "```json\n{}\n```",
            reply(json!([proposal(
                "Run tests with scripts/nextest.sh.",
                "run tests with scripts/nextest.sh"
            )]))
        );
        assert_eq!(
            plan(&fenced, SUMMARY),
            Plan {
                proposals: vec![Proposal {
                    text: "Run tests with scripts/nextest.sh.".into(),
                    quote: "run tests with scripts/nextest.sh".into(),
                    rationale: "user corrected it".into(),
                    expected_outcome: "no reruns".into(),
                    key: Some("test-runner".into()),
                    situation: Some("running tests".into()),
                    path_globs: vec!["crates/**/*.rs".into()],
                    durable: true,
                }],
                rollback: vec!["r-old".into()],
                used: vec!["project:42".into()],
                dropped: 0,
            }
        );
    }

    #[test]
    fn auto_refine_plan_drops_invented_quotes_oversized_text_and_extras() {
        let quote = "run tests with scripts/nextest.sh";
        let kept = plan(
            &reply(json!([
                proposal("Invented.", "the agent deleted the database twice"),
                proposal("Paraphrased.", "run the tests with scripts/nextest.sh"),
                proposal("Too short a quote.", "nextest"),
                proposal(&"x".repeat(MAX_TEXT + 1), quote),
                proposal("Fourth.", quote),
            ])),
            SUMMARY,
        );
        assert_eq!(
            kept.proposals,
            Vec::new(),
            "only the first three are read, and none of them holds"
        );
        assert_eq!(kept.dropped, 3);
        let three = plan(
            &reply(json!([
                proposal("a", quote),
                proposal("b", quote),
                proposal("c", quote),
                proposal("d", quote)
            ])),
            SUMMARY,
        );
        assert_eq!(
            three.proposals.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn auto_refine_plan_of_a_malformed_reply_is_empty() {
        for reply in ["", "no json here", "{\"proposals\": [", "[1, 2]", "{\"proposals\": 3}"] {
            assert_eq!(plan(reply, SUMMARY), Plan::default(), "{reply}");
        }
    }

    #[test]
    fn auto_refine_message_follows_what_the_context_shows() {
        let active = [
            json!({"id": "r1", "version": 1, "text": "Use nextest.", "scope": "session"}),
            json!({"id": "project:9", "version": 1, "text": "Check clippy.", "scope": "project"}),
        ];
        let message = message(&active, None, 7).unwrap();
        let text = message["content"].as_str().unwrap();
        assert!(text.contains("[r1 v1] Use nextest."));
        assert!(text.contains("[project:9 project] Check clippy."));
        let entries = [json!({"type": "message", "message": message})];
        let shown = shown(&entries);
        assert_eq!(shown.as_deref(), Some(&["r1".to_string(), "project:9".to_string()][..]));
        assert!(super::message(&active, shown.as_deref(), 8).is_none(), "already shown");
        let none = super::message(&[], shown.as_deref(), 9).unwrap();
        assert!(
            none["content"]
                .as_str()
                .unwrap()
                .contains("No instruction refinements are active")
        );
        assert!(super::message(&[], None, 9).is_none(), "nothing to say");
        let compacted = [entries[0].clone(), json!({"type": "compaction"})];
        assert_eq!(super::shown(&compacted), None, "a compaction shows them again");
    }

    #[test]
    fn auto_refine_follows_the_setting_and_cools_down() {
        assert!(!AutoRefine::new(&json!({"refine": {"auto": false}}), None).start_pass());
        assert!(!AutoRefine::new(&json!({}), Some("off")).on);
        let auto = AutoRefine::new(&json!({}), Some("1"));
        assert!(auto.on);
        assert!(auto.start_pass());
        assert!(!auto.start_pass(), "one pass per cooldown");
        assert!(auto.start_pass_for("entry-1", true), "bypass_cooldown works on close");
        assert!(
            !auto.start_pass_for("entry-1", true),
            "same entry is never refined twice"
        );
        assert!(
            auto.start_pass_for("entry-2", true),
            "new entry can run when bypassing cooldown"
        );
    }
}
