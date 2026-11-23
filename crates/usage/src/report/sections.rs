use super::*;

pub(super) struct LaneAttribution {
    pub(super) lanes: Vec<Option<String>>,
    pub(super) lane_models: HashMap<String, String>,
}

pub(super) fn resolve_lanes(log: &SessionLog, entry_by_id: &HashMap<&str, &LogEntry>) -> LaneAttribution {
    let mut root_ids: HashMap<String, String> = HashMap::new();
    let tree_root = |id: &str, root_ids: &mut HashMap<String, String>| -> String {
        let mut path: IndexSet<String> = IndexSet::new();
        let mut current = Some(id.to_string());
        let mut root = id.to_string();
        while let Some(c) = current {
            if let Some(known) = root_ids.get(&c) {
                root = known.clone();
                break;
            }
            path.insert(c.clone());
            root = c.clone();
            let parent = entry_by_id.get(c.as_str()).and_then(|e| e.parent_id.clone());
            current = parent.filter(|p| !path.contains(p));
        }
        for visited in path {
            root_ids.insert(visited, root.clone());
        }
        root
    };
    let mut tree_lane: HashMap<String, String> = HashMap::new();
    let mut claim = |entry_id: Option<&str>, lane: &str, root_ids: &mut HashMap<String, String>| {
        let Some(entry_id) = entry_id.filter(|id| entry_by_id.contains_key(id)) else {
            return;
        };
        let root = tree_root(entry_id, root_ids);
        if !tree_lane.contains_key(&root) || lane == "main" {
            tree_lane.insert(root, lane.to_string());
        }
    };
    for (lane, tip) in log.values_of(ns::BRANCH_TIP) {
        claim(tip.as_str(), lane, &mut root_ids);
    }
    for result in &log.results {
        let Some(op) = log.operations.get(&result.operation_id) else {
            continue;
        };
        claim(result.tip_id.as_deref(), &op.lane, &mut root_ids);
        claim(result.from_tip_id.as_deref(), &op.lane, &mut root_ids);
    }
    let lanes: Vec<Option<String>> = log
        .entries
        .iter()
        .map(|entry| tree_lane.get(&tree_root(&entry.id, &mut root_ids)).cloned())
        .collect();
    let mut lane_models: HashMap<String, String> = HashMap::new();
    for (lane, config) in log.values_of(ns::LANE_CONFIG) {
        if let Some(model) = model_name(config) {
            lane_models.insert(lane.to_string(), model);
        }
    }
    LaneAttribution { lanes, lane_models }
}

pub(super) fn build_turns(
    log: &SessionLog,
    main_runs: &[&crate::log::LogResult],
    external: bool,
    stats: Option<&Stats>,
    unrecorded: &mut Map<String, Value>,
) -> Value {
    if external {
        match stats {
            None => {
                unrecorded.insert(
                    "turns".into(),
                    "Claude Code owns the root conversation; this session predates Ultron's turn count".into(),
                );
                Value::Null
            }
            Some(s) => json!({
                "count": jsrt::json::number(s.external_turns.0),
                "completed": jsrt::json::number(s.external_turns.0),
                "aborted": 0,
                "failed": 0,
                "running": 0,
                "wallMs": jsrt::json::number(s.external_turns.1),
            }),
        }
    } else {
        let ended: HashSet<&str> = log.results.iter().map(|r| r.operation_id.as_str()).collect();
        let running = log
            .operations
            .values()
            .filter(|o| o.lane == "main" && o.kind == "run" && !ended.contains(o.id.as_str()))
            .count() as f64;
        let with = |f: &dyn Fn(&str) -> bool| main_runs.iter().filter(|r| f(&r.status)).count() as f64;
        json!({
            "count": jsrt::json::number(main_runs.len() as f64 + running),
            "completed": jsrt::json::number(with(&|s| s == "completed")),
            "aborted": jsrt::json::number(with(&|s| s == "aborted")),
            "failed": jsrt::json::number(with(&|s| s == "failed" || s == "declined")),
            "running": jsrt::json::number(running),
            "wallMs": jsrt::json::number(main_runs.iter().fold(0.0, |t, r| t + (r.ended_at - r.started_at).max(0.0))),
        })
    }
}

pub(super) struct CellsSection {
    pub(super) cells: Value,
    pub(super) redactions: f64,
    pub(super) nudges: [f64; 3],
}

pub(super) fn build_cells(
    log: &SessionLog,
    lanes: &[Option<String>],
    external: bool,
    stats: Option<&Stats>,
    unrecorded: &mut Map<String, Value>,
) -> CellsSection {
    let mut transcript: [Cells; 3] = Default::default();
    let mut other_tools: IndexMap<String, (f64, f64)> = IndexMap::new();
    let mut redactions = 0.0;
    let mut nudges = [0.0; 3];
    for (entry, lane) in log.entries.iter().zip(lanes) {
        let kind = match lane_kind(lane.as_deref()) {
            Lane::Root => 0,
            Lane::Subagents => 1,
            Lane::Frames | Lane::Other => 2,
        };
        match entry.kind {
            EntryKind::Assistant => {
                for call in &entry.tool_calls {
                    if call.name != "rlm" {
                        other_tools.entry(call.name.clone()).or_default().0 += 1.0;
                        continue;
                    }
                    transcript[kind].count += 1.0;
                    record_call_apis(&mut transcript[kind], call.apis.as_deref().unwrap_or_default());
                }
            }
            EntryKind::ToolResult => {
                redactions += entry.redactions;
                if !entry.is_error {
                    continue;
                }
                match entry.tool_name.as_deref() {
                    Some("rlm") => transcript[kind].failed += 1.0,
                    Some(name) => other_tools.entry(name.to_string()).or_default().1 += 1.0,
                    None => {}
                }
            }
            EntryKind::User => {
                if let Some(nudge) = entry.nudge {
                    nudges[["toolRounds", "wait", "skill"]
                        .iter()
                        .position(|n| *n == nudge)
                        .unwrap_or(0)] += 1.0;
                }
            }
            _ => {}
        }
    }
    let other_tools_value = Value::Object(
        other_tools
            .iter()
            .map(|(name, (calls, failed))| {
                (
                    name.clone(),
                    json!({"calls": jsrt::json::number(*calls), "failed": jsrt::json::number(*failed)}),
                )
            })
            .collect(),
    );
    let cells_from = |source: &str, by: &[Cells; 3]| -> Value {
        let mut total = Cells::default();
        for part in by {
            total.count += part.count;
            total.failed += part.failed;
            for (api, n) in &part.apis {
                *total.apis.entry(api.clone()).or_default() += n;
            }
        }
        json!({
            "source": source,
            "total": cells_value(&total),
            "root": cells_value(&by[0]),
            "subagents": cells_value(&by[1]),
            "other": cells_value(&by[2]),
            "otherTools": other_tools_value,
        })
    };
    let cells = if !external {
        cells_from("transcript", &transcript)
    } else if let Some(s) = stats {
        cells_from("counters", &s.cells)
    } else {
        unrecorded.insert(
            "cells".into(),
            "Claude Code owns the transcript; this session predates Ultron's cell count".into(),
        );
        Value::Null
    };
    CellsSection {
        cells,
        redactions,
        nudges,
    }
}

fn record_call_apis(cells: &mut Cells, apis: &[&str]) {
    for api in apis {
        *cells.apis.entry((*api).to_string()).or_default() += 1.0;
    }
}

fn collect_hint_counts(log: &SessionLog) -> IndexMap<String, f64> {
    let mut hints: IndexMap<String, f64> = IndexMap::new();
    let Some(lanes) = record(record(log.value(ns::MODULE, "hints")).and_then(|h| h.get("lanes"))) else {
        return hints;
    };
    for fired in lanes.values().filter_map(|lane| record(lane.get("fired"))) {
        for (tag, n) in fired {
            if let Some(n) = n.as_f64().filter(|n| *n > 0.0) {
                *hints.entry(tag.clone()).or_default() += n;
            }
        }
    }
    hints
}

pub(super) fn build_guardrails(
    log: &SessionLog,
    external: bool,
    stats: Option<&Stats>,
    stats_cover: bool,
    redactions: f64,
    nudges: &[f64; 3],
    unrecorded: &mut Map<String, Value>,
) -> Value {
    let hints = collect_hint_counts(log);
    if stats.is_none() {
        unrecorded.insert(
            "guardrails.guards".into(),
            "kept only in the worker's memory when this session ran".into(),
        );
        unrecorded.insert(
            "guardrails.usageLimitBlocks".into(),
            "this session predates the count".into(),
        );
    }
    let nudges_value = |n: &[f64; 3]| json!({"toolRounds": jsrt::json::number(n[0]), "wait": jsrt::json::number(n[1]), "skill": jsrt::json::number(n[2])});
    let (secrets_masked, reported_nudges) = if !external {
        (jsrt::json::number(redactions), nudges_value(nudges))
    } else if let Some(s) = stats {
        (jsrt::json::number(s.secrets_masked), nudges_value(&s.nudges))
    } else {
        unrecorded.insert(
            "guardrails.secretsMasked".into(),
            "cell output went to Claude Code; this session predates the count".into(),
        );
        unrecorded.insert(
            "guardrails.nudges".into(),
            "steers went to Claude Code with a cell result; this session predates the count".into(),
        );
        (Value::Null, Value::Null)
    };
    let guards = stats.map_or(Value::Null, |s| {
        Value::Object(
            s.guards
                .iter()
                .map(|(name, g)| {
                    (
                        name.clone(),
                        json!({
                            "checks": jsrt::json::number(g.checks),
                            "blocked": jsrt::json::number(g.blocked),
                            "unchecked": jsrt::json::number(g.unchecked),
                            "afterChecks": jsrt::json::number(g.after_checks),
                            "afterFindings": jsrt::json::number(g.after_findings),
                            "ms": jsrt::json::number(g.ms),
                        }),
                    )
                })
                .collect(),
        )
    });
    json!({
        "guards": guards,
        "secretsMasked": secrets_masked,
        "hints": counts_value(&hints),
        "nudges": reported_nudges,
        "usageLimitBlocks": stats.map_or(Value::Null, |s| jsrt::json::number(s.usage_limit_blocks)),
        "countersSince": match stats {
            Some(s) if !stats_cover => jsrt::json::number(s.since),
            _ => Value::Null,
        },
    })
}

pub(super) fn build_memory(log: &SessionLog, unrecorded: &mut Map<String, Value>) -> Value {
    let memory = record(log.value(ns::MEMORY_STATE, "root"))
        .and_then(|m| m.get("operations"))
        .and_then(Value::as_array)
        .filter(|ops| !ops.is_empty());
    let recalls: Vec<_> = log.entries.iter().filter_map(|entry| entry.recall).collect();
    let recall = (!recalls.is_empty()).then(|| {
        json!({
            "messages": recalls.len(),
            "memories": recalls.iter().map(|r| r.memories).sum::<usize>(),
            "briefs": recalls.iter().filter(|r| r.brief).count(),
            "stale": recalls.iter().map(|r| r.stale).sum::<usize>(),
        })
    });
    let operations = match memory {
        Some(ops) => {
            let mut counted: IndexMap<String, f64> = IndexMap::new();
            for op in ops {
                let field = |k: &str| match op.as_object().and_then(|o| o.get(k)) {
                    None | Some(Value::Null) => "unknown".to_string(),
                    Some(v) => jsrt::value::string(Some(v)),
                };
                *counted
                    .entry(format!("{}.{}", field("kind"), field("state")))
                    .or_default() += 1.0;
            }
            counts_value(&counted)
        }
        None => {
            unrecorded.insert(
                "memory.operations".into(),
                if recall.is_some() {
                    "the model made no memory call; automatic recall is below".into()
                } else {
                    "no memory operation recorded: Hindsight not configured, or memory unused".into()
                },
            );
            Value::Null
        }
    };
    match recall {
        Some(recall) => json!({"operations": operations, "recall": recall}),
        None => json!({"operations": operations}),
    }
}
