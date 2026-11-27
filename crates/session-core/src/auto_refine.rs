//! Automatic instruction refinements (Rust-only): the session learns from its own compaction summaries, with
//! evidence it cannot invent, and persists reusable refinements across sessions at `project` and `global` scopes.
//!
//! - Propose: after a completed compaction, in the background, one side request with the new summary, the active
//!   refinements and the recent ledger history asks for at most [`MAX_PROPOSALS`] small refinements. [`plan`] keeps
//!   only proposals whose `quote` is a verbatim substring of the summary (no invented failures); `session`-scoped ones
//!   are proposed and activated in the session's refinements ledger, while `project` and `global` ones are persisted
//!   atomically under `<agentDir>/refinements/` with full version history and inverse rollback support.
//! - Deliver: before a root run of a user message, active `session + project + global` refinements are ranked against
//!   the current prompt (3x weight) and recent turns (2x..1x decay) using IDF-discounted multi-field scoring over
//!   `id`, `tags`, `title`, and `text`. When the top refinements differ from what the context shows since its last
//!   compaction, the run opens with one `ultron.refinements` custom message listing them.
//! - Both run only while the `refine.auto` setting is on (default on) and `ULTRON_AUTO_REFINE` is not `off`.

use serde_json::{Value, json};
use services::{Branch, BranchSource, LocalServices};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::custom_types::REFINEMENTS;

/// Targets of main-lane refinements: `instruction:main.r<n>` (the ledger keeps one active record per target).
const TARGET_PREFIX: &str = "instruction:main.";
/// The least time between two proposal passes.
pub const COOLDOWN: Duration = Duration::from_mins(20);
/// The most proposals one pass keeps.
pub const MAX_PROPOSALS: usize = 3;
/// The longest refinement text, in bytes.
pub const MAX_TEXT: usize = 1024;
/// The shortest quote accepted as evidence: a word or two of the summary proves nothing.
const MIN_QUOTE_CHARS: usize = 16;
/// The most refinements one message lists.
pub const MESSAGE_LIMIT: usize = 12;
/// Ledger history the proposal request shows.
const HISTORY_LIMIT: usize = 10;
/// Maximum tags kept on a single refinement.
const MAX_TAGS: usize = 8;

const SYSTEM: &str = "You review a coding agent's compaction summary and propose small, durable instructions the agent \
should follow. Answer with JSON only.";

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "have", "were", "been", "into", "your", "when", "what",
    "where", "which", "while", "about", "after", "before", "should", "would", "could", "there", "their", "these",
    "those", "only", "also", "just", "than", "then", "them", "they", "will", "not", "use",
];

/// Persistence scope for an automatic refinement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Session,
    Project,
    Global,
}

impl Scope {
    pub fn parse(raw: Option<&str>) -> Scope {
        match raw.map(str::trim) {
            Some("project") => Scope::Project,
            Some("global") => Scope::Global,
            _ => Scope::Session,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Session => "session",
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

/// Target kind for a refinement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Instruction,
    Skill,
    Subagent,
    Memory,
}

impl Kind {
    pub fn parse(raw: Option<&str>) -> Kind {
        match raw.map(str::trim) {
            Some("skill") => Kind::Skill,
            Some("subagent") => Kind::Subagent,
            Some("memory") => Kind::Memory,
            _ => Kind::Instruction,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Instruction => "instruction",
            Kind::Skill => "skill",
            Kind::Subagent => "subagent",
            Kind::Memory => "memory",
        }
    }
}

/// Persistent cross-session refinement store under `<agentDir>/refinements/`.
#[derive(Clone)]
pub struct ScopedStore {
    project_path: PathBuf,
    global_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl ScopedStore {
    pub fn new(agent_dir: PathBuf, cwd: PathBuf) -> ScopedStore {
        let base = agent_dir.join("refinements");
        let hash = jsrt::hash::sha256_hex(cwd.to_string_lossy().as_bytes());
        let short = &hash[..16];
        ScopedStore {
            project_path: base.join("projects").join(format!("{short}.json")),
            global_path: base.join("global.json"),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn project_path(&self) -> &Path {
        &self.project_path
    }

    pub fn global_path(&self) -> &Path {
        &self.global_path
    }

    fn path_for(&self, scope: Scope) -> Option<&Path> {
        match scope {
            Scope::Project => Some(&self.project_path),
            Scope::Global => Some(&self.global_path),
            Scope::Session => None,
        }
    }

    fn read_records(path: &Path) -> Vec<Value> {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            return Vec::new();
        };
        doc["records"].as_array().cloned().unwrap_or_default()
    }

    fn write_records(path: &Path, records: &[Value]) -> Result<(), String> {
        let body = serde_json::to_string_pretty(&json!({"version": 1, "records": records}))
            .map_err(|e| format!("serialize refinements: {e}"))?;
        jsrt::fs::atomic_write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Every record across project and global stores (project first, then global).
    pub fn all(&self) -> Vec<Value> {
        let _guard = jsrt::sync::lock(&self.lock);
        let mut records = Self::read_records(&self.project_path);
        records.extend(Self::read_records(&self.global_path));
        records
    }

    /// Active project and global refinements formatted for delivery.
    pub fn active(&self) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|record| record["state"] == "active")
            .map(scoped_view)
            .collect()
    }

    /// Applies project/global proposals and rollbacks atomically under lock.
    pub fn apply(&self, plan: &Plan, compaction_id: &str, now_ms: u64) {
        let _guard = jsrt::sync::lock(&self.lock);
        for scope in [Scope::Project, Scope::Global] {
            let Some(path) = self.path_for(scope) else {
                continue;
            };
            let mut records = Self::read_records(path);
            let mut changed = false;
            for id in &plan.rollback {
                if rollback_in_vec(&mut records, id, now_ms) {
                    changed = true;
                }
            }
            let prefix = match scope {
                Scope::Project => "proj-r",
                Scope::Global => "glob-r",
                Scope::Session => continue,
            };
            for proposal in plan.proposals.iter().filter(|p| p.scope == scope) {
                let duplicate = records
                    .iter()
                    .any(|r| r["state"] == "active" && r["text"] == proposal.text.as_str());
                if duplicate {
                    continue;
                }
                let next = records.len() + 1;
                let id = format!("{prefix}{next}");
                records.push(json!({
                    "id": id,
                    "scope": scope.as_str(),
                    "kind": proposal.kind.as_str(),
                    "version": 1,
                    "state": "active",
                    "title": proposal.title,
                    "text": proposal.text,
                    "tags": proposal.tags,
                    "evidence": {
                        "ref": format!("compaction:{compaction_id}"),
                        "quote": proposal.quote,
                        "rationale": proposal.rationale,
                        "expectedOutcome": proposal.expected_outcome,
                    },
                    "history": [{
                        "action": "activated",
                        "at": now_ms,
                        "version": 1,
                        "text": proposal.text,
                    }],
                }));
                changed = true;
            }
            if changed {
                let _ = Self::write_records(path, &records);
            }
        }
    }

    /// Rolls back an active record by id in `scope`, recording an inverse history entry.
    pub fn rollback(&self, scope: Scope, id: &str, now_ms: u64) -> bool {
        let Some(path) = self.path_for(scope) else {
            return false;
        };
        let _guard = jsrt::sync::lock(&self.lock);
        let mut records = Self::read_records(path);
        if rollback_in_vec(&mut records, id, now_ms) {
            return Self::write_records(path, &records).is_ok();
        }
        false
    }

    /// Reverses the last rollback on `id` in `scope`, restoring it to `active`.
    pub fn restore_rollback(&self, scope: Scope, id: &str, now_ms: u64) -> bool {
        let Some(path) = self.path_for(scope) else {
            return false;
        };
        let _guard = jsrt::sync::lock(&self.lock);
        let mut records = Self::read_records(path);
        if transition_record(&mut records, id, "rolled_back", "active", "restored", "text", now_ms) {
            return Self::write_records(path, &records).is_ok();
        }
        false
    }
}

fn rollback_in_vec(records: &mut [Value], id: &str, now_ms: u64) -> bool {
    transition_record(
        records,
        id,
        "active",
        "rolled_back",
        "rolled_back",
        "previousText",
        now_ms,
    )
}

fn transition_record(
    records: &mut [Value],
    id: &str,
    from_state: &str,
    to_state: &str,
    action: &str,
    text_key: &str,
    now_ms: u64,
) -> bool {
    let Some(record) = records.iter_mut().find(|r| r["id"] == id && r["state"] == from_state) else {
        return false;
    };
    let text = record["text"].as_str().unwrap_or_default().to_string();
    let version = record["version"].as_u64().unwrap_or(1) + 1;
    record["state"] = json!(to_state);
    record["version"] = json!(version);
    if let Some(history) = record["history"].as_array_mut() {
        history.push(json!({
            "action": action,
            "at": now_ms,
            "version": version,
            text_key: text,
        }));
    }
    true
}

fn scoped_view(record: Value) -> Value {
    let mut view = json!({
        "id": record["id"],
        "version": record["version"],
        "text": record["text"],
        "scope": record["scope"],
        "kind": record["kind"],
    });
    if let Some(title) = record["title"].as_str().filter(|s| !s.is_empty()) {
        view["title"] = json!(title);
    }
    if let Some(tags) = record["tags"].as_array().filter(|t| !t.is_empty()) {
        view["tags"] = Value::Array(tags.clone());
    }
    view
}

/// The session's refinements ledger, optional cross-session store, and the branch its changes follow.
#[derive(Clone)]
pub struct Ledger {
    pub services: Arc<LocalServices>,
    pub branch: Option<BranchSource>,
    pub store: Option<ScopedStore>,
}

impl Ledger {
    fn branch(&self) -> Option<Branch> {
        self.branch.as_ref().map(|source| source())
    }

    /// Every main-lane instruction record on the branch, oldest first.
    fn records(&self, branch: Option<&Branch>) -> Vec<Value> {
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

    /// The active refinements across session, project, and global scopes.
    pub fn active(&self) -> Vec<Value> {
        let mut combined = active(&self.records(self.branch().as_ref()));
        if let Some(store) = &self.store {
            combined.extend(store.active());
        }
        combined
    }

    /// Active refinements ranked for the current `goal` prompt and up to 4 `recent_turns` (newest first).
    pub fn active_ranked(&self, goal: &str, recent_turns: &[String]) -> Vec<Value> {
        let combined = self.active();
        rank_refinements(&combined, goal, recent_turns, MESSAGE_LIMIT)
    }
}

fn active(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .filter(|record| record["state"] == "active")
        .map(services::refinement_view)
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() >= 3 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

struct DocFields {
    ids: HashSet<String>,
    tags: HashSet<String>,
    titles: HashSet<String>,
    texts: HashSet<String>,
}

fn build_query_weights(goal: &str, recent_turns: &[String]) -> HashMap<String, f64> {
    let mut query_weights: HashMap<String, f64> = HashMap::new();
    for term in tokenize(goal) {
        let entry = query_weights.entry(term).or_insert(0.0);
        *entry = entry.max(3.0);
    }
    for (idx, turn) in recent_turns.iter().take(4).enumerate() {
        let weight = 2.0 - 0.25 * f64::from(idx as u32);
        for term in tokenize(turn) {
            let entry = query_weights.entry(term).or_insert(0.0);
            *entry = entry.max(weight);
        }
    }
    query_weights
}

fn extract_doc_fields(candidate: &Value) -> DocFields {
    DocFields {
        ids: tokenize(candidate["id"].as_str().unwrap_or_default())
            .into_iter()
            .collect(),
        titles: tokenize(candidate["title"].as_str().unwrap_or_default())
            .into_iter()
            .collect(),
        texts: tokenize(candidate["text"].as_str().unwrap_or_default())
            .into_iter()
            .collect(),
        tags: candidate["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .flat_map(tokenize)
            .collect(),
    }
}

fn compute_doc_freq(doc_fields: &[DocFields]) -> HashMap<String, usize> {
    let mut df: HashMap<String, usize> = HashMap::new();
    for fields in doc_fields {
        let mut seen: HashSet<&str> = HashSet::new();
        for term in fields
            .ids
            .iter()
            .chain(fields.tags.iter())
            .chain(fields.titles.iter())
            .chain(fields.texts.iter())
        {
            if seen.insert(term.as_str()) {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }
    }
    df
}

fn score_refinement(
    candidate: &Value,
    fields: &DocFields,
    query_weights: &HashMap<String, f64>,
    df: &HashMap<String, usize>,
    n: f64,
) -> f64 {
    let mut score = match candidate["scope"].as_str() {
        Some("project") => 0.03,
        Some("global") => 0.01,
        _ => 0.05,
    };
    for (term, q_weight) in query_weights {
        let doc_freq = df.get(term).copied().unwrap_or(0) as f64;
        let idf = ((n + 1.0) / (doc_freq + 1.0)).ln() + 1.0;
        let field_weight = 2.5 * f64::from(fields.ids.contains(term))
            + 2.0 * f64::from(fields.tags.contains(term))
            + 1.5 * f64::from(fields.titles.contains(term))
            + f64::from(fields.texts.contains(term));
        score += field_weight * q_weight * idf;
    }
    score
}

/// Goal-weighted, IDF-discounted multi-field ranking over `id`, `tags`, `title`, and `text`.
///
/// The current prompt (`goal`) carries 3x weight; up to 4 recent turns decay linearly from 2.0 to 1.25.
/// When no query terms exist, returns candidates in their original order up to `limit`.
pub fn rank_refinements(candidates: &[Value], goal: &str, recent_turns: &[String], limit: usize) -> Vec<Value> {
    if candidates.len() <= 1 || limit == 0 {
        return candidates.iter().take(limit).cloned().collect();
    }
    let query_weights = build_query_weights(goal, recent_turns);
    if query_weights.is_empty() {
        return candidates.iter().take(limit).cloned().collect();
    }
    let doc_fields: Vec<DocFields> = candidates.iter().map(extract_doc_fields).collect();
    let df = compute_doc_freq(&doc_fields);
    let n = candidates.len() as f64;
    let mut scored: Vec<(usize, f64)> = doc_fields
        .iter()
        .enumerate()
        .map(|(index, fields)| {
            (
                index,
                score_refinement(&candidates[index], fields, &query_weights, &df, n),
            )
        })
        .collect();
    scored.sort_by(|(idx_a, score_a), (idx_b, score_b)| score_b.total_cmp(score_a).then_with(|| idx_a.cmp(idx_b)));
    scored
        .into_iter()
        .take(limit)
        .map(|(index, _)| candidates[index].clone())
        .collect()
}

/// Whether automatic refinement is on for a session (`refine.auto`, default on), and when it last proposed.
pub struct AutoRefine {
    pub on: bool,
    last_pass: Mutex<Option<Instant>>,
}

impl AutoRefine {
    /// From the merged `settings` (`refine.auto`) and `ULTRON_AUTO_REFINE` (`off`, `0`, `false`: off).
    pub fn new(settings: &Value, env: Option<&str>) -> AutoRefine {
        let env_off = crate::flag_off(env);
        AutoRefine {
            on: !env_off && settings["refine"]["auto"].as_bool().unwrap_or(true),
            last_pass: Mutex::new(None),
        }
    }

    /// Whether a proposal pass may start now (on, and past the cooldown); starting one restarts the cooldown.
    pub fn start_pass(&self) -> bool {
        let mut last = jsrt::sync::lock(&self.last_pass);
        if !self.on || last.is_some_and(|at| at.elapsed() < COOLDOWN) {
            return false;
        }
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
        "[refinements] Instructions this session learned from its own compaction summaries, each backed by a quote \
         from one. Follow them unless the user says otherwise; `refinements.rollback` with an id retires a wrong one."
            .to_string()
    };
    for refinement in active {
        let text = refinement["text"].as_str().unwrap_or_default();
        let cut = text.char_indices().take_while(|(at, _)| *at < MAX_TEXT).map(|(_, c)| c);
        let scope_tag = match refinement["scope"].as_str() {
            Some("project") => " project",
            Some("global") => " global",
            _ => "",
        };
        let kind_tag = match refinement["kind"].as_str() {
            Some(kind) if kind != "instruction" => format!(" {kind}"),
            _ => String::new(),
        };
        let title_prefix = refinement["title"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map_or_default(|t| format!("{t}: "));
        content.push_str(&format!(
            "\n- [{} v{}{}{}] {}{}",
            refinement["id"].as_str().unwrap_or_default(),
            refinement["version"],
            scope_tag,
            kind_tag,
            title_prefix,
            cut.collect::<String>()
        ));
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

/// One proposed refinement, its quote already found verbatim in the summary.
#[derive(Debug, PartialEq)]
pub struct Proposal {
    pub text: String,
    pub quote: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub scope: Scope,
    pub kind: Kind,
    pub title: Option<String>,
    pub tags: Vec<String>,
}

/// What a proposal reply asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    pub rollback: Vec<String>,
}

/// The plan in a proposal `reply` (`{"proposals": [{text, quote, rationale, expectedOutcome, scope?, kind?, title?,
/// tags?}], "rollback": [ids]}`, fenced or not): at most [`MAX_PROPOSALS`], each with text of at most [`MAX_TEXT`]
/// bytes and a quote that is a verbatim substring of `summary`; the rest are dropped.
pub fn plan(reply: &str, summary: &str) -> Plan {
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
    let proposals = value["proposals"].as_array().into_iter().flatten().take(MAX_PROPOSALS);
    Plan {
        proposals: proposals
            .filter_map(|item| {
                let mut seen_tags = HashSet::new();
                let tags: Vec<String> = item["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|t| t.trim().to_ascii_lowercase())
                    .filter(|t| !t.is_empty() && seen_tags.insert(t.clone()))
                    .take(MAX_TAGS)
                    .collect();
                let proposal = Proposal {
                    text: field(item, "text")?,
                    quote: item["quote"].as_str()?.to_string(),
                    rationale: field(item, "rationale")?,
                    expected_outcome: field(item, "expectedOutcome")?,
                    scope: Scope::parse(item["scope"].as_str()),
                    kind: Kind::parse(item["kind"].as_str()),
                    title: field(item, "title"),
                    tags,
                };
                (proposal.text.len() <= MAX_TEXT
                    && proposal.quote.trim().chars().count() >= MIN_QUOTE_CHARS
                    && summary.contains(&proposal.quote))
                .then_some(proposal)
            })
            .collect(),
        rollback: value["rollback"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect(),
    }
}

fn prompt(summary: &str, records: &[Value], scoped_records: &[Value]) -> String {
    let mut all_active = active(records);
    all_active.extend(
        scoped_records
            .iter()
            .filter(|r| r["state"] == "active")
            .cloned()
            .map(scoped_view),
    );
    let active_json = serde_json::to_string_pretty(&all_active).unwrap_or_default();
    let mut history: Vec<Value> = records[records.len().saturating_sub(HISTORY_LIMIT)..]
        .iter()
        .map(|r| {
            let view = services::refinement_view(r);
            json!({"id": view["id"], "state": r["state"], "scope": "session", "text": view["text"]})
        })
        .collect();
    for r in &scoped_records[scoped_records.len().saturating_sub(HISTORY_LIMIT)..] {
        history.push(json!({
            "id": r["id"],
            "state": r["state"],
            "scope": r["scope"],
            "text": r["text"],
        }));
    }
    let history_json = serde_json::to_string_pretty(&history).unwrap_or_default();
    format!(
        "<summary>\n{summary}\n</summary>\n\n<active-refinements>\n{active_json}\n</active-refinements>\n\n\
         <refinement-history>\n{history_json}\n</refinement-history>\n\n\
         Propose at most {MAX_PROPOSALS} short refinements (each under {MAX_TEXT} bytes) the agent should follow. \
         Prefer none over speculative ones: only repeated failures, user corrections or reusable tactics the summary \
         shows. Each needs `quote`, an excerpt copied character for character from the summary that shows why; a \
         proposal whose quote is not in the summary is discarded. Optional fields: `scope` (\"session\" default, \
         \"project\" for repo-wide rules, \"global\" for universal user preferences), `kind` (\"instruction\" default, \
         \"skill\", \"subagent\", \"memory\"), `title`, and `tags` (short keywords for retrieval). Do not repeat an \
         active refinement. List in `rollback` the ids of active refinements the summary shows to be wrong or harmful.\n\n\
         Answer with JSON only: {{\"proposals\": [{{\"text\": \"...\", \"quote\": \"...\", \"rationale\": \"...\", \
         \"expectedOutcome\": \"...\", \"scope\": \"session\", \"kind\": \"instruction\", \"title\": \"...\", \
         \"tags\": []}}], \"rollback\": []}}"
    )
}

fn now_ms() -> u64 {
    jsrt::date::now_ms().max(0) as u64
}

/// One proposal pass over the compaction `compaction_id`'s `summary`, in the background: `ask(system, prompt)` is
/// the side request; its plan is applied to `ledger`. Failures are not reported.
pub fn propose(
    ledger: Ledger,
    compaction_id: String,
    summary: String,
    ask: impl FnOnce(&str, &str) -> Result<String, String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let records = ledger.records(ledger.branch().as_ref());
        let scoped = ledger.store.as_ref().map_or_else(Vec::new, ScopedStore::all);
        let Ok(reply) = ask(SYSTEM, &prompt(&summary, &records, &scoped)) else {
            return;
        };
        apply(&ledger, &plan(&reply, &summary), &compaction_id);
    });
}

/// Rolls back the plan's active rollbacks, then proposes and activates its proposals (session proposals in the
/// session ledger; project/global proposals in the persistent [`ScopedStore`]).
fn apply(ledger: &Ledger, plan: &Plan, compaction_id: &str) {
    let branch = ledger.branch();
    let records = ledger.records(branch.as_ref());
    let current = active(&records);
    for id in &plan.rollback {
        if current.iter().any(|r| r["id"] == id.as_str()) {
            let _ = ledger
                .services
                .handle("refinements.rollback", &json!({"id": id}), branch.as_ref());
        }
    }
    let mut next = records.len();
    for proposal in plan.proposals.iter().filter(|p| p.scope == Scope::Session) {
        if current.iter().any(|r| r["text"] == proposal.text.as_str()) {
            continue;
        }
        next += 1;
        let request = json!({
            "kind": "instruction",
            "target": format!("{TARGET_PREFIX}r{next}"),
            "baseVersion": 0,
            "content": proposal.text,
            "evidence": [{
                "ref": format!("compaction:{compaction_id}"),
                "quote": proposal.quote,
                "rationale": proposal.rationale,
                "expectedOutcome": proposal.expected_outcome,
            }],
            "scope": "session",
        });
        if let Ok(record) = ledger.services.handle("refinements.propose", &request, branch.as_ref()) {
            let _ = ledger
                .services
                .handle("refinements.activate", &json!({"id": record["id"]}), branch.as_ref());
        }
    }
    if let Some(store) = &ledger.store {
        store.apply(plan, compaction_id, now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str =
        "## Progress\nThe user corrected the agent twice: run tests with scripts/nextest.sh, not cargo test.";

    fn reply(proposals: Value) -> String {
        json!({"proposals": proposals, "rollback": ["r-old"]}).to_string()
    }

    fn proposal(text: &str, quote: &str) -> Value {
        json!({"text": text, "quote": quote, "rationale": "user corrected it", "expectedOutcome": "no reruns"})
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
                    scope: Scope::Session,
                    kind: Kind::Instruction,
                    title: None,
                    tags: Vec::new(),
                }],
                rollback: vec!["r-old".into()],
            }
        );
    }

    #[test]
    fn auto_refine_plan_parses_scope_kind_title_and_tags() {
        let raw = json!({
            "proposals": [{
                "text": "Always use scripts/nextest.sh for Rust tests.",
                "quote": "run tests with scripts/nextest.sh",
                "rationale": "CPU-capped runner",
                "expectedOutcome": "no OOM or thrash",
                "scope": "project",
                "kind": "skill",
                "title": "Nextest Runner",
                "tags": ["Rust", "nextest", "rust", " "]
            }],
            "rollback": []
        })
        .to_string();
        let parsed = plan(&raw, SUMMARY);
        assert_eq!(parsed.proposals.len(), 1);
        let p = &parsed.proposals[0];
        assert_eq!(p.scope, Scope::Project);
        assert_eq!(p.kind, Kind::Skill);
        assert_eq!(p.title.as_deref(), Some("Nextest Runner"));
        assert_eq!(p.tags, vec!["rust", "nextest"]);
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
        let active = [json!({"id": "r1", "version": 1, "text": "Use nextest."})];
        let message = message(&active, None, 7).unwrap();
        assert!(message["content"].as_str().unwrap().contains("[r1 v1] Use nextest."));
        let entries = [json!({"type": "message", "message": message})];
        let shown = shown(&entries);
        assert_eq!(shown.as_deref(), Some(&["r1".to_string()][..]));
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
    }

    #[test]
    fn auto_refine_scoped_store_persists_across_sessions_and_supports_inverse_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        let project_a = dir.path().join("repo-a");
        let project_b = dir.path().join("repo-b");

        let store_a = ScopedStore::new(agent_dir.clone(), project_a.clone());
        let plan = Plan {
            proposals: vec![
                Proposal {
                    text: "Run nextest in repo-a.".into(),
                    quote: "run tests with scripts/nextest.sh".into(),
                    rationale: "repo-a runner".into(),
                    expected_outcome: "fast tests".into(),
                    scope: Scope::Project,
                    kind: Kind::Instruction,
                    title: Some("Repo A Tests".into()),
                    tags: vec!["nextest".into(), "cargo".into()],
                },
                Proposal {
                    text: "Never add Co-Authored-By trailers.".into(),
                    quote: "no Co-Authored-By trailers".into(),
                    rationale: "global git policy".into(),
                    expected_outcome: "clean commits".into(),
                    scope: Scope::Global,
                    kind: Kind::Instruction,
                    title: Some("Git Trailers".into()),
                    tags: vec!["git".into(), "commit".into()],
                },
            ],
            rollback: Vec::new(),
        };
        store_a.apply(&plan, "cmp-1", 1000);

        let reloaded_a = ScopedStore::new(agent_dir.clone(), project_a);
        let active_a = reloaded_a.active();
        assert_eq!(active_a.len(), 2);
        assert_eq!(active_a[0]["id"], "proj-r1");
        assert_eq!(active_a[1]["id"], "glob-r1");

        let store_b = ScopedStore::new(agent_dir, project_b);
        let active_b = store_b.active();
        assert_eq!(active_b.len(), 1, "project-b must see global but not project-a");
        assert_eq!(active_b[0]["id"], "glob-r1");

        assert!(reloaded_a.rollback(Scope::Project, "proj-r1", 2000));
        assert_eq!(reloaded_a.active().len(), 1);
        assert!(reloaded_a.restore_rollback(Scope::Project, "proj-r1", 3000));
        let restored = reloaded_a.active();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0]["version"], 3);
    }

    #[test]
    fn auto_refine_rank_prefers_goal_weighted_idf_matches() {
        let candidates = vec![
            json!({"id": "r1", "version": 1, "scope": "global", "title": "Formatting", "tags": ["style"], "text": "Keep Rust code formatted cleanly."}),
            json!({"id": "r2", "version": 1, "scope": "project", "title": "Kernel Snapshot", "tags": ["snapshot", "hmac"], "text": "Verify HMAC signatures on Rust kernel snapshots."}),
            json!({"id": "r3", "version": 1, "scope": "session", "title": "SQLite", "tags": ["sqlite", "wal"], "text": "Enable WAL mode on SQLite databases."}),
        ];
        let ranked = rank_refinements(
            &candidates,
            "Fix the HMAC snapshot signature failure",
            &["We were checking Rust formatting earlier".into()],
            2,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["id"], "r2", "goal (3x) + rare IDF terms should rank r2 first");
        assert_eq!(ranked[1]["id"], "r1", "recent turn match should rank r1 second");
    }
}
