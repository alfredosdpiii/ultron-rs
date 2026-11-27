//! "What did this session do?" (`ultron/session-report.ts`): turns, cells, depth (frames, subagents, workflows,
//! jobs), tokens and cost per lane kind and model, guardrails and memory decisions, from a session file's digest.
//! A number the session did not keep is `null` and named in `unrecorded`; it is never reported as zero.
//!
//! The report is a `serde_json::Value` in the TypeScript field order: `ultron usage --json` prints it and the text
//! renderer reads it.

use crate::log::{EntryKind, LogEntry, LogUsage, SessionLog, log_usage, record};
use crate::stats::{CELL_APIS, Cells, SESSION_STATS_MODULE, Stats, parse_session_stats};
use indexmap::{IndexMap, IndexSet};
use lanes::lane_names::{LaneOwner, child_lane, frame_lane, parse_lane};
use serde_json::{Map, Value, json};
use session_journal::ns;
use std::collections::{HashMap, HashSet};

mod depth;
mod sections;
mod usage_buckets;

pub const SESSION_REPORT_SCHEMA: &str = "ultron.session-report/1";
/// Billed through a subscription login whatever the caller passes (session-report.ts `SUBSCRIPTION_PROVIDERS`).
const SUBSCRIPTION_PROVIDERS: [&str; 1] = [config::providers::CLAUDE_CODE];
const COUNTER_SLACK_MS: f64 = 1000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Lane {
    Root,
    Frames,
    Subagents,
    Other,
}

/// The model row of usage the host's ledger counted beyond every response the session file records (it keeps no
/// model per call): totals only. Durable children's own responses are read from the durable records, with their
/// models; this row is what remains (a session whose durable records are incomplete).
pub const DURABLE_CHILDREN_MODEL: &str = "sub-agents (host ledger)";

fn lane_kind(lane: Option<&str>) -> Lane {
    match lane {
        Some("main") => Lane::Root,
        Some(lane) => match parse_lane(lane) {
            Some(LaneOwner::Frame(_)) => Lane::Frames,
            Some(LaneOwner::Child(_)) => Lane::Subagents,
            None => Lane::Other,
        },
        None => Lane::Other,
    }
}

#[derive(Clone, Debug, Default)]
struct Bucket {
    responses: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    total_tokens: f64,
    reported_usd: Option<f64>,
    subscription_usd: Option<f64>,
    unpriced: f64,
    unmeasured: f64,
}

impl Bucket {
    fn add(&mut self, usage: &LogUsage, subscription: bool) {
        self.responses += 1.0;
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.total_tokens += usage.total_tokens;
        if subscription {
            self.subscription_usd = Some(self.subscription_usd.unwrap_or(0.0) + usage.cost.unwrap_or(0.0));
        } else if let Some(cost) = usage.cost.filter(|c| *c > 0.0) {
            self.reported_usd = Some(self.reported_usd.unwrap_or(0.0) + cost);
        } else if usage.total_tokens > 0.0 {
            self.unpriced += 1.0;
        }
    }

    /// Usage known only as totals (the host's ledger): responses, tokens and cost, with no input/output split.
    fn add_totals(&mut self, responses: f64, tokens: f64, cost: f64, subscription: bool) {
        self.responses += responses;
        self.total_tokens += tokens;
        if cost > 0.0 {
            let slot = if subscription {
                &mut self.subscription_usd
            } else {
                &mut self.reported_usd
            };
            *slot = Some(slot.unwrap_or(0.0) + cost);
        } else if tokens > 0.0 {
            self.unpriced += responses;
        }
    }

    fn fields(&self) -> Map<String, Value> {
        let opt = |v: Option<f64>| v.map_or(Value::Null, jsrt::json::number);
        let Value::Object(map) = json!({
            "responses": jsrt::json::number(self.responses),
            "input": jsrt::json::number(self.input),
            "output": jsrt::json::number(self.output),
            "cacheRead": jsrt::json::number(self.cache_read),
            "cacheWrite": jsrt::json::number(self.cache_write),
            "totalTokens": jsrt::json::number(self.total_tokens),
            "cost": {
                "reportedUsd": opt(self.reported_usd),
                "subscriptionUsd": opt(self.subscription_usd),
                "unpricedResponses": jsrt::json::number(self.unpriced),
            },
            "unmeasured": jsrt::json::number(self.unmeasured),
        }) else {
            unreachable!()
        };
        map
    }
}

struct Buckets<'s> {
    /// Providers whose costs are notional (a subscription login).
    subscription: &'s HashSet<&'s str>,
    lanes: [Bucket; 4],
    models: IndexMap<String, Bucket>,
    by_lane: HashMap<String, Bucket>,
    total: Bucket,
}

fn provider_of(model: &str) -> &str {
    model.split_once('/').map_or(model, |(p, _)| p)
}

impl Buckets<'_> {
    fn index(kind: Lane) -> usize {
        kind as usize
    }

    fn add(&mut self, kind: Lane, model: &str, usage: &LogUsage, lane: Option<&str>) {
        let subscription = self.subscription.contains(provider_of(model));
        self.lanes[Self::index(kind)].add(usage, subscription);
        self.models
            .entry(model.to_string())
            .or_default()
            .add(usage, subscription);
        self.total.add(usage, subscription);
        if let Some(lane) = lane {
            self.by_lane
                .entry(lane.to_string())
                .or_default()
                .add(usage, subscription);
        }
    }

    fn add_totals(&mut self, kind: Lane, model: &str, responses: f64, tokens: f64, cost: f64, subscription: bool) {
        self.lanes[Self::index(kind)].add_totals(responses, tokens, cost, subscription);
        self.models
            .entry(model.to_string())
            .or_default()
            .add_totals(responses, tokens, cost, subscription);
        self.total.add_totals(responses, tokens, cost, subscription);
    }

    fn unmeasured(&mut self, kind: Lane, model: &str) {
        self.lanes[Self::index(kind)].unmeasured += 1.0;
        self.models.entry(model.to_string()).or_default().unmeasured += 1.0;
        self.total.unmeasured += 1.0;
    }
}

fn plural(count: f64, one: &str) -> String {
    format!(
        "{} {}",
        jsrt::number::to_string(count),
        if count == 1.0 {
            one.to_string()
        } else {
            format!("{one}s")
        }
    )
}

/// The one-line answer to "did this session use depth or only the root?".
fn depth_verdict(level: f64, frames: f64, subagents: f64, nested: f64, typed: f64, background: f64) -> String {
    if level == 0.0 {
        return "root only".into();
    }
    let mut parts = vec![plural(frames, "frame")];
    parts.push(format!(
        "{}{}",
        plural(subagents, "sub-agent"),
        if nested > 0.0 {
            format!(" ({} nested)", jsrt::number::to_string(nested))
        } else {
            String::new()
        }
    ));
    if typed > 0.0 {
        parts.push(plural(typed, "typed-agent task"));
    }
    if background > 0.0 {
        parts.push(plural(background, "background job"));
    }
    format!("depth {}: {}", jsrt::number::to_string(level), parts.join(", "))
}

/// Whether nothing ran in the session: no entry, no task, and no runtime value beyond the skill catalog.
pub fn session_is_empty(log: &SessionLog) -> bool {
    log.entries.is_empty()
        && !log.values.keys().any(|address| {
            let (namespace, key) = address.split_once('/').unwrap_or((address, ""));
            namespace.starts_with("ultron.")
                && !(namespace == ns::MODULE && key == "skills")
                && ![ns::SESSION_NAME, ns::LANE_CONFIG, ns::BRANCH_TIP].contains(&namespace)
        })
}

#[derive(Clone, Debug)]
struct Task {
    id: String,
    definition: String,
    state: String,
    parent_id: Option<String>,
    result: Option<Map<String, Value>>,
}

fn definition_name(definition: Option<&Value>) -> String {
    let text = match definition {
        None | Some(Value::Null) => "unknown".to_string(),
        Some(v) => jsrt::value::string(Some(v)),
    };
    if let Some(at) = text.rfind('@') {
        let digits = &text[at + 1..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return text[..at].to_string();
        }
    }
    text
}

fn tasks_of(log: &SessionLog) -> Vec<Task> {
    let list = record(log.value(ns::TASKS, "root"))
        .and_then(|t| t.get("tasks"))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|item| {
            let task = item.as_object()?;
            Some(Task {
                id: task.get("id")?.as_str()?.to_string(),
                definition: definition_name(task.get("definition")),
                state: task
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                parent_id: task.get("parentId").and_then(Value::as_str).map(str::to_string),
                result: record(task.get("result")).cloned(),
            })
        })
        .collect()
}

fn model_name(config: &Value) -> Option<String> {
    let model = record(Some(config))?.get("model")?.as_object()?;
    Some(format!(
        "{}/{}",
        model.get("provider")?.as_str()?,
        model.get("modelId")?.as_str()?
    ))
}

#[derive(Clone, Default)]
struct ModelCount {
    count: f64,
    tokens: f64,
    unmeasured: f64,
}

fn by_model_list(models: IndexMap<String, ModelCount>) -> Value {
    let mut list: Vec<(String, ModelCount)> = models.into_iter().collect();
    list.sort_by(|a, b| {
        b.1.count
            .partial_cmp(&a.1.count)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| jsrt::collate::locale_compare(&a.0, &b.0))
    });
    Value::Array(
        list.into_iter()
            .map(|(model, c)| {
                json!({"model": model, "count": jsrt::json::number(c.count), "tokens": jsrt::json::number(c.tokens), "unmeasured": jsrt::json::number(c.unmeasured)})
            })
            .collect(),
    )
}

fn apis_value(apis: &IndexMap<String, f64>) -> Value {
    let mut ordered = Map::new();
    for api in CELL_APIS {
        if let Some(n) = apis.get(api) {
            ordered.insert(api.to_string(), jsrt::json::number(*n));
        }
    }
    let mut unknown: Vec<&String> = apis.keys().filter(|k| !CELL_APIS.contains(&k.as_str())).collect();
    jsrt::utf16::sort(&mut unknown);
    for api in unknown {
        ordered.insert(api.clone(), jsrt::json::number(apis[api]));
    }
    Value::Object(ordered)
}

fn cells_value(cells: &Cells) -> Value {
    json!({"count": jsrt::json::number(cells.count), "failed": jsrt::json::number(cells.failed), "apis": apis_value(&cells.apis)})
}

fn first_activity_at(log: &SessionLog, usage_roots: &Map<String, Value>) -> f64 {
    let mut first = f64::INFINITY;
    let mut record_at = |at: Option<f64>| {
        if let Some(at) = at
            && at > 0.0
            && at < first
        {
            first = at;
        }
    };
    for entry in &log.entries {
        record_at(Some(entry.timestamp));
    }
    for op in log.operations.values() {
        record_at(Some(op.started_at));
    }
    for root in usage_roots.values() {
        record_at(root.get("startedAt").and_then(Value::as_f64));
    }
    first
}

fn has_external_marks(name: Option<&str>, usage_roots: &Map<String, Value>) -> bool {
    name == Some("claude code")
        || name.is_some_and(|n| n.starts_with("claude subagent ("))
        || usage_roots.keys().any(|id| id.starts_with("turn:cc-"))
}

fn detect_mode(
    log: &SessionLog,
    main_entries: &[&LogEntry],
    stats: Option<&Stats>,
    external_marks: bool,
) -> &'static str {
    if stats.is_some_and(|s| s.external) || (stats.is_none() && main_entries.is_empty() && external_marks) {
        return "ultron claude";
    }
    let has_claude_code_assistant = main_entries
        .iter()
        .any(|e| e.kind == EntryKind::Assistant && e.provider.as_deref() == Some(config::providers::CLAUDE_CODE));
    if !log.values_of("ultron.claude-code.lanes").is_empty() || has_claude_code_assistant {
        "ultron --claude"
    } else {
        "ultron"
    }
}

fn sorted_root_models(root_models: IndexMap<String, f64>) -> Vec<Value> {
    let mut list: Vec<(String, f64)> = root_models.into_iter().collect();
    list.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| jsrt::collate::locale_compare(&a.0, &b.0))
    });
    list.into_iter()
        .map(|(model, responses)| json!({"model": model, "responses": jsrt::json::number(responses)}))
        .collect()
}

fn sorted_model_buckets(models: &IndexMap<String, Bucket>) -> Vec<Value> {
    let mut list: Vec<(String, Bucket)> = models.clone().into_iter().collect();
    list.sort_by(|a, b| {
        b.1.total_tokens
            .partial_cmp(&a.1.total_tokens)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| jsrt::collate::locale_compare(&a.0, &b.0))
    });
    list.into_iter()
        .map(|(model, bucket)| {
            let mut map = Map::new();
            map.insert("model".into(), Value::String(model));
            map.extend(bucket.fields());
            Value::Object(map)
        })
        .collect()
}

/// Build the report from a session file's digest, given the providers whose costs are notional besides Claude Code
/// (`subscriptionProviders`: every provider the session's runtime reaches through a subscription login).
pub fn build_session_report(log: &SessionLog, subscription_providers: &[String]) -> Value {
    let subscription: HashSet<&str> = SUBSCRIPTION_PROVIDERS
        .into_iter()
        .chain(subscription_providers.iter().map(String::as_str))
        .collect();
    let mut unrecorded = Map::new();
    let tasks = tasks_of(log);
    let task_by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let entry_by_id: HashMap<&str, &LogEntry> = log.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let sections::LaneAttribution { lanes, lane_models } = sections::resolve_lanes(log, &entry_by_id);

    let main_entries: Vec<&LogEntry> = log
        .entries
        .iter()
        .zip(&lanes)
        .filter(|(_, lane)| lane.as_deref() == Some("main"))
        .map(|(e, _)| e)
        .collect();
    let main_runs: Vec<_> = log
        .results
        .iter()
        .filter(|r| r.kind == "run" && log.operations.get(&r.operation_id).map(|o| o.lane.as_str()) == Some("main"))
        .collect();
    let name = log.value(ns::SESSION_NAME, "").and_then(Value::as_str);
    let empty_roots = Map::new();
    let ledger = log.usage_ledger();
    let usage_roots = record(ledger.as_ref().and_then(|u| u.get("roots"))).unwrap_or(&empty_roots);
    let external_marks = has_external_marks(name, usage_roots);
    let first_activity = first_activity_at(log, usage_roots);
    let nothing_ran = first_activity == f64::INFINITY && tasks.is_empty() && !external_marks;
    let stats: Option<Stats> = parse_session_stats(log.value(ns::MODULE, SESSION_STATS_MODULE))
        .or_else(|| nothing_ran.then(|| Stats::empty(log.header.created_at)));
    let mode = detect_mode(log, &main_entries, stats.as_ref(), external_marks);
    let external = mode == "ultron claude";
    let stats_cover = stats
        .as_ref()
        .is_some_and(|s| s.since <= first_activity + COUNTER_SLACK_MS);

    let turns = sections::build_turns(log, &main_runs, external, stats.as_ref(), &mut unrecorded);
    let usage_buckets::CollectedUsage {
        buckets,
        root_models,
        child_models,
        process_children,
    } = usage_buckets::collect_usage(
        log,
        &lanes,
        &entry_by_id,
        &lane_models,
        &tasks,
        usage_roots,
        external,
        stats.as_ref(),
        &subscription,
    );
    let sections::CellsSection {
        cells,
        redactions,
        nudges,
    } = sections::build_cells(log, &lanes, external, stats.as_ref(), &mut unrecorded);
    let depth = depth::build_depth(
        log,
        &tasks,
        &task_by_id,
        &lane_models,
        &buckets,
        &child_models,
        &process_children,
        stats.as_ref(),
        stats_cover,
        &mut unrecorded,
    );
    let guardrails = sections::build_guardrails(
        log,
        external,
        stats.as_ref(),
        stats_cover,
        redactions,
        &nudges,
        &mut unrecorded,
    );
    let memory = sections::build_memory(log, &mut unrecorded);
    if external {
        unrecorded.insert(
            "root.models".into(),
            "Claude Code owns the root conversation: its model, tokens and cost are Claude Code's".into(),
        );
    }

    json!({
        "schema": SESSION_REPORT_SCHEMA,
        "session": {
            "id": log.header.id,
            "path": log.path.display().to_string(),
            "cwd": log.header.cwd,
            "name": name,
            "createdAt": jsrt::json::number(log.header.created_at),
            "modifiedAt": jsrt::json::number(log.modified_at),
            "bytes": jsrt::json::number(log.bytes as f64),
            "parentSessionId": log.header.parent_session_id,
        },
        "mode": mode,
        "turns": turns,
        "root": {
            "models": sorted_root_models(root_models),
        },
        "cells": cells,
        "depth": depth,
        "usage": {
            "lanes": {
                "root": buckets.lanes[0].fields(),
                "frames": buckets.lanes[1].fields(),
                "subagents": buckets.lanes[2].fields(),
                "other": buckets.lanes[3].fields(),
            },
            "models": sorted_model_buckets(&buckets.models),
            "total": buckets.total.fields(),
        },
        "guardrails": guardrails,
        "memory": memory,
        "unrecorded": unrecorded,
    })
}

fn counts_value(counts: &IndexMap<String, f64>) -> Value {
    Value::Object(
        counts
            .iter()
            .map(|(k, v)| (k.clone(), jsrt::json::number(*v)))
            .collect(),
    )
}
