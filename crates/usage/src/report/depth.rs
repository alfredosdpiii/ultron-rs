use super::*;

fn frame_level(
    trace: &Map<String, Value>,
    seen: &mut HashSet<String>,
    levels: &mut HashMap<String, f64>,
    traces: &IndexMap<String, &Map<String, Value>>,
    level_of_task: &dyn Fn(Option<&str>) -> f64,
) -> f64 {
    let id = trace.get("id").map_or_default(|v| jsrt::value::string(Some(v)));
    if let Some(known) = levels.get(&id) {
        return *known;
    }
    seen.insert(id.clone());
    let parent = trace
        .get("parentFrame")
        .and_then(Value::as_str)
        .filter(|p| !seen.contains(*p))
        .and_then(|p| traces.get(p).copied());
    let level = match parent {
        Some(parent) => frame_level(parent, seen, levels, traces, level_of_task) + 1.0,
        None => level_of_task(trace.get("callerTaskId").and_then(Value::as_str)) + 1.0,
    };
    levels.insert(id, level);
    level
}

fn child_depth(task: &Task, task_by_id: &HashMap<&str, &Task>) -> f64 {
    let mut depth = 0.0;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut current = Some(task);
    while let Some(c) = current {
        if !seen.insert(c.id.as_str()) {
            break;
        }
        if c.definition == "rlm-child" {
            depth += 1.0;
        }
        current = c.parent_id.as_deref().and_then(|p| task_by_id.get(p).copied());
    }
    depth
}

fn task_level(id: Option<&str>, task_by_id: &HashMap<&str, &Task>) -> f64 {
    id.and_then(|i| task_by_id.get(i))
        .map_or(0.0, |t| child_depth(t, task_by_id))
}

fn count_states(items: &[&Task], states: &[&str]) -> f64 {
    items.iter().filter(|t| states.contains(&t.state.as_str())).count() as f64
}

struct ChildrenSummary {
    verdicts: [f64; 6],
    claims: [f64; 3],
    worktrees: Vec<Value>,
    by_model: IndexMap<String, ModelCount>,
    max_depth: f64,
    nested: f64,
}

fn record_child_verdict(result: &Map<String, Value>, verdicts: &mut [f64; 6], claims: &mut [f64; 3]) {
    let verdict = record(result.get("verdict"));
    let outcome = record(result.get("check"))
        .and_then(|c| c.get("outcome"))
        .and_then(Value::as_str);
    match (verdict, outcome) {
        (None, Some("invalid")) => verdicts[3] += 1.0,
        (None, _) => verdicts[5] += 1.0,
        (Some(_), Some("verified")) => verdicts[0] += 1.0,
        (Some(_), Some("contradicted")) => verdicts[1] += 1.0,
        (Some(_), _) => verdicts[4] += 1.0,
    }
    match verdict.and_then(|v| v.get("status")).and_then(Value::as_str) {
        Some("passed") => claims[0] += 1.0,
        Some("failed") => claims[1] += 1.0,
        Some("blocked") => claims[2] += 1.0,
        _ => {}
    }
}

fn child_worktree_entry(child_id: &str, result: &Map<String, Value>, stats: Option<&Stats>) -> Option<Value> {
    let w = record(result.get("worktree"))?;
    let branch = w.get("branch").and_then(Value::as_str)?;
    let merge = stats.and_then(|s| s.merges.get(child_id).cloned()).map_or_else(
        || {
            if w.get("commit") == Some(&Value::Null) {
                Value::String("empty".into())
            } else {
                Value::Null
            }
        },
        Value::String,
    );
    let changed_files = w.get("changed_files").and_then(Value::as_array).map_or(0, Vec::len) as f64;
    Some(json!({
        "task": child_id,
        "branch": branch,
        "changedFiles": jsrt::json::number(changed_files),
        "merge": merge,
    }))
}

fn durable_models_by_lane(log: &SessionLog) -> HashMap<&str, &str> {
    let mut durable_models: HashMap<&str, &str> = HashMap::new();
    for response in &log.durable_responses {
        if let Some(lane) = &response.lane {
            durable_models.entry(lane.as_str()).or_insert(response.model.as_str());
        }
    }
    durable_models
}

fn resolve_child_model(
    child: &Task,
    lane: &str,
    lane_models: &HashMap<String, String>,
    durable_models: &HashMap<&str, &str>,
    child_models: &HashMap<String, String>,
    stats: Option<&Stats>,
) -> String {
    lane_models
        .get(lane)
        .cloned()
        .or_else(|| durable_models.get(lane).map(std::string::ToString::to_string))
        .or_else(|| child_models.get(child.id.as_str()).cloned())
        .or_else(|| stats.and_then(|s| s.child_models.get(&child.id).cloned()))
        .unwrap_or_else(|| "not recorded".into())
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal report-stage helper taking disjoint precomputed tables"
)]
fn summarize_children(
    log: &SessionLog,
    children: &[&Task],
    task_by_id: &HashMap<&str, &Task>,
    lane_models: &HashMap<String, String>,
    buckets: &Buckets<'_>,
    child_models: &HashMap<String, String>,
    process_children: &HashSet<String>,
    stats: Option<&Stats>,
) -> ChildrenSummary {
    let mut verdicts = [0.0; 6];
    let mut claims = [0.0; 3];
    let mut worktrees = Vec::new();
    let mut by_model: IndexMap<String, ModelCount> = IndexMap::new();
    let durable_models = durable_models_by_lane(log);
    for child in children {
        let lane = child_lane(&child.id);
        let model = resolve_child_model(child, &lane, lane_models, &durable_models, child_models, stats);
        let tokens = buckets.by_lane.get(&lane).map(|b| b.total_tokens);
        let entry = by_model.entry(model).or_default();
        entry.count += 1.0;
        entry.tokens += tokens.unwrap_or(0.0);
        if tokens.is_none() && process_children.contains(child.id.as_str()) && child.result.is_some() {
            entry.unmeasured += 1.0;
        }
        let Some(result) = &child.result else { continue };
        record_child_verdict(result, &mut verdicts, &mut claims);
        if let Some(worktree) = child_worktree_entry(&child.id, result, stats) {
            worktrees.push(worktree);
        }
    }
    verdicts[2] = verdicts[3] + verdicts[4] + verdicts[5];
    let depths: Vec<f64> = children.iter().map(|c| child_depth(c, task_by_id)).collect();
    ChildrenSummary {
        verdicts,
        claims,
        worktrees,
        by_model,
        max_depth: depths.iter().fold(0.0_f64, |m, d| m.max(*d)),
        nested: depths.iter().filter(|d| **d > 1.0).count() as f64,
    }
}

struct FramesSummary {
    counts: [f64; 7],
    incomplete_reasons: IndexMap<String, f64>,
    budget_tokens: f64,
    by_model: IndexMap<String, ModelCount>,
    max_level: f64,
}

fn collect_frame_traces(log: &SessionLog) -> IndexMap<String, &Map<String, Value>> {
    let mut trace_by_id = IndexMap::new();
    for (key, value) in log.values_of(ns::RLM_FRAMES) {
        if key == "index" {
            continue;
        }
        if let Some(trace) = value.as_object()
            && let Some(id) = trace.get("id").and_then(Value::as_str)
        {
            trace_by_id.insert(id.to_string(), trace);
        }
    }
    trace_by_id
}

fn count_frame_model(
    model: String,
    lane: Option<&str>,
    buckets: &Buckets<'_>,
    by_model: &mut IndexMap<String, ModelCount>,
) {
    let tokens = lane.and_then(|l| buckets.by_lane.get(l)).map(|b| b.total_tokens);
    let entry = by_model.entry(model).or_default();
    entry.count += 1.0;
    entry.tokens += tokens.unwrap_or(0.0);
}

fn record_trace_status(trace: &Map<String, Value>, summary: &mut FramesSummary) {
    match trace.get("status").and_then(Value::as_str) {
        Some("complete") => summary.counts[1] += 1.0,
        Some("incomplete") => {
            summary.counts[2] += 1.0;
            let reason = trace.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            *summary.incomplete_reasons.entry(reason.to_string()).or_default() += 1.0;
        }
        Some("error") => summary.counts[3] += 1.0,
        _ => summary.counts[5] += 1.0,
    }
    if let Some(spent) = record(trace.get("spent"))
        .and_then(|s| s.get("tokens"))
        .and_then(Value::as_f64)
        && spent.is_finite()
    {
        summary.budget_tokens += spent;
    }
}

fn trace_lane(trace: &Map<String, Value>) -> Option<String> {
    if let Some(lane) = trace.get("lane").and_then(Value::as_str) {
        return Some(lane.to_string());
    }
    trace.get("taskId").and_then(Value::as_str).map(frame_lane)
}

fn summarize_frames(
    log: &SessionLog,
    tasks: &[Task],
    task_by_id: &HashMap<&str, &Task>,
    lane_models: &HashMap<String, String>,
    buckets: &Buckets<'_>,
) -> FramesSummary {
    let trace_by_id = collect_frame_traces(log);
    let level_of_task = |id: Option<&str>| task_level(id, task_by_id);
    let mut traced: HashSet<&str> = HashSet::new();
    let mut frame_levels: HashMap<String, f64> = HashMap::new();
    let mut summary = FramesSummary {
        counts: [0.0; 7],
        incomplete_reasons: IndexMap::new(),
        budget_tokens: 0.0,
        by_model: IndexMap::new(),
        max_level: 0.0,
    };
    for trace in trace_by_id.values() {
        summary.counts[0] += 1.0;
        if let Some(task_id) = trace.get("taskId").and_then(Value::as_str) {
            traced.insert(task_id);
        }
        record_trace_status(trace, &mut summary);
        let level = frame_level(
            trace,
            &mut HashSet::new(),
            &mut frame_levels,
            &trace_by_id,
            &level_of_task,
        );
        if level > 1.0 {
            summary.counts[6] += 1.0;
        }
        summary.max_level = summary.max_level.max(level);
        let lane = trace_lane(trace);
        let model = trace
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| lane.as_deref().and_then(|l| lane_models.get(l).cloned()))
            .unwrap_or_else(|| "not recorded".into());
        count_frame_model(model, lane.as_deref(), buckets, &mut summary.by_model);
    }
    accumulate_untraced_frame_tasks(tasks, &traced, task_by_id, lane_models, buckets, &mut summary);
    summary
}

fn accumulate_untraced_frame_tasks(
    tasks: &[Task],
    traced: &HashSet<&str>,
    task_by_id: &HashMap<&str, &Task>,
    lane_models: &HashMap<String, String>,
    buckets: &Buckets<'_>,
    summary: &mut FramesSummary,
) {
    for task in tasks
        .iter()
        .filter(|t| t.definition == "rlm-frame" && !traced.contains(t.id.as_str()))
    {
        summary.counts[0] += 1.0;
        match task.state.as_str() {
            "completed" => summary.counts[1] += 1.0,
            "cancelled" | "interrupted" => summary.counts[4] += 1.0,
            "failed" => summary.counts[3] += 1.0,
            _ => summary.counts[5] += 1.0,
        }
        let level = task_level(task.parent_id.as_deref(), task_by_id) + 1.0;
        if level > 1.0 {
            summary.counts[6] += 1.0;
        }
        summary.max_level = summary.max_level.max(level);
        let lane = frame_lane(&task.id);
        let model = lane_models
            .get(lane.as_str())
            .cloned()
            .unwrap_or_else(|| "not recorded".into());
        count_frame_model(model, Some(&lane), buckets, &mut summary.by_model);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal report-stage helper taking disjoint precomputed tables"
)]
pub(super) fn build_depth(
    log: &SessionLog,
    tasks: &[Task],
    task_by_id: &HashMap<&str, &Task>,
    lane_models: &HashMap<String, String>,
    buckets: &Buckets<'_>,
    child_models: &HashMap<String, String>,
    process_children: &HashSet<String>,
    stats: Option<&Stats>,
    stats_cover: bool,
    unrecorded: &mut Map<String, Value>,
) -> Value {
    let children: Vec<&Task> = tasks.iter().filter(|t| t.definition == "rlm-child").collect();
    let child_summary = summarize_children(
        log,
        &children,
        task_by_id,
        lane_models,
        buckets,
        child_models,
        process_children,
        stats,
    );
    let frame_summary = summarize_frames(log, tasks, task_by_id, lane_models, buckets);

    let typed: Vec<&Task> = tasks
        .iter()
        .filter(|t| !["rlm-child", "rlm-frame", "background-job"].contains(&t.definition.as_str()))
        .collect();
    let mut by_definition: IndexMap<String, f64> = IndexMap::new();
    for task in &typed {
        *by_definition.entry(task.definition.clone()).or_default() += 1.0;
    }
    let background: Vec<&Task> = tasks.iter().filter(|t| t.definition == "background-job").collect();
    let other_level = typed.iter().chain(&background).fold(0.0_f64, |m, t| {
        m.max(task_level(t.parent_id.as_deref(), task_by_id) + 1.0)
    });
    let level = child_summary.max_depth.max(frame_summary.max_level).max(other_level);
    if !stats_cover {
        let reason = if stats.is_none() {
            "this session predates Ultron's runtime counters"
        } else {
            "the runtime counters began after this session did"
        };
        unrecorded.insert("depth.workflows".into(), reason.into());
        unrecorded.insert("depth.frames.calls".into(), reason.into());
    }
    let host_call = |name: &str| jsrt::json::number(stats.and_then(|s| s.host_calls.get(name).copied()).unwrap_or(0.0));
    let frames_value = json!({
        "count": jsrt::json::number(frame_summary.counts[0]),
        "complete": jsrt::json::number(frame_summary.counts[1]),
        "incomplete": jsrt::json::number(frame_summary.counts[2]),
        "failed": jsrt::json::number(frame_summary.counts[3]),
        "cancelled": jsrt::json::number(frame_summary.counts[4]),
        "running": jsrt::json::number(frame_summary.counts[5]),
        "nested": jsrt::json::number(frame_summary.counts[6]),
        "incompleteReasons": counts_value(&frame_summary.incomplete_reasons),
        "budgetTokens": jsrt::json::number(frame_summary.budget_tokens),
        "byModel": by_model_list(frame_summary.by_model),
        "calls": if stats_cover {
            json!({"infer": host_call("rlm.infer"), "map": host_call("rlm.map")})
        } else {
            Value::Null
        },
    });
    json!({
        "verdict": depth_verdict(
            level,
            frame_summary.counts[0],
            children.len() as f64,
            child_summary.nested,
            typed.len() as f64,
            background.len() as f64,
        ),
        "level": jsrt::json::number(level),
        "frames": frames_value,
        "subagents": {
            "count": jsrt::json::number(children.len() as f64),
            "maxDepth": jsrt::json::number(child_summary.max_depth),
            "nested": jsrt::json::number(child_summary.nested),
            "completed": jsrt::json::number(count_states(&children, &["completed"])),
            "failed": jsrt::json::number(count_states(&children, &["failed"])),
            "cancelled": jsrt::json::number(count_states(&children, &["cancelled"])),
            "interrupted": jsrt::json::number(count_states(&children, &["interrupted"])),
            "running": jsrt::json::number(count_states(&children, &["admitted", "running"])),
            "byModel": by_model_list(child_summary.by_model),
            "verdicts": {
                "verified": jsrt::json::number(child_summary.verdicts[0]),
                "contradicted": jsrt::json::number(child_summary.verdicts[1]),
                "unverified": jsrt::json::number(child_summary.verdicts[2]),
                "invalid": jsrt::json::number(child_summary.verdicts[3]),
                "unchecked": jsrt::json::number(child_summary.verdicts[4]),
                "none": jsrt::json::number(child_summary.verdicts[5]),
            },
            "claims": {
                "passed": jsrt::json::number(child_summary.claims[0]),
                "failed": jsrt::json::number(child_summary.claims[1]),
                "blocked": jsrt::json::number(child_summary.claims[2]),
            },
            "worktrees": child_summary.worktrees,
        },
        "workflows": if stats_cover { json!({"runs": host_call("workflows.run")}) } else { Value::Null },
        "typedAgents": {"count": jsrt::json::number(typed.len() as f64), "byDefinition": counts_value(&by_definition)},
        "backgroundJobs": {
            "count": jsrt::json::number(background.len() as f64),
            "completed": jsrt::json::number(count_states(&background, &["completed"])),
            "failed": jsrt::json::number(count_states(&background, &["failed"])),
            "cancelled": jsrt::json::number(count_states(&background, &["cancelled", "interrupted"])),
            "running": jsrt::json::number(count_states(&background, &["admitted", "running"])),
        },
    })
}
