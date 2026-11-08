//! The parts of `ultron/session-stats.ts` the report reads: which REPL APIs a cell names, which steer a message is,
//! and the stored runtime counters (`ultron.module/stats`).

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

/// The module name: the session value `ultron.module/stats`.
pub const SESSION_STATS_MODULE: &str = "stats";
/// The stored document's format version.
pub const STATS_VERSION: u64 = 1;

/// REPL APIs a cell's code can name, in the order the report lists them.
pub const CELL_APIS: [&str; 16] = [
    "bash",
    "read",
    "edit",
    "write",
    "rlm.load",
    "rlm.infer",
    "rlm.map",
    "rlm.spawn",
    "rlm.collect",
    "rlm.merge",
    "mcp.call",
    "tools.call",
    "workflows.run",
    "agents.*",
    "background.start",
    "view_image",
];

static API_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    // `(?<![\w.])name\s*\(`: the regex crate has no look-behind, so the preceding character is matched instead.
    let bare = |name: &str| format!(r"(?:^|[^A-Za-z0-9_.]){name}\s*\(");
    let dotted = |name: &str| format!(r"(?:^|[^A-Za-z0-9_]){}\s*\(", name.replace('.', r"\."));
    CELL_APIS
        .iter()
        .map(|&name| {
            let pattern = match name {
                "bash" | "read" | "edit" | "write" | "view_image" => bare(name),
                "agents.*" => r"(?:^|[^A-Za-z0-9_])agents\.(?:spawn|invoke|register)\s*\(".to_string(),
                _ => dotted(name),
            };
            (name, Regex::new(&pattern).expect("API pattern compiles"))
        })
        .collect()
});

/// The REPL APIs a cell's code names (each at most once): a static read of the source, so a call inside a function
/// the cell only defines counts too, and a call made through an alias does not.
pub fn cell_apis(code: &str) -> Vec<&'static str> {
    API_PATTERNS
        .iter()
        .filter(|(_, pattern)| pattern.is_match(code))
        .map(|(name, _)| *name)
        .collect()
}

static NUDGES: LazyLock<[(&'static str, Regex); 3]> = LazyLock::new(|| {
    let re = |p: &str| Regex::new(p).expect("nudge pattern compiles");
    [
        (
            "skill",
            re(r"^\[Ultron\] [0-9]+ tool rounds in a row succeeded(?-u:\b)"),
        ),
        (
            "wait",
            re(
                r"^\[Ultron\] (?:1 subagent or task you started is|[0-9]+ subagents or tasks you started are) still running(?-u:\b)",
            ),
        ),
        (
            "toolRounds",
            re(r"^\[Ultron\] You have (?:now )?used [0-9]+ rounds of tool calls(?-u:\b)"),
        ),
    ]
});

/// Which steer a `[Ultron] ...` message is (`skill`, `wait` or `toolRounds`; see crate::nudge), or None.
pub fn classify_nudge(text: &str) -> Option<&'static str> {
    if !text.starts_with("[Ultron] ") {
        return None;
    }
    NUDGES.iter().find(|(_, re)| re.is_match(text)).map(|(kind, _)| *kind)
}

/// Which counter a lane's cells go to: the root lane, `rlm.spawn` subagents, or anything else (typed agents, jobs).
pub fn stats_lane(lane: &str) -> &'static str {
    if lane == "main" {
        "root"
    } else if matches!(
        lanes::lane_names::parse_lane(lane),
        Some(lanes::lane_names::LaneOwner::Child(_))
    ) {
        "subagents"
    } else {
        "other"
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cells {
    pub count: f64,
    pub failed: f64,
    pub apis: IndexMap<String, f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guard {
    pub checks: f64,
    pub blocked: f64,
    pub unchecked: f64,
    pub after_checks: f64,
    pub after_findings: f64,
    pub ms: f64,
}

/// A stored stats document with every field present (`SessionStatsDocument`).
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub external: bool,
    pub since: f64,
    pub updated_at: f64,
    pub cells: [Cells; 3],
    pub host_calls: IndexMap<String, f64>,
    pub secrets_masked: f64,
    pub guards: IndexMap<String, Guard>,
    /// toolRounds, wait, skill.
    pub nudges: [f64; 3],
    pub usage_limit_blocks: f64,
    pub merges: IndexMap<String, String>,
    pub child_models: IndexMap<String, String>,
    pub external_turns: (f64, f64),
}

impl Stats {
    /// The stored document (`SessionStatsDocument`), every field present.
    pub fn to_json(&self) -> Value {
        let number = jsrt::json::number;
        let numbers = |map: &IndexMap<String, f64>| -> Map<String, Value> {
            map.iter().map(|(k, v)| (k.clone(), number(*v))).collect()
        };
        let texts = |map: &IndexMap<String, String>| -> Map<String, Value> {
            map.iter().map(|(k, v)| (k.clone(), json!(v))).collect()
        };
        let cells = |c: &Cells| json!({"count": number(c.count), "failed": number(c.failed), "apis": numbers(&c.apis)});
        let guards: Map<String, Value> = self
            .guards
            .iter()
            .map(|(name, g)| {
                let guard = json!({
                    "checks": number(g.checks),
                    "blocked": number(g.blocked),
                    "unchecked": number(g.unchecked),
                    "afterChecks": number(g.after_checks),
                    "afterFindings": number(g.after_findings),
                    "ms": number(g.ms),
                });
                (name.clone(), guard)
            })
            .collect();
        let [tool_rounds, wait, skill] = self.nudges;
        json!({
            "version": STATS_VERSION,
            "root": if self.external { "external" } else { "lane" },
            "since": number(self.since),
            "updatedAt": number(self.updated_at),
            "cells": {"root": cells(&self.cells[0]), "subagents": cells(&self.cells[1]), "other": cells(&self.cells[2])},
            "hostCalls": numbers(&self.host_calls),
            "secretsMasked": number(self.secrets_masked),
            "guards": guards,
            "nudges": {"toolRounds": number(tool_rounds), "wait": number(wait), "skill": number(skill)},
            "usageLimitBlocks": number(self.usage_limit_blocks),
            "merges": texts(&self.merges),
            "childModels": texts(&self.child_models),
            "externalTurns": {"count": number(self.external_turns.0), "wallMs": number(self.external_turns.1)},
        })
    }

    /// `emptySessionStats("lane", now)`.
    pub fn empty(now: f64) -> Stats {
        Stats {
            external: false,
            since: now,
            updated_at: now,
            cells: Default::default(),
            host_calls: IndexMap::new(),
            secrets_masked: 0.0,
            guards: IndexMap::new(),
            nudges: [0.0; 3],
            usage_limit_blocks: 0.0,
            merges: IndexMap::new(),
            child_models: IndexMap::new(),
            external_turns: (0.0, 0.0),
        }
    }
}

fn count(value: Option<&Value>) -> f64 {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => 0.0,
    }
}

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn counts(value: Option<&Value>) -> IndexMap<String, f64> {
    object(value)
        .map(|m| {
            jsrt::json::object_entries(m)
                .into_iter()
                .filter(|(_, v)| count(Some(v)) > 0.0)
                .map(|(k, v)| (k.clone(), count(Some(v))))
                .collect()
        })
        .unwrap_or_default()
}

fn strings(value: Option<&Value>) -> IndexMap<String, String> {
    object(value)
        .map(|m| {
            jsrt::json::object_entries(m)
                .into_iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `parseSessionStats`: None when `saved` is not a stats document of version 1. Unknown fields are dropped and
/// malformed counters read as zero. A value that is not an object (`null` included) reads as an empty part.
pub fn parse_session_stats(saved: Option<&Value>) -> Option<Stats> {
    let item = object(saved)?;
    if let Some(version) = item.get("version")
        && version.as_f64() != Some(STATS_VERSION as f64)
    {
        return None;
    }
    let part = |key: &str| item.get(key).filter(|v| !v.is_null());
    let cells = part("cells");
    let cells_of = |name: &str| {
        let c = cells.and_then(|c| c.get(name)).filter(|v| !v.is_null());
        Cells {
            count: count(c.and_then(|c| c.get("count"))),
            failed: count(c.and_then(|c| c.get("failed"))),
            apis: counts(c.and_then(|c| c.get("apis"))),
        }
    };
    let mut guards = IndexMap::new();
    if let Some(map) = object(item.get("guards")) {
        for (name, value) in jsrt::json::object_entries(map) {
            let g = |k: &str| count(Some(value).filter(|v| !v.is_null()).and_then(|v| v.get(k)));
            guards.insert(
                name.clone(),
                Guard {
                    checks: g("checks"),
                    blocked: g("blocked"),
                    unchecked: g("unchecked"),
                    after_checks: g("afterChecks"),
                    after_findings: g("afterFindings"),
                    ms: g("ms"),
                },
            );
        }
    }
    let nudges = part("nudges");
    let nudge = |k: &str| count(nudges.and_then(|n| n.get(k)));
    let turns = part("externalTurns");
    Some(Stats {
        external: item.get("root").and_then(Value::as_str) == Some("external"),
        since: count(item.get("since")),
        updated_at: count(item.get("updatedAt")),
        cells: [cells_of("root"), cells_of("subagents"), cells_of("other")],
        host_calls: counts(item.get("hostCalls")),
        secrets_masked: count(item.get("secretsMasked")),
        guards,
        nudges: [nudge("toolRounds"), nudge("wait"), nudge("skill")],
        usage_limit_blocks: count(item.get("usageLimitBlocks")),
        merges: strings(item.get("merges")),
        child_models: strings(item.get("childModels")),
        external_turns: (
            count(turns.and_then(|t| t.get("count"))),
            count(turns.and_then(|t| t.get("wallMs"))),
        ),
    })
}
