//! `rlm.progress`: how each running subagent is doing, for a person who asks. Plain code over the child's transcript
//! (no model call): a described level (what it is doing now, never a percentage) and forecasts of its time and cost
//! from its timeout and its spend so far.

use serde_json::{Value, json};
use std::sync::Arc;

/// A running child's transcript (user, assistant and tool result messages, oldest first) by its lane name; `None`
/// when the runtime has no conversation for it (yet).
pub type TranscriptHook = Arc<dyn Fn(&str) -> Option<Vec<Value>> + Send + Sync>;

/// No activity for this long reads as stalled.
pub const STALL_MS: f64 = 5.0 * 60_000.0;
/// This many failed cells in a row read as stalled.
const FAILED_IN_ROW: usize = 3;

/// Code that changes files: the REPL's edit/write, Python file writes, in-place shell edits, patches.
const CHANGES: [&str; 8] = [
    "edit(",
    "write(",
    ".write_text(",
    ".write_bytes(",
    "'w')",
    "\"w\")",
    "sed -i",
    "apply_patch",
];
/// Commands that check work: tests, builds, type checks, linters.
const CHECKS: [&str; 16] = [
    "pytest",
    "cargo test",
    "cargo check",
    "cargo build",
    "cargo clippy",
    "nextest",
    "npm test",
    "npm run",
    "pnpm ",
    "go test",
    "go build",
    "make ",
    "tsc",
    "mypy",
    "ruff",
    "eslint",
];

/// What a child has done, read from its transcript.
#[derive(Debug, Default, PartialEq)]
pub struct Activity {
    pub responses: usize,
    pub cells: usize,
    /// Cost its responses reported (USD).
    pub cost_usd: f64,
    /// Timestamp of its newest message (ms).
    pub last_at: Option<f64>,
    /// Index of the newest cell that changed files, and of the newest that ran a check.
    last_change: Option<usize>,
    last_check: Option<usize>,
    finished: bool,
    failed_in_row: usize,
}

impl Activity {
    pub fn of(messages: &[Value]) -> Activity {
        let mut activity = Activity::default();
        for message in messages {
            if let Some(at) = message["timestamp"].as_f64().filter(|at| *at > 0.0) {
                activity.last_at = Some(activity.last_at.map_or(at, |last| last.max(at)));
            }
            match message["role"].as_str() {
                Some("assistant") => {
                    activity.responses += 1;
                    activity.cost_usd += message["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                    for block in message["content"].as_array().into_iter().flatten() {
                        if block["type"] != "toolCall" {
                            continue;
                        }
                        let code = block["arguments"]["code"].as_str().unwrap_or_default();
                        let index = activity.cells;
                        activity.cells += 1;
                        if CHANGES.iter().any(|c| code.contains(c)) {
                            activity.last_change = Some(index);
                        }
                        if CHECKS.iter().any(|c| code.contains(c)) {
                            activity.last_check = Some(index);
                        }
                        if code.contains("rlm.finish(") {
                            activity.finished = true;
                        }
                    }
                }
                Some("toolResult") => {
                    if message["isError"] == true {
                        activity.failed_in_row += 1;
                    } else {
                        activity.failed_in_row = 0;
                    }
                }
                _ => {}
            }
        }
        activity
    }

    /// The level it is at and what that means.
    pub fn level(&self, now_ms: f64) -> (&'static str, String) {
        if self.failed_in_row >= FAILED_IN_ROW {
            return (
                "stalled",
                format!("Its last {} cells failed in a row.", self.failed_in_row),
            );
        }
        if let Some(idle) = self.last_at.map(|at| now_ms - at).filter(|idle| *idle >= STALL_MS) {
            return ("stalled", format!("No activity for {} min.", (idle / 60_000.0).floor()));
        }
        let level = if self.finished {
            ("finishing", "Recorded its verdict (rlm.finish); writing its reply.")
        } else if self.responses == 0 {
            ("starting", "Its first model response has not come back yet.")
        } else if self.last_change.is_none() {
            ("exploring", "Reading and searching; it has changed no files yet.")
        } else if self.last_check > self.last_change {
            (
                "checking",
                "Checking its changes: a test, build or linter ran after its last edit.",
            )
        } else {
            ("changing", "Editing files; nothing checked them since its last edit.")
        };
        (level.0, level.1.to_string())
    }
}

/// A child's progress entry: its level and forecasts. `elapsed_ms` since it started, `timeout_ms` its limit.
pub fn report(messages: &[Value], now_ms: f64, elapsed_ms: f64, timeout_ms: Option<u64>) -> Value {
    let activity = Activity::of(messages);
    let (level, description) = activity.level(now_ms);
    let minutes = elapsed_ms / 60_000.0;
    let rate = (minutes > 0.0).then(|| activity.cost_usd / minutes);
    let left_ms = timeout_ms.map(|limit| (limit as f64 - elapsed_ms).max(0.0));
    json!({
        "level": level,
        "description": description,
        "responses": activity.responses,
        "cells": activity.cells,
        "elapsed_s": (elapsed_ms / 1000.0).round(),
        "cost_usd": activity.cost_usd,
        "forecast": {
            // At most this long before its timeout ends it.
            "time_left_s_max": left_ms.map(|left| (left / 1000.0).round()),
            "cost_usd_per_min": rate,
            // What it will have cost if it runs to its timeout at its rate so far.
            "cost_usd_at_timeout": match (rate, left_ms) {
                (Some(rate), Some(left)) => Some(activity.cost_usd + rate * left / 60_000.0),
                _ => None,
            },
        },
    })
}
