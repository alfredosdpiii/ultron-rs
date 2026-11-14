//! What the host checks when a root run on `main` ends normally with nothing waiting, after the keep-going and
//! persona nudges (core::persona_nudge) had their turn. The first that applies continues the turn with a nudge
//! message (a displayed custom message, journaled like any run's prompt):
//!
//! 1. An empty reply (no text and no call, reasoning at most): "continue", at most [`MAX_EMPTY`] times in a row.
//! 2. Subagents of the root that ended with `rlm.finish(left=[...])`: their items, once per subagent, so each is
//!    done, tracked or dropped with a reason before the turn ends.
//! 3. The `turnEndChecks` setting (commands, as a spawn contract's `run` checks) after a run that made an `rlm`
//!    call: the host runs them off the actor, under the session's sandbox, and continues the turn with the ones
//!    that failed, at most [`MAX_CHECK_CONTINUES`] times since the user's last input.
//!
//! 1 and 2 are off under `ULTRON_TURN_GUARDS=off` and the TypeScript guide (crate::compose::turn_guards); 3 is on
//! when the setting names checks.

use super::{Core, Input, MAIN_LANE};
use delegation::spawn_contract::{CheckPlace, Hop, HostCheck, SpawnContract};
use jsrt::sync;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::Arc;

/// Empty replies in a row that are continued; the next one ends the turn.
pub const MAX_EMPTY: u32 = 2;
/// Turns continued for failed `turnEndChecks` since the user's last input.
pub const MAX_CHECK_CONTINUES: u32 = 5;
pub const CHECKS_KEY: &str = "turnEndChecks";
pub const EMPTY_TEXT: &str = "Your last reply was empty. Continue the work; if it is done, say in a sentence or two what was done and what is left.";

#[derive(Default)]
pub(super) struct TurnGuard {
    /// The run's last assistant message had no text and no call.
    last_empty: bool,
    /// Empty replies continued in a row.
    empty: u32,
    /// Subagents whose left items the root was already given.
    // ponytail: in memory, so a restarted worker gives earlier subagents' items once more; keep the ids beside the
    // persona state (session values) if that repeats too often.
    left_seen: HashSet<String>,
    /// Turns continued for failed checks since the user's last input.
    check_continues: u32,
    /// The checks running off the actor: their number and the signal that stops them.
    checking: Option<(u64, Signal)>,
    check_seq: u64,
}

/// Whether an assistant message (as stored) says and calls nothing.
pub fn is_empty_reply(message: &Value) -> bool {
    let blocks = message["content"].as_array().map(Vec::as_slice).unwrap_or_default();
    !blocks.iter().any(|block| {
        block["type"] == "toolCall"
            || (block["type"] == "text" && !block["text"].as_str().unwrap_or("").trim().is_empty())
    })
}

/// The `turnEndChecks` setting: commands, each a string (it must exit 0) or `{"run", "expect", "timeout_ms"}` as in
/// a spawn contract.
pub fn parse_checks(settings: &Map<String, Value>) -> Result<Vec<HostCheck>, String> {
    let items = match settings.get(CHECKS_KEY) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(format!("{CHECKS_KEY} must be a list of commands")),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let written = match item {
                Value::String(command) => json!({ "run": command }),
                other => other.clone(),
            };
            match HostCheck::parse(&written, index) {
                Ok(check @ HostCheck::Run { .. }) => Ok(check),
                Ok(_) => Err(format!(
                    "{CHECKS_KEY}[{index}] must be a command (a string or {{\"run\": ...}})"
                )),
                Err(error) => Err(error.replace("contract.checks", CHECKS_KEY)),
            }
        })
        .collect()
}

fn nudge(guard: &str, count: Option<(u32, u32)>, text: String) -> Value {
    let mut details = json!({ "guard": guard });
    if let Some((n, max)) = count {
        details["nudge"] = n.into();
        details["max"] = max.into();
    }
    json!({
        "role": "custom",
        "customType": protocol::custom_types::NUDGE,
        "content": text,
        "display": true,
        "details": details,
        "timestamp": jsrt::date::now_ms(),
    })
}

pub fn left_text(left: &[(String, String, Vec<String>)]) -> String {
    let mut lines = vec!["Subagents ended with work left undone or problems found outside their briefs:".to_string()];
    for (id, name, items) in left {
        lines.push(format!("{name} ({id}):"));
        lines.extend(items.iter().map(|item| format!("- {item}")));
    }
    lines.push(
        "Settle each before you end the turn: do it now, track it as a task, or say why it will never need doing (\"later\" is not a reason). One you already settled needs only a line saying so."
            .into(),
    );
    lines.join("\n")
}

pub fn checks_text(failed: &[String]) -> String {
    format!(
        "The host ran this project's turn-end checks ({CHECKS_KEY}) and these did not pass:\n{}\nFix what they show and end the turn again; if one cannot pass here, say why.",
        failed.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
    )
}

impl Core {
    /// An assistant message of the run ended.
    pub(super) fn turn_guard_assistant(&mut self, message: &Value) {
        self.turn_guard.last_empty = is_empty_reply(message);
    }

    /// The user sent input: the counts start over, and checks still running are for a turn that has moved on.
    pub(super) fn turn_guard_user_input(&mut self) {
        self.turn_guard.empty = 0;
        self.turn_guard.check_continues = 0;
        self.turn_guard_cancel();
    }

    /// Stops the checks in progress, if any; their result is dropped.
    pub(super) fn turn_guard_cancel(&mut self) {
        if let Some((_, signal)) = self.turn_guard.checking.take() {
            signal.abort("turn moved on");
        }
    }

    /// Whether turn-end checks are running (the session is not idle).
    pub(super) fn turn_guard_checking(&self) -> bool {
        self.turn_guard.checking.is_some()
    }

    /// The run ended (`aborted`, `failed`) and the persona's nudges had their turn.
    pub(super) fn turn_guard_run_ended(&mut self, aborted: bool, failed: bool) {
        let empty = std::mem::take(&mut self.turn_guard.last_empty);
        let pending =
            !sync::lock(&self.queues).items.is_empty() || !self.deferred.is_empty() || !self.after_run.is_empty();
        if aborted || failed || pending || self.run.is_some() {
            return;
        }
        if self.runtime.config.turn_guards {
            if !empty {
                self.turn_guard.empty = 0;
            } else if self.turn_guard.empty < MAX_EMPTY {
                self.turn_guard.empty += 1;
                let message = nudge(
                    "empty reply",
                    Some((self.turn_guard.empty, MAX_EMPTY)),
                    EMPTY_TEXT.into(),
                );
                // A nudge that cannot start is dropped, as the persona's is.
                let _ = self.prompt_with(message, None, true);
                return;
            }
            let mut left = self.lane.stack.delegation.left_items(MAIN_LANE);
            left.retain(|(id, _, _)| self.turn_guard.left_seen.insert(id.clone()));
            if !left.is_empty() {
                let _ = self.prompt_with(nudge("work left by subagents", None, left_text(&left)), None, true);
                return;
            }
        }
        if self.persona.tool_calls > 0 && self.turn_guard.check_continues < MAX_CHECK_CONTINUES {
            self.turn_guard_check();
        }
    }

    /// Runs the settings' turn-end checks off the actor; [`Input::TurnChecked`] brings what failed.
    fn turn_guard_check(&mut self) {
        let config = &self.runtime.config;
        // Read when the turn ends, so an edit of the settings applies; a list that does not parse was reported
        // when the session started.
        let settings =
            config::settings::SettingsManager::create(&self.env.cwd, &config.agent_dir, config.project_trusted);
        let checks = parse_checks(settings.settings()).unwrap_or_default();
        if checks.is_empty() {
            return;
        }
        self.turn_guard.check_seq += 1;
        let (id, signal) = (self.turn_guard.check_seq, Signal::new());
        self.turn_guard.checking = Some((id, signal.clone()));
        let runner = crate::contract_checks::check_runner(Arc::downgrade(&self.lane.stack));
        let (tx, cwd, env) = (self.tx.clone(), self.env.cwd.clone(), config.child_env());
        std::thread::spawn(move || {
            let contract = SpawnContract {
                role: None,
                checks,
                returns: None,
                hop: Hop::Report,
            };
            let place = CheckPlace {
                cwd: Some(&cwd),
                env: &env,
                diff: None,
                signal: &signal,
            };
            let report = contract.evaluate(None, &place, &runner);
            let failed = report["failed"]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let _ = tx.send(Input::TurnChecked { id, failed });
        });
        // The session is not idle while they run.
        self.report_activity();
    }

    /// The checks numbered `id` ended with `failed`: the turn continues with them, unless it has moved on.
    pub(super) fn turn_checked(&mut self, id: u64, failed: Vec<String>) {
        if self.turn_guard.checking.as_ref().map(|(current, _)| *current) != Some(id) {
            return;
        }
        self.turn_guard.checking = None;
        if !failed.is_empty() && self.run.is_none() {
            self.turn_guard.check_continues += 1;
            let count = Some((self.turn_guard.check_continues, MAX_CHECK_CONTINUES));
            let _ = self.prompt_with(nudge("turn-end checks failed", count, checks_text(&failed)), None, true);
        }
        self.report_activity();
    }
}
