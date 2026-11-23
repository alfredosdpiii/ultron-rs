//! Background memory, playbook routing, refinement and procedure-mining helpers for [`super::Core`].

use super::*;

impl Core {
    /// The `ultron.memory` message a run of the user message `text` opens with, when automatic recall finds
    /// memories the context does not show yet (crate::auto_memory).
    pub(super) fn recalled_memory(&self, text: &str) -> Option<Value> {
        if !self.auto_memory.recall_ready() || !crate::auto_memory::worth_recalling(text) {
            return None;
        }
        let entries = lock_session(&self.session)
            .branch_entries(MAIN_LANE)
            .unwrap_or_default();
        let shown = crate::auto_memory::shown(&entries);
        let recalled = self.auto_memory.recall(&self.lane.stack, text, &shown);
        crate::auto_memory::recall_message(&recalled, now_ms() as u64)
    }

    /// The `ultron.playbook` message a run of the user message `text` opens with, in engineering mode
    /// (crate::playbook_route).
    pub(super) fn routed_playbook(&self, text: &str) -> Option<Value> {
        crate::playbook_route::route(text)?;
        let manager = (self.runtime.config.settings_reader())();
        if !config::keys::engineering(manager.settings()) {
            return None;
        }
        let rigor = std::path::PathBuf::from(self.runtime.config.bundled_skills()?).join("pstack/rigor");
        let entries = lock_session(&self.session)
            .branch_entries(MAIN_LANE)
            .unwrap_or_default();
        let shown = crate::playbook_route::shown(&entries);
        crate::playbook_route::playbook_message(text, &rigor, &shown, now_ms() as u64)
    }

    /// The `ultron.refinements` message a root user run opens with, when the active refinements ranked for `prompt`
    /// differ from what the context shows (crate::auto_refine).
    pub(super) fn refinements_message(&self, prompt: &str) -> Option<Value> {
        if !self.auto_refine.on {
            return None;
        }
        let entries = lock_session(&self.session)
            .branch_entries(MAIN_LANE)
            .unwrap_or_default();
        let shown = crate::auto_refine::shown(&entries);
        let recent_turns: Vec<String> = entries
            .iter()
            .rev()
            .filter(|entry| entry["type"] == "message")
            .filter_map(|entry| {
                let message = &entry["message"];
                let role = message["role"].as_str()?;
                if role != "user" && role != "assistant" {
                    return None;
                }
                let text = message_text(message);
                (!text.trim().is_empty()).then_some(text)
            })
            .take(4)
            .collect();
        let active = self.lane.stack.ledger.active_ranked(prompt, &recent_turns);
        crate::auto_refine::message(&active, shown.as_deref(), now_ms() as u64)
    }

    /// Selects a uniquely matched admitted mined procedure for pre-planning execution, if any.
    pub(super) fn mined_procedure_message(&self, prompt: &str) -> Option<Value> {
        let procedures = self.lane.stack.mined_procedures.list();
        let plan = delegation::procedure_miner::select_pre_run(prompt, &procedures)?;
        Some(delegation::procedure_miner::pre_run_message(&plan, now_ms() as u64))
    }

    /// Observes completed root turns in the background to mine recurring procedures and record telemetry.
    pub(super) fn observe_mined_procedures(&self, aborted: bool, failed: bool) {
        if aborted {
            return;
        }
        let entries = lock_session(&self.session)
            .branch_entries(MAIN_LANE)
            .unwrap_or_default();
        let store = self.lane.stack.mined_procedures.clone();
        let ok = !failed;
        let ts = now_ms() as u64;
        std::thread::spawn(move || {
            let traces = delegation::procedure_miner::extract_traces(&entries);
            if let Some(last_proc_id) = entries
                .iter()
                .rev()
                .take_while(|entry| entry["type"] != "message" || entry["message"]["role"] != "user")
                .chain(
                    entries
                        .iter()
                        .rev()
                        .skip_while(|entry| entry["type"] != "message" || entry["message"]["role"] != "user")
                        .skip(1)
                        .take(3),
                )
                .find(|entry| {
                    entry["type"] == "message"
                        && entry["message"]["role"] == "custom"
                        && entry["message"]["customType"] == protocol::custom_types::PROCEDURE
                })
                .and_then(|entry| entry["message"]["details"]["procedureId"].as_str())
            {
                let (requests, tokens) = traces
                    .last()
                    .filter(|t| t.used_procedure)
                    .map_or((1, 0), |t| (t.requests, t.tokens));
                store.record_run(last_proc_id, ok, requests, tokens, ts);
            }
            if ok {
                let mined = delegation::procedure_miner::mine(&traces, ts);
                let _ = store.upsert_mined(mined, ts);
            }
        });
    }

    /// Propose refinements from the newest compaction's summary in the background, when automatic refinement is on
    /// and past its cooldown (crate::auto_refine).
    fn refine_after_compaction(&self) {
        let newest = {
            let session = lock_session(&self.session);
            let entries = session.branch_entries(MAIN_LANE).unwrap_or_default();
            entries
                .iter()
                .rev()
                .find(|entry| entry["type"] == "compaction")
                .and_then(|entry| {
                    Some((
                        entry["id"].as_str()?.to_string(),
                        entry["summary"].as_str()?.to_string(),
                    ))
                })
        };
        let Some((id, summary)) = newest.filter(|(_, summary)| !summary.trim().is_empty()) else {
            return;
        };
        if !self.auto_refine.start_pass() {
            return;
        }
        let summarizer = self.summarizer();
        crate::auto_refine::propose(self.lane.stack.ledger.clone(), id, summary, move |system, prompt| {
            summarizer.ask(system, prompt)
        });
    }

    /// After a completed compaction: distil its summary into long-term memory, when automatic retain is on
    /// (crate::auto_memory), and propose refinements from it (crate::auto_refine).
    pub(super) fn retain_compaction(&self) {
        self.refine_after_compaction();
        if !self.auto_memory.retain {
            return;
        }
        let (session_id, entries) = {
            let session = lock_session(&self.session);
            (
                session.id().to_string(),
                session.branch_entries(MAIN_LANE).unwrap_or_default(),
            )
        };
        let cwd = self.runtime.config.cwd.display().to_string();
        if let Some(keep) = crate::auto_memory::summary_keep(&session_id, &cwd, &entries) {
            self.keep_in_background(keep);
        }
    }

    /// Keep what no compaction summarized in long-term memory, when automatic retain is on (crate::auto_memory).
    /// `in_background` when the session stays open (it went quiet): the call does not hold the session up, and its
    /// outcome is shown. As it closes, the keep waits at most [`crate::auto_memory::CLOSE_TIMEOUT`] and is not shown.
    pub(super) fn retain_tail(&self, in_background: bool) {
        if !self.auto_memory.retain {
            return;
        }
        let (session_id, entries) = {
            let session = lock_session(&self.session);
            (
                session.id().to_string(),
                session.branch_entries(MAIN_LANE).unwrap_or_default(),
            )
        };
        let cwd = self.runtime.config.cwd.display().to_string();
        let Some(keep) = crate::auto_memory::tail_keep(&session_id, &cwd, &entries) else {
            return;
        };
        if in_background {
            self.keep_in_background(keep);
        } else {
            let summarizer = self.summarizer();
            let agent_dir = self.runtime.config.agent_dir.clone();
            crate::auto_memory::keep(
                &self.lane.stack,
                &agent_dir,
                keep,
                move |system, prompt| summarizer.ask(system, prompt),
                crate::auto_memory::CLOSE_TIMEOUT,
            );
        }
    }

    /// Distils `keep` on a thread (the side request is the session's summarizer, bounded by IDLE_WAIT); its outcome
    /// comes back to the actor as `Input::MemoryKept`.
    fn keep_in_background(&self, keep: crate::auto_memory::Keep) {
        let summarizer = self.summarizer();
        let (stack, agent_dir, tx) = (
            self.lane.stack.clone(),
            self.runtime.config.agent_dir.clone(),
            self.tx.clone(),
        );
        std::thread::spawn(move || {
            let outcome = crate::auto_memory::keep(
                &stack,
                &agent_dir,
                keep,
                move |system, prompt| summarizer.ask(system, prompt),
                IDLE_WAIT,
            );
            if let Some(outcome) = outcome {
                let _ = tx.send(Input::MemoryKept(outcome));
            }
        });
    }

    /// Shows a background keep's outcome as one display-only line, when the outcome is worth one. Not journaled: the
    /// model never sees it.
    pub(super) fn memory_kept(&mut self, outcome: crate::auto_memory::Outcome) {
        if outcome.shown() {
            let message = crate::auto_memory::outcome_message(&outcome, now_ms() as u64);
            self.announce_message(&message);
        }
    }
}
