//! Automatic instruction refinements (Rust-only): the session learns from its own compaction summaries, with
//! evidence it cannot invent.
//!
//! - Propose: after a completed compaction, in the background, one side request with the new summary, the active
//!   refinements and the recent ledger history asks for at most [`MAX_PROPOSALS`] small instructions. [`plan`] keeps
//!   only proposals whose `quote` is a verbatim substring of the summary (no invented failures); each kept one is
//!   proposed and activated in the session's refinements ledger (kind `instruction`, target `instruction:main.r<n>`,
//!   branch-anchored, checked by the ledger's own validator), and listed rollbacks of active ones are rolled back.
//!   At most one pass per [`COOLDOWN`]; every failure is silent.
//! - Deliver: before a root run of a user message, when the active `instruction:main.*` refinements differ from what
//!   the context shows since its last compaction, the run opens with one `ultron.refinements` custom message listing
//!   them (on the branch, like recalled memory, so the prompt cache holds). The summarizer never sees it (compaction
//!   `convert_for_summary`).
//! - Both run only while the `refine.auto` setting is on (default on) and `ULTRON_AUTO_REFINE` is not `off` (tests
//!   that count requests turn it off); off, the session runs the unmodified harness.

use serde_json::{Value, json};
use services::{Branch, BranchSource, LocalServices};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use protocol::custom_types::REFINEMENTS;
/// Targets of main-lane refinements: `instruction:main.r<n>` (the ledger keeps one active record per target).
const TARGET_PREFIX: &str = "instruction:main.";
/// The least time between two proposal passes.
pub const COOLDOWN: Duration = Duration::from_secs(20 * 60);
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

const SYSTEM: &str = "You review a coding agent's compaction summary and propose small, durable instructions the agent \
should follow for the rest of this session. Answer with JSON only.";

/// The session's refinements ledger and the branch its changes follow (compose builds it).
#[derive(Clone)]
pub struct Ledger {
    pub services: Arc<LocalServices>,
    pub branch: Option<BranchSource>,
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

    /// The active main-lane refinements on the current branch: `{id, version, text}`, oldest first.
    pub fn active(&self) -> Vec<Value> {
        active(&self.records(self.branch().as_ref()))
    }
}

fn active(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .filter(|record| record["state"] == "active")
        .map(services::refinement_view)
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
        let mut last = self.last_pass.lock().unwrap_or_else(PoisonError::into_inner);
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
        content.push_str(&format!(
            "\n- [{} v{}] {}",
            refinement["id"].as_str().unwrap_or_default(),
            refinement["version"],
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
}

/// What a proposal reply asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    pub rollback: Vec<String>,
}

/// The plan in a proposal `reply` (`{"proposals": [{text, quote, rationale, expectedOutcome}], "rollback": [ids]}`,
/// fenced or not): at most [`MAX_PROPOSALS`], each with text of at most [`MAX_TEXT`] bytes and a quote that is a
/// verbatim substring of `summary`; the rest are dropped. A reply that is not such JSON plans nothing.
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
                let proposal = Proposal {
                    text: field(item, "text")?,
                    quote: item["quote"].as_str()?.to_string(),
                    rationale: field(item, "rationale")?,
                    expected_outcome: field(item, "expectedOutcome")?,
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

/// The proposal request's prompt.
fn prompt(summary: &str, records: &[Value]) -> String {
    let active = serde_json::to_string_pretty(&active(records)).unwrap_or_default();
    let history: Vec<Value> = records[records.len().saturating_sub(HISTORY_LIMIT)..]
        .iter()
        .map(|r| {
            let view = services::refinement_view(r);
            json!({"id": view["id"], "state": r["state"], "text": view["text"]})
        })
        .collect();
    let history = serde_json::to_string_pretty(&history).unwrap_or_default();
    format!(
        "<summary>\n{summary}\n</summary>\n\n<active-refinements>\n{active}\n</active-refinements>\n\n\
         <refinement-history>\n{history}\n</refinement-history>\n\n\
         Propose at most {MAX_PROPOSALS} short instructions (each under {MAX_TEXT} bytes) the agent should follow for \
         the rest of this session. Prefer none over speculative ones: only repeated failures, user corrections or \
         reusable tactics the summary shows. Each needs `quote`, an excerpt copied character for character from the \
         summary that shows why; a proposal whose quote is not in the summary is discarded. Do not repeat an active \
         refinement. List in `rollback` the ids of active refinements the summary shows to be wrong or harmful.\n\n\
         Answer with JSON only: {{\"proposals\": [{{\"text\": \"...\", \"quote\": \"...\", \"rationale\": \"...\", \
         \"expectedOutcome\": \"...\"}}], \"rollback\": []}}"
    )
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
        let Ok(reply) = ask(SYSTEM, &prompt(&summary, &records)) else {
            return;
        };
        apply(&ledger, &plan(&reply, &summary), &compaction_id);
    });
}

/// Rolls back the plan's active rollbacks, then proposes and activates its proposals (each under a new target).
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
    for proposal in &plan.proposals {
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
                }],
                rollback: vec!["r-old".into()],
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
        assert_eq!(message["customType"], REFINEMENTS);
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
}
